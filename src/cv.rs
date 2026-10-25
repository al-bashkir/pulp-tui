//! Content Views: an application-managed layer (inspired by Katello, NOT a Pulp API).
//!
//! * Publish: for every repository in the view, the selected content of the source's
//!   latest version is copied into an application-owned *derived* repository, giving a
//!   new immutable repository version, which is then published. The derived repository
//!   has no retention limit, so source-side pruning cannot remove CV snapshots.
//! * Promote/rollback: per environment and repository a distribution with a stable base
//!   path (`<env label>/<cv>/<repo>`) is re-pointed to the version's publication.
//!   History is never deleted to roll back.
//!
//! Pulp has no cross-resource transactions. Each step is idempotent (find-or-create by
//! name, publication reuse) and checkpointed in the store, including the href of a
//! submitted task, so an interrupted publish resumes by re-attaching to that task.
//! Partial promotions are recorded as `partial` and can be retried or reverted.

use crate::filters::Selector;
use crate::pulp::models::{DebPackage, Distribution, Package, RepoVersion, Repository, Task};
use crate::pulp::ops::{self, OnTask};
use crate::pulp::{Mutation, PulpClient, PulpError, RepoType, deb, file, rpm};
use crate::store::{
    BindingRepo, Component, ContentView, CvRepo, CvVersion, CvVersionRepo, Environment, LIBRARY, LOCK_RENEW_SECS, LOCK_TTL_SECS,
    LockHolder, Store, UsedVersion, validate_label,
};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub type Log<'a> = &'a (dyn Fn(String) + Send + Sync);

pub const LABEL_CV: &str = "pulp_tui_cv";
pub const LABEL_ENV: &str = "pulp_tui_env";
/// Id of the pulp-tui database that created or adopted the object (`Store::db_id`).
pub const LABEL_DB: &str = "pulp_tui_db";

#[derive(Clone)]
pub struct CvService {
    pub client: PulpClient,
    pub store: Arc<Store>,
    /// Store scope of this server (`Store::scope`).
    pub scope: String,
    /// `store::actor`: who is shown in the audit log.
    pub actor: String,
    /// Held by workflows started through `locked`; they stop at the next step once it is lost.
    pub lock: Option<Arc<CvLock>>,
}

/// One operator's lock on one Content View, renewed in the background while held.
/// Dropping it (workflow end, Esc) releases it.
#[derive(Debug)]
pub struct CvLock {
    store: Arc<Store>,
    cv_id: i64,
    name: String,
    token: String,
    /// Why the lock was lost; set by the renewal task, read by `check` (no database round trip).
    lost: Arc<Mutex<Option<String>>>,
    /// Wall-clock unix seconds of the acquire and of the last successful renewal. Tokio timers stop
    /// during suspend, so `check` also fails when this is too old, even if no renewal round has run.
    ok_at: Arc<AtomicI64>,
    renew: tokio::task::AbortHandle,
}

pub fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn conflict(name: &str, h: &LockHolder) -> String {
    let since = h.acquired_at.get(11..16).unwrap_or(&h.acquired_at);
    format!("Content View {name} is locked by {} ({}) since {since} UTC", h.owner, h.operation)
}

/// One renewal round; `false` (lock lost, reason recorded) stops the renewal task.
async fn renew_step(store: &Store, cv_id: i64, token: &str, lost: &Mutex<Option<String>>, ok_at: &AtomicI64) -> bool {
    let now = unix_now();
    let reason = match tokio::time::timeout(Duration::from_secs(LOCK_RENEW_SECS), store.renew_lock(cv_id, token, now)).await {
        Ok(Ok(true)) => {
            ok_at.store(now, Ordering::Relaxed);
            return true;
        }
        Ok(Ok(false)) => "taken over or broken by another operator".to_string(),
        Ok(Err(e)) => format!("database unreachable: {e}"),
        Err(_) => "database unreachable: renewal timed out".to_string(),
    };
    *lost.lock().unwrap() = Some(reason);
    false
}

impl CvLock {
    pub async fn acquire(store: Arc<Store>, cv_id: i64, name: &str, owner: &str, operation: &str) -> Result<Arc<CvLock>> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let token = format!("{}-{nanos}", std::process::id());
        let now = unix_now();
        if let Err(h) = store.try_lock(cv_id, owner, &token, operation, now).await? {
            bail!("{}", conflict(name, &h));
        }
        let (lost, ok_at) = (Arc::new(Mutex::new(None)), Arc::new(AtomicI64::new(now)));
        let (st, tok, l, ok) = (store.clone(), token.clone(), lost.clone(), ok_at.clone());
        let renew = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(LOCK_RENEW_SECS)).await;
                if !renew_step(&st, cv_id, &tok, &l, &ok).await {
                    break;
                }
            }
        })
        .abort_handle();
        Ok(Arc::new(CvLock { store, cv_id, name: name.into(), token, lost, ok_at, renew }))
    }

    pub fn check(&self) -> Result<()> {
        if let Some(reason) = &*self.lost.lock().unwrap() {
            bail!("lock on {} lost: {reason}", self.name);
        }
        // Past this age the row can expire (TTL) before the next renewal round would notice.
        let n = unix_now() - self.ok_at.load(Ordering::Relaxed);
        if n > LOCK_TTL_SECS - LOCK_RENEW_SECS as i64 {
            bail!("lock on {} lost: not renewed for {n} s", self.name);
        }
        Ok(())
    }

    #[cfg(test)]
    async fn renew_now(&self) {
        renew_step(&self.store, self.cv_id, &self.token, &self.lost, &self.ok_at).await;
    }
}

impl Drop for CvLock {
    fn drop(&mut self) {
        self.renew.abort();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let (store, cv_id, token) = (self.store.clone(), self.cv_id, std::mem::take(&mut self.token));
            rt.spawn(async move {
                if let Err(e) = store.release_lock(cv_id, &token).await {
                    tracing::warn!("releasing the lock of content view {cv_id} failed (it expires): {e:#}");
                }
            });
        }
    }
}

/// Turn any name into a safe Pulp name / base path segment.
pub fn slug(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '-' }).collect()
}

/// Repository fields that pulp_rpm uses at publish time when the publication does not set them.
const RPM_PUBLISH_FIELDS: [&str; 5] = ["metadata_signing_service", "checksum_type", "repo_config", "compression_type", "layout"];

/// The PATCH that gives a derived repository the publish settings of its source (empty: they agree), so a
/// view of a signed repository serves signed metadata. Fields this server does not have are never sent.
fn publish_settings(source: &Value, derived: &Value) -> serde_json::Map<String, Value> {
    RPM_PUBLISH_FIELDS
        .iter()
        .filter_map(|&k| source.get(k).filter(|v| derived.get(k) != Some(*v)).map(|v| (k.to_string(), v.clone())))
        .collect()
}

/// The PATCH that gives a derived Deb repository the signing choice of the publish (empty: it has it already):
/// "" = the keys of the source, "none" = no keys, else that signing service and no per-release keys.
/// Fields the derived repository does not have are never sent.
fn deb_signing_patch(choice: &str, source: &Value, derived: &Value) -> serde_json::Map<String, Value> {
    let want = |k: &str| match choice {
        "" => source.get(k).cloned(),
        _ if k == "signing_service_release_overrides" => Some(json!({})),
        "none" => Some(Value::Null),
        href => Some(href.into()),
    };
    ["signing_service", "signing_service_release_overrides"]
        .into_iter()
        .filter_map(|k| {
            let (mut v, have) = (want(k)?, derived.get(k)?);
            if v == *have {
                return None;
            }
            // pulp_deb changes only the per-release keys a PATCH names; `null` deletes one.
            if let (Some(w), Some(h)) = (v.as_object_mut(), have.as_object()) {
                for gone in h.keys() {
                    w.entry(gone.clone()).or_insert(Value::Null);
                }
            }
            Some((k.to_string(), v))
        })
        .collect()
}

pub fn derived_repo_name(cv: &str, repo: &str) -> String {
    format!("pulp-tui-cv-{}-{}", slug(cv), slug(repo))
}

pub fn distribution_name(cv: &str, env: &str, repo: &str) -> String {
    format!("pulp-tui-cv-{}-{}-{}", slug(cv), slug(env), slug(repo))
}

pub fn base_path(cv: &str, env_label: &str, repo: &str) -> String {
    format!("{}/{}/{}", slug(env_label), slug(cv), slug(repo))
}

/// Refuse repositories that one (composite) view cannot serve together: `(view name, repositories)` per
/// part. The same repository in two parts, or two names with one path segment (also inside one part).
pub fn check_repo_clash(parts: &[(String, Vec<CvRepo>)]) -> Result<()> {
    let all: Vec<(&str, &CvRepo)> = parts.iter().flat_map(|(n, rs)| rs.iter().map(move |r| (n.as_str(), r))).collect();
    for (i, (a, x)) in all.iter().enumerate() {
        for (b, y) in &all[i + 1..] {
            if x.repo_href == y.repo_href {
                bail!("components {a} and {b} both contain repository {}", x.repo_name);
            }
            let segment = slug(&x.repo_name);
            if segment == slug(&y.repo_name) {
                bail!("repositories {} ({a}) and {} ({b}) would be served at the same path segment {segment}", x.repo_name, y.repo_name);
            }
        }
    }
    Ok(())
}

/// Which component versions changed between two composite versions; empty for normal views.
pub fn component_lines(old: &[UsedVersion], new: &[UsedVersion]) -> Vec<String> {
    let mut names: Vec<&str> = old.iter().chain(new).map(|u| u.name.as_str()).collect();
    if names.is_empty() {
        return vec![];
    }
    names.sort();
    names.dedup();
    let mut out = vec!["== components".to_string()];
    for n in names {
        let num = |vs: &[UsedVersion]| vs.iter().find(|u| u.name == n).map(|u| u.number);
        out.push(match (num(old), num(new)) {
            (Some(a), Some(b)) if a == b => format!("  {n}: v{a} (unchanged)"),
            (Some(a), Some(b)) => format!("  {n}: v{a} -> v{b}"),
            (None, Some(b)) => format!("  {n}: added (v{b})"),
            (Some(a), None) => format!("  {n}: removed (was v{a})"),
            (None, None) => continue,
        });
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Requires the version to be active in the prior environment.
    Promote,
    /// Any published version; used to restore an earlier version.
    Rollback,
}

/// Content of one repository selected by the filters.
struct Selection {
    /// `None` = everything (no filters), copied without an explicit unit list.
    hrefs: Option<Vec<String>>,
    keys: BTreeSet<String>,
}

impl CvService {
    pub async fn audit(&self, action: &str, detail: &str) {
        if let Err(e) = self.store.audit(&self.scope, &self.actor, action, detail).await {
            tracing::warn!("audit write failed: {e:#}");
        }
    }

    /// This service with the lock of Content View `cv_id` for `operation` (shown to other operators).
    pub async fn locked(mut self, cv_id: i64, operation: &str) -> Result<CvService> {
        let name = self.store.content_view(cv_id).await?.name;
        self.lock = Some(CvLock::acquire(self.store.clone(), cv_id, &name, &self.actor, operation).await?);
        Ok(self)
    }

    /// Called before every Pulp mutation and checkpoint/binding write of a workflow.
    fn alive(&self) -> Result<()> {
        self.lock.as_ref().map_or(Ok(()), |l| l.check())
    }

    /// The id of another pulp-tui database that owns the object at `href` (database ID label);
    /// `None` when it is ours, unlabeled, owned by an alias, or gone. Call it before acting on a recorded href.
    async fn foreign_owner(&self, href: &str, aliases: &[String]) -> Result<Option<String>> {
        #[derive(serde::Deserialize)]
        struct Labeled {
            #[serde(default)]
            pulp_labels: BTreeMap<String, String>,
        }
        match self.client.get::<Labeled>(href, &[]).await {
            Ok(o) => Ok(ops::label_owner(&o.pulp_labels, LABEL_DB, self.store.db_id(), aliases).err()),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    // ---- selection ---------------------------------------------------------------

    async fn select(&self, repo: &CvRepo, source_version: &str, with_keys: bool) -> Result<Selection> {
        match repo.repo_type {
            RepoType::Rpm => self.select_rpm(repo, source_version, with_keys).await,
            RepoType::File => self.select_file(repo, source_version, with_keys).await,
            RepoType::Deb => self.select_deb(repo, source_version, with_keys).await,
        }
    }

    /// Like RPM without advisories: the structured copy brings the release records of the chosen packages.
    async fn select_deb(&self, repo: &CvRepo, source_version: &str, with_keys: bool) -> Result<Selection> {
        if repo.filters.is_empty() && !with_keys {
            return Ok(Selection { hrefs: None, keys: BTreeSet::new() });
        }
        let sel = Selector::new(&repo.filters);
        let chosen: Vec<DebPackage> =
            deb::packages_in(&self.client, source_version).await?.into_iter().filter(|p| sel.deb_package(p)).collect();
        let keys = chosen.iter().map(DebPackage::key).collect();
        let hrefs = (!repo.filters.is_empty()).then(|| chosen.into_iter().map(|p| p.pulp_href).collect());
        Ok(Selection { hrefs, keys })
    }

    async fn select_rpm(&self, repo: &CvRepo, source_version: &str, with_keys: bool) -> Result<Selection> {
        let c = &self.client;
        let sel = Selector::new(&repo.filters);
        if repo.filters.is_empty() && !with_keys {
            return Ok(Selection { hrefs: None, keys: BTreeSet::new() });
        }
        let pkgs = rpm::packages_in(c, source_version).await?;
        let chosen: Vec<&Package> = pkgs.iter().filter(|p| sel.package(p)).collect();
        let keys = chosen.iter().map(|p| p.nevra()).collect();
        if repo.filters.is_empty() {
            return Ok(Selection { hrefs: None, keys });
        }
        let chosen_nevra: HashSet<(String, String, String, String, String)> =
            chosen.iter().map(|p| (p.name.clone(), norm_epoch(&p.epoch), p.version.clone(), p.release.clone(), p.arch.clone())).collect();
        let all_nevra: HashSet<_> =
            pkgs.iter().map(|p| (p.name.clone(), norm_epoch(&p.epoch), p.version.clone(), p.release.clone(), p.arch.clone())).collect();
        let mut hrefs: Vec<String> = chosen.iter().map(|p| p.pulp_href.clone()).collect();
        // Advisories: kept only if every referenced package present in the source is selected.
        let advisories: Vec<Value> = c
            .list_all(
                &c.api("content/rpm/advisories/"),
                &[("repository_version", source_version.into()), ("fields", "pulp_href,pkglist".into())],
                1_000_000,
            )
            .await?;
        for a in &advisories {
            let refs: Vec<_> = a["pkglist"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|coll| coll["packages"].as_array().cloned().unwrap_or_default())
                .map(|p| {
                    let s = |k: &str| p[k].as_str().map(str::to_string).unwrap_or_else(|| p[k].to_string());
                    (s("name"), norm_epoch(&s("epoch")), s("version"), s("release"), s("arch"))
                })
                .filter(|n| all_nevra.contains(n))
                .collect();
            if refs.iter().all(|n| chosen_nevra.contains(n)) {
                hrefs.push(a["pulp_href"].as_str().unwrap_or_default().into());
            }
        }
        // Other metadata (comps, modules, distribution trees, repo metadata files) is kept as is.
        for h in ops::content_in(c, source_version).await? {
            if !h.contains("/content/rpm/packages/") && !h.contains("/content/rpm/advisories/") {
                hrefs.push(h);
            }
        }
        Ok(Selection { hrefs: Some(hrefs), keys })
    }

    async fn select_file(&self, repo: &CvRepo, source_version: &str, _with_keys: bool) -> Result<Selection> {
        let sel = Selector::new(&repo.filters);
        let files = file::files_in(&self.client, source_version).await?;
        let chosen: Vec<_> = files.iter().filter(|f| sel.file(f)).collect();
        Ok(Selection {
            keys: chosen.iter().map(|f| file::key(f)).collect(),
            hrefs: Some(chosen.into_iter().map(|f| f.pulp_href.clone()).collect()),
        })
    }

    // ---- publish -------------------------------------------------------------------

    /// Create a new numbered version from the current definition and publish it. `signing` applies to every
    /// Deb repository: "" = same as source, "none", or a signing service href (a composite ignores it).
    pub async fn publish(&self, cv_id: i64, notes: &str, signing: &str, log: Log<'_>, on: OnTask<'_>) -> Result<CvVersion> {
        let cv = self.store.content_view(cv_id).await?;
        if cv.composite {
            return self.publish_composite(&cv, notes, log, on).await;
        }
        let repos = self.store.cv_repos(cv_id).await?;
        if repos.is_empty() {
            bail!("content view {} has no repositories", cv.name);
        }
        check_repo_clash(&[(cv.name.clone(), repos.clone())])?;
        // Snapshot all source versions first so the view is consistent in time.
        let mut sources = vec![];
        for r in &repos {
            let repo: Repository =
                self.client.get(&r.repo_href, &[]).await.with_context(|| format!("source repository {}", r.repo_name))?;
            if let Some(owner) = repo.pulp_labels.get(LABEL_CV) {
                bail!("{} is a snapshot repository of content view {owner}; remove it from {} and add its source", r.repo_name, cv.name);
            }
            let sv = repo.latest_version_href.ok_or_else(|| anyhow!("{} has no versions", r.repo_name))?;
            sources.push(sv);
        }
        self.alive()?;
        let v = self.store.begin_version(cv_id, notes, &repos).await?;
        for (mut vr, sv) in self.store.version_repos(v.id).await?.into_iter().zip(sources) {
            vr.source_version = Some(sv);
            vr.signing = signing.into(); // a checkpoint: a resumed publish signs as asked
            self.alive()?;
            self.store.save_version_repo(&vr).await?;
        }
        self.audit("cv.publish.start", &format!("{} version {}", cv.name, v.number)).await;
        log(format!("{} v{}: publishing {} repositories", cv.name, v.number, repos.len()));
        self.resume(v.id, log, on).await
    }

    /// Each component of a composite with the version it publishes from (its pin, else its newest
    /// published version) and that version's rows. Reads only.
    async fn resolve_components(&self, cv: &ContentView) -> Result<Vec<(Component, CvVersion, Vec<CvVersionRepo>)>> {
        let comps = self.store.components(cv.id).await?;
        if comps.is_empty() {
            bail!("composite {} has no components", cv.name);
        }
        let mut out = vec![];
        for c in comps {
            let v = match c.pinned {
                Some(id) => self.store.version(id).await?,
                None => self
                    .store
                    .versions(c.cv_id)
                    .await?
                    .into_iter()
                    .find(|v| v.state == "published")
                    .ok_or_else(|| anyhow!("component {} has no published version", c.name))?,
            };
            if v.state != "published" {
                bail!("pinned version v{} of {} is {}", v.number, c.name, v.state);
            }
            let rows = self.store.version_repos(v.id).await?;
            out.push((c, v, rows));
        }
        // Component definitions change after they were added: check again over what is published now.
        let parts: Vec<_> = out.iter().map(|(c, _, rows)| (c.name.clone(), rows.iter().map(|r| r.repo().clone()).collect())).collect();
        check_repo_clash(&parts)?;
        Ok(out)
    }

    /// A composite version borrows the rows of its component versions: nothing is copied or published
    /// in Pulp; `resume` then binds Library at the composite's own paths.
    async fn publish_composite(&self, cv: &ContentView, notes: &str, log: Log<'_>, on: OnTask<'_>) -> Result<CvVersion> {
        let parts = self.resolve_components(cv).await?;
        let used: Vec<i64> = parts.iter().map(|(_, v, _)| v.id).collect();
        let rows: Vec<CvVersionRepo> = parts.iter().flat_map(|(_, _, r)| r.iter().cloned()).collect();
        self.alive()?;
        let v = self.store.begin_composite_version(cv.id, notes, &used, &rows).await?;
        self.audit("cv.publish.start", &format!("{} version {}", cv.name, v.number)).await;
        let list: Vec<String> = parts.iter().map(|(c, u, _)| format!("{} v{}", c.name, u.number)).collect();
        log(format!("{} v{}: composite of {}", cv.name, v.number, list.join(", ")));
        self.resume(v.id, log, on).await
    }

    /// Publish every composite with auto publish that uses `component_id` as latest, each under its own
    /// lock. One result per composite; a failure does not stop the others. Never promotes beyond Library.
    pub async fn auto_publish(&self, component_id: i64, log: Log<'_>, on: OnTask<'_>) -> Vec<std::result::Result<String, String>> {
        let found = async {
            let name = self.store.content_view(component_id).await?.name;
            let latest = self.store.versions(component_id).await?.into_iter().find(|v| v.state == "published");
            let notes = format!("auto publish after {name} v{}", latest.map_or(0, |v| v.number));
            anyhow::Ok((notes, self.store.auto_composites(component_id).await?))
        };
        let (notes, composites) = match found.await {
            Ok(x) => x,
            Err(e) => return vec![Err(format!("auto publish: {e:#}"))],
        };
        let mut out = vec![];
        for c in composites {
            let res = async {
                CvService { lock: None, ..self.clone() }.locked(c.id, "auto publish").await?.publish(c.id, &notes, "", log, on).await
            };
            out.push(match res.await {
                Ok(v) => Ok(format!("{} v{} published", c.name, v.number)),
                Err(e) => Err(format!("{}: {e:#}", c.name)),
            });
        }
        out
    }

    /// Run (or continue) the publish workflow of a version from its checkpoints.
    pub async fn resume(&self, version_id: i64, log: Log<'_>, on: OnTask<'_>) -> Result<CvVersion> {
        let v = self.store.version(version_id).await?;
        let cv = self.store.content_view(v.cv_id).await?;
        if v.state == "published" {
            return Ok(v);
        }
        if v.state == "deleted" {
            bail!("version {} was deleted", v.number);
        }
        if v.state == "deleting" {
            bail!("version {} is being deleted; delete it again to finish", v.number);
        }
        self.alive()?;
        self.store.finish_version(v.id, "publishing", None).await?;
        let res = async {
            for mut vr in self.store.version_repos(v.id).await? {
                self.publish_repo(&cv.name, &mut vr, log, on).await.with_context(|| format!("repository {}", vr.repo().repo_name))?;
            }
            anyhow::Ok(())
        }
        .await;
        match res {
            Ok(()) => {
                self.alive()?;
                self.store.finish_version(v.id, "published", None).await?;
                self.audit("cv.publish.done", &format!("{} version {}", cv.name, v.number)).await;
                log(format!("{} v{} published", cv.name, v.number));
                let lib = self.store.ensure_library(&self.scope).await?;
                self.promote(v.cv_id, v.id, lib, Mode::Rollback, log, on).await?;
                Ok(self.store.version(v.id).await?)
            }
            Err(e) => {
                let msg = format!("{e:#}");
                // Runs even when the lock is lost (it records why the workflow stopped), but then
                // only over this workflow's own `publishing` state.
                if self.alive().is_ok() {
                    self.store.finish_version(v.id, "failed", Some(&msg)).await?;
                } else {
                    self.store.fail_publishing_version(v.id, &msg).await?;
                }
                self.audit("cv.publish.failed", &format!("{} version {}: {msg}", cv.name, v.number)).await;
                Err(e)
            }
        }
    }

    async fn publish_repo(&self, cv: &str, vr: &mut CvVersionRepo, log: Log<'_>, on: OnTask<'_>) -> Result<()> {
        let c = &self.client;
        let repo = vr.repo().clone();
        let t = repo.repo_type;
        if vr.publication.is_some() {
            return Ok(());
        }
        if vr.borrowed {
            bail!("the component version has no publication"); // its objects are not ours to publish
        }
        let name = derived_repo_name(cv, &repo.repo_name);
        let aliases = self.store.aliases().await?;
        // Resuming: another database may have adopted the repository since the checkpoint.
        if let Some(r) = &vr.derived_repo
            && let Some(o) = self.foreign_owner(r, &aliases).await?
        {
            bail!("repository {name} is owned by another pulp-tui database ({o})");
        }
        if vr.derived_repo.is_none() {
            let href = match ops::find_by_name::<Repository>(c, &c.api(t.repositories()), &name).await? {
                // Never write into a repository this view does not own (name collision).
                Some(r) if r.pulp_labels.get(LABEL_CV).map(String::as_str) == Some(slug(cv).as_str()) => {
                    let id = self.store.db_id();
                    match ops::label_owner(&r.pulp_labels, LABEL_DB, id, &aliases) {
                        Err(o) => bail!("repository {name} is owned by another pulp-tui database ({o})"),
                        Ok(true) => {
                            let mut labels = r.pulp_labels.clone();
                            labels.insert(LABEL_DB.into(), id.into());
                            self.alive()?;
                            let m = c.patch(&r.pulp_href, &json!({ "pulp_labels": labels })).await?;
                            ops::settle(c, m, on).await?;
                        }
                        Ok(false) => {}
                    }
                    r.pulp_href
                }
                Some(_) => bail!("repository {name} exists but is not labeled {LABEL_CV}={}; refusing to modify it", slug(cv)),
                None => {
                    log(format!("creating derived repository {name}"));
                    let body = json!({
                        "name": name,
                        "description": format!("Content View {cv} snapshot of {} (managed by pulp-tui)", repo.repo_name),
                        "pulp_labels": { LABEL_CV: slug(cv), LABEL_DB: self.store.db_id() },
                    });
                    self.alive()?;
                    ops::create(c, &c.api(t.repositories()), &body, "/repositories/", on).await?
                }
            };
            vr.derived_repo = Some(href);
            self.alive()?;
            self.store.save_version_repo(vr).await?;
        }
        let derived = vr.derived_repo.clone().unwrap_or_default();
        if vr.derived_version.is_none() {
            let sv = vr.source_version.clone();
            let copy = async {
                let sv = sv.ok_or_else(|| anyhow!("missing source version"))?;
                log(format!("{}: selecting content from {sv}", repo.repo_name));
                let sel = self.select(&repo, &sv, false).await?;
                self.alive()?;
                anyhow::Ok(match t {
                    RepoType::Rpm => rpm::copy(c, &sv, &derived, Some(0), sel.hrefs, false).await?,
                    RepoType::File => file::modify(c, &derived, true, sel.hrefs.unwrap_or_default()).await?,
                    RepoType::Deb => deb::copy(c, &sv, &derived, Some(0), sel.hrefs).await?,
                })
            };
            let task = self.tracked_task(vr, "copy", copy, log, on).await?;
            // No new version is created when the content equals the latest version.
            let version = match task.created_matching("/versions/") {
                Some(v) => v,
                None => {
                    let r: Repository = c.get(&derived, &[]).await?;
                    r.latest_version_href.ok_or_else(|| anyhow!("derived repository has no version"))?
                }
            };
            let rv: RepoVersion = c.get(&version, &[]).await?;
            vr.content_count = Some(rv.content_summary.present.values().map(|x| x.count as i64).sum());
            vr.derived_version = Some(version);
            vr.pending_task = None;
            self.alive()?;
            self.store.save_version_repo(vr).await?;
        }
        let dv = vr.derived_version.clone().unwrap_or_default();
        // `service`: the signing service that the PATCH sets (Deb only).
        let (what, patch, service) = match t {
            RepoType::Rpm => {
                let (src, dst): (Value, Value) = (c.get(&repo.repo_href, &[]).await?, c.get(&derived, &[]).await?);
                ("publish settings of the source", publish_settings(&src, &dst), None)
            }
            RepoType::File => ("", serde_json::Map::new(), None),
            RepoType::Deb => {
                let (src, dst): (Value, Value) = (c.get(&repo.repo_href, &[]).await?, c.get(&derived, &[]).await?);
                let service = Some(vr.signing.as_str()).filter(|s| !["", "none"].contains(s));
                if service.is_some() && dst.get("signing_service").is_none() {
                    bail!("not supported by this server: signing_service on Deb repositories");
                }
                ("signing", deb_signing_patch(&vr.signing, &src, &dst), service)
            }
        };
        if !patch.is_empty() {
            log(format!("{}: {what}: {}", repo.repo_name, patch.keys().cloned().collect::<Vec<_>>().join(", ")));
            self.alive()?;
            let m = match (c.patch(&derived, &Value::Object(patch)).await, service) {
                // The choice is stored with the version, so Resume would send the refused service again.
                (Err(e @ PulpError::Api { status: 400, .. }), Some(s)) => {
                    return Err(e).context(format!(
                        "signing service {s} was refused by the server; delete this Content View version and publish again with another choice"
                    ));
                }
                (m, _) => m?,
            };
            ops::settle(c, m, on).await?;
        }
        log(format!("{}: publishing {dv}", repo.repo_name));
        self.alive()?;
        vr.publication = Some(match t {
            RepoType::Rpm | RepoType::File => ops::ensure_publication(c, t, &dv, None, on).await?,
            // Never reused: a publication signed with the repository's keys stores `signing_service: null`, so
            // one of another signing choice cannot be told apart. Checkpointed like the copy.
            RepoType::Deb => {
                // With `derived_version` set, `pending_task` is the publication task (the copy step clears it with the version).
                let post = async { anyhow::Ok(c.post(&c.api(t.publications()), &json!({ "repository_version": dv })).await?) };
                let task = self.tracked_task(vr, "publish", post, log, on).await?;
                let publication =
                    task.created_matching("/publications/").ok_or_else(|| anyhow!("the publish task created no publication"))?;
                vr.pending_task = None;
                publication
            }
        });
        self.alive()?;
        self.store.save_version_repo(vr).await?;
        Ok(())
    }

    /// The task of one checkpointed step of `vr` (the copy, or a Deb publication): re-attach to `vr.pending_task`,
    /// else run `submit` and record the task before waiting. A failed or canceled task is not re-attached: it is
    /// cleared, so the next Resume sends it again (a copy starts from an empty base).
    async fn tracked_task(
        &self,
        vr: &mut CvVersionRepo,
        what: &str,
        submit: impl Future<Output = Result<Mutation>>,
        log: Log<'_>,
        on: OnTask<'_>,
    ) -> Result<Task> {
        let c = &self.client;
        let task = match vr.pending_task.clone() {
            Some(h) => {
                log(format!("{}: re-attaching to task {h}", vr.repo().repo_name));
                ops::wait_task(c, &h, on).await
            }
            None => {
                let Mutation::Task(h) = submit.await? else { bail!("{what} did not return a task") };
                vr.pending_task = Some(h.clone());
                // Saved even when the lock is lost: it records a task already sent, so Resume re-attaches.
                self.store.save_version_repo(vr).await?;
                self.alive()?;
                ops::wait_task(c, &h, on).await
            }
        };
        if let Err(PulpError::TaskFailed { .. }) = &task {
            vr.pending_task = None;
            self.alive()?;
            self.store.save_version_repo(vr).await?;
        }
        Ok(task?)
    }

    // ---- promote / rollback --------------------------------------------------------

    /// Bind `version_id` to `env_id`. Re-points stable distributions; repositories that
    /// are bound in the environment but not part of the version stop being served.
    pub async fn promote(&self, cv_id: i64, version_id: i64, env_id: i64, mode: Mode, log: Log<'_>, on: OnTask<'_>) -> Result<()> {
        let cv = self.store.content_view(cv_id).await?;
        let v = self.store.version(version_id).await?;
        if v.cv_id != cv_id || v.state != "published" {
            bail!("version {} is not a published version of {}", v.number, cv.name);
        }
        let envs = self.store.environments(&self.scope).await?;
        let env = envs.iter().find(|e| e.id == env_id).ok_or_else(|| anyhow!("unknown environment"))?;
        if mode == Mode::Promote
            && let Some(prior) = env.prior_id
        {
            let pb = self.store.binding(prior, cv_id).await?;
            if pb.as_ref().map(|b| (b.version_id, b.state.as_str())) != Some((version_id, "active")) {
                let pname = envs.iter().find(|e| e.id == prior).map(|e| e.name.as_str()).unwrap_or("?");
                bail!("version {} is not active in {pname}; promote it there first (or use rollback)", v.number);
            }
        }
        self.alive()?;
        self.store.set_binding(env_id, cv_id, version_id, "promoting").await?;
        log(format!("{} v{} -> {}", cv.name, v.number, env.name));
        let mut errors = vec![];
        let vrs = self.store.version_repos(version_id).await?;
        for vr in &vrs {
            if let Err(e) = self.bind_repo(&cv.name, env, vr, on).await {
                errors.push(format!("{}: {e:#}", vr.repo().repo_name));
            }
        }
        // `failed` means no repository of the version is served; unbind errors below only make it `partial`.
        let bind_failed = errors.len();
        // Repositories no longer part of the view stop serving (distribution kept for rollback).
        let aliases = self.store.aliases().await?;
        for b in self.store.binding_repos(env_id, cv_id).await? {
            if vrs.iter().any(|vr| vr.repo().repo_href == b.repo_href) || b.publication.is_none() {
                continue;
            }
            let res = async {
                if let Some(o) = self.foreign_owner(&b.distribution, &aliases).await? {
                    bail!(
                        "distribution {} is owned by another pulp-tui database ({o})",
                        distribution_name(&cv.name, &env.name, &b.repo_name)
                    );
                }
                self.alive()?;
                let m = self.client.patch(&b.distribution, &json!({ "publication": null })).await?;
                ops::settle(&self.client, m, on).await?;
                self.alive() // before the checkpoint below
            }
            .await;
            match res {
                Ok(()) => self.store.save_binding_repo(&BindingRepo { publication: None, ..b }).await?,
                Err(e) => errors.push(format!("unbind {}: {e}", b.repo_name)),
            }
        }
        let state = if errors.is_empty() {
            "active"
        } else if bind_failed < vrs.len() {
            "partial"
        } else {
            "failed"
        };
        // Runs even when the lock is lost (it records the partial state; a retry is idempotent), but
        // then only over this promotion's own `promoting` row.
        if self.alive().is_ok() {
            self.store.set_binding(env_id, cv_id, version_id, state).await?;
        } else {
            self.store.finish_promoting_binding(env_id, cv_id, version_id, state).await?;
        }
        let what = if mode == Mode::Rollback && env.name != LIBRARY { "cv.rollback" } else { "cv.promote" };
        self.audit(what, &format!("{} v{} -> {}: {state} {}", cv.name, v.number, env.name, errors.join("; "))).await;
        if errors.is_empty() {
            log(format!("{} v{} active in {}", cv.name, v.number, env.name));
            Ok(())
        } else {
            Err(anyhow!("{} in {}: {state}: {}", cv.name, env.name, errors.join("; ")))
        }
    }

    /// Where `repo` of view `cv` is served in `env`: an existing distribution keeps its path (older
    /// servers use a `cv/` prefix, and an operator may move a distribution), a new one gets `base_path`.
    pub async fn served_path(&self, cv: &str, env: &Environment, repo: &CvRepo) -> Result<String> {
        let coll = self.client.api(repo.repo_type.distributions());
        let name = distribution_name(cv, &env.name, &repo.repo_name);
        Ok(match ops::find_by_name::<Distribution>(&self.client, &coll, &name).await? {
            Some(d) => d.base_path,
            None => base_path(cv, &env.label, &repo.repo_name),
        })
    }

    async fn bind_repo(&self, cv: &str, env: &Environment, vr: &CvVersionRepo, on: OnTask<'_>) -> Result<()> {
        let repo = vr.repo();
        let publication = vr.publication.as_deref().ok_or_else(|| anyhow!("version has no publication"))?;
        let name = distribution_name(cv, &env.name, &repo.repo_name);
        let bp = self.served_path(cv, env, repo).await?;
        let labels = json!({ LABEL_CV: slug(cv), LABEL_ENV: slug(&env.name) });
        let aliases = self.store.aliases().await?;
        self.alive()?;
        let db = Some((LABEL_DB, self.store.db_id(), aliases.as_slice()));
        let href = ops::ensure_distribution(&self.client, repo.repo_type, &name, &bp, Some(publication), &labels, db, on).await?;
        let cv_id = vr_cv(&self.store, vr.version_id).await?;
        self.alive()?;
        self.store
            .save_binding_repo(&BindingRepo {
                env_id: env.id,
                cv_id,
                repo_href: repo.repo_href.clone(),
                repo_name: repo.repo_name.clone(),
                repo_type: repo.repo_type,
                distribution: href,
                base_path: bp,
                publication: Some(publication.into()),
            })
            .await?;
        Ok(())
    }

    // ---- lifecycle environment labels -----------------------------------------------

    /// The served paths that the label `label` of environment `env_id` moves: each served row whose
    /// path differs, with its new path. Reads only.
    pub async fn relabel_impact(&self, env_id: i64, label: &str) -> Result<Vec<(BindingRepo, String)>> {
        validate_label(label)?;
        if let Some(e) = self.store.environments(&self.scope).await?.into_iter().find(|e| e.id != env_id && e.label == label) {
            bail!("label {label} is already used by environment {}", e.name);
        }
        let mut out = vec![];
        for cv_id in self.store.env_cv_ids(env_id).await? {
            let cv = self.store.content_view(cv_id).await?.name;
            for b in self.store.binding_repos(env_id, cv_id).await? {
                let new = base_path(&cv, label, &b.repo_name);
                if b.base_path != new {
                    out.push((b, new));
                }
            }
        }
        Ok(out)
    }

    /// Change the label of environment `env_id` and move its served paths in place. The stored label is
    /// the checkpoint: a rerun with the same label moves what is left. Returns result lines and the
    /// number of rows not moved (they keep their old path).
    pub async fn relabel_env(&self, env_id: i64, label: &str, log: Log<'_>, on: OnTask<'_>) -> Result<(Vec<String>, usize)> {
        let envs = self.store.environments(&self.scope).await?;
        let env = envs.into_iter().find(|e| e.id == env_id).ok_or_else(|| anyhow!("unknown environment"))?;
        let mut locks = vec![];
        for cv_id in self.store.env_cv_ids(env_id).await? {
            let name = self.store.content_view(cv_id).await?.name;
            let lock = CvLock::acquire(self.store.clone(), cv_id, &name, &self.actor, &format!("relabel {}", env.name)).await?;
            locks.push((cv_id, name, lock));
        }
        let alive = || locks.iter().try_for_each(|(_, _, l)| l.check());
        alive()?;
        let aliases = self.store.aliases().await?;
        let db = Some((LABEL_DB, self.store.db_id(), aliases.as_slice()));
        self.store.set_env_label(env_id, label).await?; // after the calls that can fail: then the audit entry below is always written
        let (mut lines, mut moved, mut failed) = (vec![], 0, 0);
        let res = async {
            for (b, new) in self.relabel_impact(env_id, label).await? {
                let (repo, old) = (b.repo_name.clone(), b.base_path.clone());
                // A view bound after the locks were taken is not ours to change now.
                let Some((_, cv, _)) = locks.iter().find(|l| l.0 == b.cv_id) else {
                    failed += 1;
                    let line = format!(
                        "FAILED {repo} of {}: its view was bound during the relabel (still at /{old})",
                        self.store.content_view(b.cv_id).await?.name
                    );
                    log(line.clone());
                    lines.push(line);
                    continue;
                };
                let labels = json!({ LABEL_CV: slug(cv), LABEL_ENV: slug(&env.name) });
                alive()?;
                let res = ops::move_distribution(&self.client, &b.distribution, &new, &labels, db, on).await;
                alive()?;
                let line = match res {
                    Ok(_) => {
                        self.store.save_binding_repo(&BindingRepo { base_path: new.clone(), ..b }).await?;
                        moved += 1;
                        format!("moved {repo} of {cv}: /{old} -> /{new}")
                    }
                    Err(e) => {
                        failed += 1;
                        format!("FAILED {repo} of {cv}: {e:#} (still at /{old})")
                    }
                };
                log(line.clone());
                lines.push(line);
            }
            anyhow::Ok(())
        }
        .await;
        // Written also when a lost lock stopped the workflow: the label is already changed.
        if env.label != label || moved > 0 {
            self.audit("env.relabel", &format!("{}: {} -> {label}: {moved} moved, {failed} failed", env.name, env.label)).await;
        }
        res?;
        if moved + failed == 0 {
            lines.push("no served path changes".into());
        }
        for (cv_id, cv, _) in &locks {
            let issues = self.verify(*cv_id, env_id).await.unwrap_or_else(|e| vec![format!("{e:#}")]);
            lines.push(format!("{cv}: {}", if issues.is_empty() { "OK" } else { "DRIFT" }));
            lines.extend(issues.into_iter().map(|i| format!("    {i}")));
        }
        Ok((lines, failed))
    }

    // ---- verification / drift --------------------------------------------------------

    /// Check that what Pulp serves for an environment is exactly the bound version.
    /// Returns human-readable findings; empty means verified.
    pub async fn verify(&self, cv_id: i64, env_id: i64) -> Result<Vec<String>> {
        let c = &self.client;
        let Some(b) = self.store.binding(env_id, cv_id).await? else { return Ok(vec!["not bound".into()]) };
        let mut issues = vec![];
        if b.state != "active" {
            issues.push(format!("binding state is {}", b.state));
        }
        let vrs = self.store.version_repos(b.version_id).await?;
        for br in self.store.binding_repos(env_id, cv_id).await? {
            let vr = vrs.iter().find(|vr| vr.repo().repo_href == br.repo_href);
            let want = vr.and_then(|v| v.publication.clone());
            let d: Distribution = match c.get(&br.distribution, &[]).await {
                Ok(d) => d,
                Err(e) => {
                    issues.push(format!("{}: distribution {}: {e}", br.repo_name, br.distribution));
                    continue;
                }
            };
            if d.publication != want {
                issues.push(format!(
                    "{}: distribution serves {}, expected {}",
                    br.repo_name,
                    d.publication.as_deref().unwrap_or("nothing"),
                    want.as_deref().unwrap_or("nothing")
                ));
                continue;
            }
            let Some(vr) = vr else { continue };
            let p: Value = match c.get(want.as_deref().unwrap_or_default(), &[]).await {
                Ok(p) => p,
                Err(e) => {
                    issues.push(format!("{}: publication: {e}", br.repo_name));
                    continue;
                }
            };
            if p["repository_version"].as_str() != vr.derived_version.as_deref() {
                issues.push(format!(
                    "{}: publication is of {}, expected {}",
                    br.repo_name,
                    p["repository_version"].as_str().unwrap_or("?"),
                    vr.derived_version.as_deref().unwrap_or("?")
                ));
            }
            if let Some(dv) = &vr.derived_version
                && let Err(e) = c.get::<Value>(dv, &[("fields", "pulp_href".into())]).await
            {
                issues.push(format!("{}: snapshot {dv} missing (pruned?): {e}", br.repo_name));
            }
            let Some(base) = d.base_url.as_deref() else {
                issues.push(format!("{}: distribution has no base_url", br.repo_name));
                continue;
            };
            let dv = vr.derived_version.as_deref().unwrap_or_default();
            if let Err(e) = ops::verify_served(c, br.repo_type, base, dv, p["layout"].as_str().unwrap_or_default()).await {
                issues.push(format!("{}: {e}", br.repo_name));
            }
        }
        // Retention on derived repositories would prune CV snapshots.
        for vr in &vrs {
            if let Some(dr) = &vr.derived_repo
                && let Ok(r) = c.get::<Repository>(dr, &[]).await
                && r.retain_repo_versions.is_some()
            {
                issues.push(format!("{}: retain_repo_versions is set on derived repository {}", vr.repo().repo_name, r.name));
            }
        }
        Ok(issues)
    }

    // ---- preview / compare -------------------------------------------------------------

    /// Exact content the current definition would select, compared to a published version.
    pub async fn preview(&self, cv_id: i64, against: Option<i64>) -> Result<Vec<String>> {
        let cv = self.store.content_view(cv_id).await?;
        let old = match against {
            Some(v) => self.store.version_repos(v).await?,
            None => vec![],
        };
        let old_name = if against.is_some() { "published" } else { "empty" };
        if cv.composite {
            let parts = self.resolve_components(&cv).await?;
            let old_used = match against {
                Some(v) => self.store.version_components(v).await?,
                None => vec![],
            };
            let used: Vec<UsedVersion> = parts
                .iter()
                .map(|(c, v, _)| UsedVersion { cv_id: c.cv_id, name: c.name.clone(), version_id: v.id, number: v.number })
                .collect();
            let rows: Vec<CvVersionRepo> = parts.into_iter().flat_map(|(_, _, r)| r).collect();
            let mut out = component_lines(&old_used, &used);
            out.extend(self.compare_rows(&old, &rows, old_name, "draft").await?);
            return Ok(out);
        }
        let repos = self.store.cv_repos(cv_id).await?;
        let mut out = vec![];
        for r in &repos {
            let repo: Repository = self.client.get(&r.repo_href, &[]).await?;
            let Some(sv) = repo.latest_version_href else {
                out.push(format!("== {} has no versions", r.repo_name));
                continue;
            };
            let sel = self.select(r, &sv, true).await?;
            let prev = old.iter().find(|o| o.repo().repo_href == r.repo_href).and_then(|o| o.derived_version.clone());
            let prev_keys = ops::content_keys(&self.client, r.repo_type, prev.as_deref()).await?;
            let d = ops::diff(&prev_keys, &sel.keys);
            out.push(format!(
                "== {} ({}) from {sv}: {} selected, filters: {}",
                r.repo_name,
                r.repo_type.label(),
                sel.keys.len(),
                crate::filters::format_rules(&r.filters)
            ));
            out.extend(d.lines(old_name, "draft"));
            out.push(String::new());
        }
        Ok(out)
    }

    /// Component changes (composites), then the content difference of each repository.
    pub async fn compare_versions(&self, a: i64, b: i64) -> Result<Vec<String>> {
        let mut out = component_lines(&self.store.version_components(a).await?, &self.store.version_components(b).await?);
        let (va, vb) = (self.store.version_repos(a).await?, self.store.version_repos(b).await?);
        out.extend(self.compare_rows(&va, &vb, "older", "newer").await?);
        Ok(out)
    }

    async fn compare_rows(&self, va: &[CvVersionRepo], vb: &[CvVersionRepo], old: &str, new: &str) -> Result<Vec<String>> {
        let mut hrefs: Vec<&CvRepo> = va.iter().chain(vb.iter()).map(|v| v.repo()).collect();
        hrefs.sort_by(|x, y| x.repo_name.cmp(&y.repo_name));
        hrefs.dedup_by(|x, y| x.repo_href == y.repo_href);
        let mut out = vec![];
        for r in hrefs {
            let dv = |vs: &[CvVersionRepo]| vs.iter().find(|v| v.repo().repo_href == r.repo_href).and_then(|v| v.derived_version.clone());
            let ka = ops::content_keys(&self.client, r.repo_type, dv(va).as_deref()).await?;
            let kb = ops::content_keys(&self.client, r.repo_type, dv(vb).as_deref()).await?;
            out.push(format!("== {} ({})", r.repo_name, r.repo_type.label()));
            out.extend(ops::diff(&ka, &kb).lines(old, new));
            out.push(String::new());
        }
        Ok(out)
    }

    // ---- cleanup -------------------------------------------------------------------------

    /// Delete a version's Pulp objects unless shared with another live version or bound. Refused while a
    /// composite uses or pins it; a composite version's borrowed rows are kept (they are the component's).
    pub async fn delete_version(&self, version_id: i64, log: Log<'_>, on: OnTask<'_>) -> Result<()> {
        let v = self.store.version(version_id).await?;
        let used = self.store.version_in_use(version_id).await?;
        if !used.is_empty() {
            bail!("version {} is bound to {}", v.number, used.join(", "));
        }
        // Marked before the composite check: a composite publish that starts later refuses a version that is not
        // `published` (`Store::begin`), and one that started earlier is found below. A failed delete stays `deleting`.
        self.alive()?;
        if !self.store.set_version_state(version_id, &v.state, "deleting").await? {
            bail!("version {} changed meanwhile; refresh and try again", v.number);
        }
        let users = self.store.composite_users(version_id).await?;
        if !users.is_empty() {
            self.store.set_version_state(version_id, "deleting", &v.state).await?;
            bail!("version {} is used by {}", v.number, users.join(", "));
        }
        let mut shared: HashSet<String> = HashSet::new();
        for other in self.store.versions(v.cv_id).await? {
            if other.id != v.id && other.state != "deleted" {
                for vr in self.store.version_repos(other.id).await? {
                    shared.extend(vr.publication.into_iter().chain(vr.derived_version));
                }
            }
        }
        // Any binding row of the scope, also a composite's, may still serve a publication of this version.
        let served = self.store.served_publications(&self.scope).await?;
        shared.extend(served.iter().map(|(p, _, _)| p.clone()));
        let aliases = self.store.aliases().await?;
        for vr in self.store.version_repos(version_id).await?.into_iter().filter(|vr| !vr.borrowed) {
            let by = served.iter().find(|(p, _, _)| vr.publication.as_ref() == Some(p));
            // Pulp deletes a repository version's publications with it: a kept publication keeps its version.
            if let (Some(p), Some(dv)) = (&vr.publication, &vr.derived_version)
                && shared.contains(p)
            {
                shared.insert(dv.clone());
            }
            // Publications and versions belong to the derived repository.
            let foreign = match &vr.derived_repo {
                Some(r) => self.foreign_owner(r, &aliases).await?,
                None => None,
            };
            for href in [vr.publication.as_ref(), vr.derived_version.as_ref()].into_iter().flatten() {
                if let Some(o) = &foreign {
                    log(format!("keeping {href}: owned by another pulp-tui database ({o})"));
                    continue;
                }
                if shared.contains(href) {
                    log(match by {
                        Some((_, other, env)) => format!("keeping {href}: still served by {other} in {env}"),
                        None => format!("keeping shared {href}"),
                    });
                    continue;
                }
                self.alive()?;
                match self.client.delete(href).await {
                    Ok(m) => {
                        ops::settle(&self.client, m, on).await?;
                    }
                    Err(e) if e.is_not_found() => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        self.alive()?;
        self.store.finish_version(version_id, "deleted", None).await?;
        let name = self.store.content_view(v.cv_id).await?.name;
        self.audit("cv.version.delete", &format!("{name} version {}", v.number)).await;
        Ok(())
    }

    /// Remove a whole content view: its distributions and derived repositories (a composite has none:
    /// its rows borrow the components' objects).
    pub async fn delete_view(&self, cv_id: i64, log: Log<'_>, on: OnTask<'_>) -> Result<()> {
        let cv = self.store.content_view(cv_id).await?;
        let refs = self.store.composite_refs(cv_id).await?;
        if !refs.is_empty() {
            bail!(
                "content view {} is used by {}; remove it from the composite and delete those composite versions first",
                cv.name,
                refs.join(", ")
            );
        }
        let mut hrefs: Vec<String> = self.store.all_binding_repos(cv_id).await?.into_iter().map(|b| b.distribution).collect();
        let served = self.store.served_publications(&self.scope).await?;
        let mut repos: BTreeSet<String> = BTreeSet::new();
        for v in self.store.versions(cv_id).await? {
            for vr in self.store.version_repos(v.id).await?.into_iter().filter(|vr| !vr.borrowed) {
                // Deleting the derived repository deletes its publications, also one another view still serves.
                if let Some((_, other, env)) = served.iter().find(|(p, o, _)| *o != cv.name && vr.publication.as_ref() == Some(p)) {
                    bail!(
                        "content view {}: publication of {} is still served by {other} in {env}; \
                         promote {other} in {env} again (or roll it back) so it stops serving it, or delete {other} first",
                        cv.name,
                        vr.repo().repo_name
                    );
                }
                repos.extend(vr.derived_repo);
            }
        }
        hrefs.extend(repos);
        let aliases = self.store.aliases().await?;
        for h in hrefs {
            if let Some(o) = self.foreign_owner(&h, &aliases).await? {
                log(format!("keeping {h}: owned by another pulp-tui database ({o})"));
                continue;
            }
            log(format!("deleting {h}"));
            self.alive()?;
            match self.client.delete(&h).await {
                Ok(m) => {
                    ops::settle(&self.client, m, on).await?;
                }
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.alive()?;
        self.store.delete_cv(cv_id).await?;
        self.audit("cv.delete", &cv.name).await;
        Ok(())
    }
}

fn norm_epoch(e: &str) -> String {
    if e.is_empty() || e == "null" { "0".into() } else { e.into() }
}

async fn vr_cv(store: &Store, version_id: i64) -> Result<i64> {
    Ok(store.version(version_id).await?.cv_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_paths_are_stable_and_safe() {
        assert_eq!(slug("base os/x86 64"), "base-os-x86-64");
        assert_eq!(derived_repo_name("web", "rhel9 baseos"), "pulp-tui-cv-web-rhel9-baseos");
        assert_eq!(distribution_name("web", "Prod", "os"), "pulp-tui-cv-web-Prod-os");
        assert_eq!(base_path("web", "prod", "os"), "prod/web/os");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_lock_releases_it() {
        let s = Arc::new(Store::open_in_memory().await.unwrap());
        let cv = s.create_cv("k", "web", "").await.unwrap();
        let l = CvLock::acquire(s.clone(), cv, "web", "a@h", "publish").await.unwrap();
        let e = CvLock::acquire(s.clone(), cv, "web", "b@h", "promote to Dev").await.unwrap_err();
        assert!(e.to_string().starts_with("Content View web is locked by a@h (publish) since "), "{e}");
        drop(l);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(s.lock_holder(cv).await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn renew_failure_marks_lost() {
        let s = Arc::new(Store::open_in_memory().await.unwrap());
        let cv = s.create_cv("k", "web", "").await.unwrap();
        let l = CvLock::acquire(s.clone(), cv, "web", "a@h", "publish").await.unwrap();
        l.renew_now().await;
        assert!(l.check().is_ok(), "renewal of a held lock succeeds");
        assert!(s.break_lock(cv, &s.lock_holder(cv).await.unwrap().unwrap()).await.unwrap());
        l.renew_now().await; // one renewal round without waiting 30 s
        assert!(l.check().unwrap_err().to_string().starts_with("lock on web lost: taken over or broken"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stale_renewal_fails_the_check() {
        let s = Arc::new(Store::open_in_memory().await.unwrap());
        let cv = s.create_cv("k", "web", "").await.unwrap();
        let l = CvLock::acquire(s.clone(), cv, "web", "a@h", "publish").await.unwrap();
        l.check().unwrap();
        // The laptop slept: tokio timers stood still, no renewal ran, and the row may be expired and taken over.
        l.ok_at.fetch_sub(LOCK_TTL_SECS, Ordering::Relaxed);
        let e = l.check().unwrap_err().to_string();
        let n: i64 = e.strip_prefix("lock on web lost: not renewed for ").and_then(|r| r.strip_suffix(" s")).expect(&e).parse().unwrap();
        assert!((LOCK_TTL_SECS..LOCK_TTL_SECS + 5).contains(&n), "{e}");
    }

    /// Pulp accepts the copy while another operator breaks the lock.
    struct LoseLockOnPost(Arc<Mutex<Option<String>>>);

    impl wiremock::Respond for LoseLockOnPost {
        fn respond(&self, _: &wiremock::Request) -> wiremock::ResponseTemplate {
            *self.0.lock().unwrap() = Some("taken over or broken by another operator".into());
            wiremock::ResponseTemplate::new(202).set_body_json(json!({ "task": "/pulp/api/v3/tasks/1/" }))
        }
    }

    const DERIVED: &str = "/pulp/api/v3/repositories/rpm/rpm/2/";
    const DIST: &str = "/pulp/api/v3/distributions/rpm/rpm/5/";

    /// A service against a mock Pulp, with an in-memory store.
    #[test]
    fn snapshots_get_the_publish_settings_of_their_source() {
        let src = json!({ "name": "os", "metadata_signing_service": "/s/1/", "checksum_type": "sha256", "repo_config": { "gpgcheck": 1 },
            "layout": "nested_alphabetically", "retain_repo_versions": 3 });
        let dst =
            json!({ "name": "web-os", "metadata_signing_service": null, "checksum_type": "sha256", "layout": "nested_alphabetically" });
        let patch = publish_settings(&src, &dst);
        assert_eq!(Value::Object(patch), json!({ "metadata_signing_service": "/s/1/", "repo_config": { "gpgcheck": 1 } }));
        assert!(publish_settings(&src, &src).is_empty(), "nothing to send when they agree");
        assert!(publish_settings(&json!({ "name": "old server" }), &dst).is_empty(), "fields the server lacks are never sent");
    }

    async fn mock_service(server: &wiremock::MockServer) -> CvService {
        let p = crate::config::Profile { url: server.uri(), auth: crate::config::AuthMethod::None, ..Default::default() };
        let client = PulpClient::new("t", &p, &crate::config::Settings::default()).unwrap();
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        CvService { client, store, scope: "k".into(), actor: "a@h".into(), lock: None }
    }

    #[test]
    fn deb_snapshots_get_the_signing_choice() {
        let src = json!({ "name": "apt", "signing_service": "/s/1/", "signing_service_release_overrides": { "bookworm": "/s/2/" } });
        let dst = json!({ "name": "web-apt", "signing_service": null, "signing_service_release_overrides": {} });
        let patch = |choice: &str, src: &Value, dst: &Value| Value::Object(deb_signing_patch(choice, src, dst));
        assert_eq!(
            patch("", &src, &dst),
            json!({ "signing_service": "/s/1/", "signing_service_release_overrides": { "bookworm": "/s/2/" } })
        );
        // Pulp changes only the per-release keys a PATCH names: a key the derived repository must lose is sent as null.
        let over = |m: Value| json!({ "signing_service": null, "signing_service_release_overrides": m });
        assert_eq!(
            patch("none", &src, &over(json!({ "bookworm": "/s/2/" }))),
            json!({ "signing_service_release_overrides": { "bookworm": null } })
        );
        assert_eq!(patch("none", &src, &src), over(json!({ "bookworm": null })));
        assert_eq!(
            patch("/s/3/", &src, &src),
            json!({ "signing_service": "/s/3/", "signing_service_release_overrides": { "bookworm": null } })
        );
        let dropped = over(json!({ "bookworm": "/s/2/", "bullseye": "/s/4/" }));
        assert_eq!(
            patch("", &src, &dropped),
            json!({ "signing_service": "/s/1/", "signing_service_release_overrides": { "bookworm": "/s/2/", "bullseye": null } }),
            "same as source removes a key the source dropped"
        );
        assert_eq!(patch("none", &src, &dst), json!({}), "nothing to send when the derived repository already has it");
        assert_eq!(patch("", &src, &src), json!({}));
        let old = json!({ "name": "old server", "signing_service": "/s/1/" });
        assert_eq!(patch("", &old, &dst), json!({ "signing_service": "/s/1/" }), "a key the source lacks is never sent");
        assert_eq!(patch("/s/3/", &old, &old), json!({ "signing_service": "/s/3/" }));
        assert_eq!(patch("none", &old, &old), json!({ "signing_service": null }));
    }

    #[tokio::test]
    async fn deb_selection_keeps_the_filtered_packages() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let mut repo = CvRepo { repo_href: "/r/".into(), repo_name: "apt".into(), repo_type: RepoType::Deb, filters: vec![] };
        let all = svc.select(&repo, "/r/versions/1/", false).await.unwrap();
        assert!(all.hrefs.is_none() && all.keys.is_empty(), "no filters: the copy takes everything, no listing");
        assert!(server.received_requests().await.unwrap().is_empty());
        let p = |href: &str, name: &str| json!({ "pulp_href": href, "package": name, "version": "1.0-1", "architecture": "amd64" });
        let pkgs = json!({ "count": 2, "next": null, "results": [p("/p/1/", "nginx"), p("/p/2/", "nginx-dbg")] });
        Mock::given(method("GET"))
            .and(path("/pulp/api/v3/content/deb/packages/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pkgs))
            .mount(&server)
            .await;
        let keys = svc.select(&repo, "/r/versions/1/", true).await.unwrap();
        assert_eq!(
            (keys.hrefs, keys.keys.into_iter().collect::<Vec<_>>()),
            (None, vec!["nginx-dbg_1.0-1_amd64".into(), "nginx_1.0-1_amd64".into()])
        );
        repo.filters = crate::filters::parse_rules("exclude name=*-dbg", RepoType::Deb).unwrap();
        let sel = svc.select(&repo, "/r/versions/1/", false).await.unwrap();
        assert_eq!((sel.hrefs, sel.keys.into_iter().collect::<Vec<_>>()), (Some(vec!["/p/1/".into()]), vec!["nginx_1.0-1_amd64".into()]));
    }

    /// The object at `href` carries the database ID label `db`.
    async fn serve_owner(server: &wiremock::MockServer, href: &str, db: &str) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(href))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({ "pulp_labels": { LABEL_DB: db } })))
            .mount(server)
            .await;
    }

    /// No request of this HTTP method reached the server.
    async fn assert_none(server: &wiremock::MockServer, method: &str) {
        let sent: Vec<_> = server.received_requests().await.unwrap().into_iter().filter(|r| r.method.as_str() == method).collect();
        assert!(sent.is_empty(), "unexpected {method}: {sent:?}");
    }

    async fn rpm_version(svc: &CvService) -> (i64, CvVersionRepo) {
        let cv = svc.store.create_cv("k", "web", "").await.unwrap();
        // RPM without filters: the copy is the first mutation (selection needs no content listing).
        let repo = CvRepo {
            repo_href: "/pulp/api/v3/repositories/rpm/rpm/1/".into(),
            repo_name: "os".into(),
            repo_type: RepoType::Rpm,
            filters: vec![],
        };
        let v = svc.store.begin_version(cv, "", &[repo]).await.unwrap();
        let mut vr = svc.store.version_repos(v.id).await.unwrap().remove(0);
        vr.source_version = Some("/pulp/api/v3/repositories/rpm/rpm/1/versions/1/".into());
        vr.derived_repo = Some(DERIVED.into());
        (cv, vr)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn task_sent_before_the_lock_is_lost_is_recorded() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (cv, mut vr) = rpm_version(&svc).await;
        let svc = svc.locked(cv, "publish").await.unwrap();
        serve_owner(&server, DERIVED, svc.store.db_id()).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/pulp/api/v3/rpm/copy/"))
            .respond_with(LoseLockOnPost(svc.lock.as_ref().unwrap().lost.clone()))
            .mount(&server)
            .await;
        let e = svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await.unwrap_err();
        assert!(e.to_string().starts_with("lock on web lost: "), "{e}");
        let saved = svc.store.version_repos(vr.version_id).await.unwrap().remove(0);
        assert_eq!(saved.pending_task.as_deref(), Some("/pulp/api/v3/tasks/1/"), "Resume re-attaches instead of copying again");
        let methods: Vec<_> = server.received_requests().await.unwrap().iter().map(|r| r.method.to_string()).collect();
        assert_eq!(methods, ["GET", "POST"], "label check, then the copy; stopped before waiting for the task");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_copy_task_is_sent_again_by_the_next_resume() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (_, mut vr) = rpm_version(&svc).await;
        vr.pending_task = Some("/pulp/api/v3/tasks/1/".into());
        svc.store.save_version_repo(&vr).await.unwrap();
        serve_owner(&server, DERIVED, svc.store.db_id()).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/pulp/api/v3/tasks/1/"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(json!({ "pulp_href": "/pulp/api/v3/tasks/1/", "state": "canceled", "name": "copy" })),
            )
            .mount(&server)
            .await;
        let e = svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await.unwrap_err();
        assert!(e.to_string().starts_with("task canceled"), "{e}");
        let saved = svc.store.version_repos(vr.version_id).await.unwrap().remove(0);
        assert_eq!(saved.pending_task, None, "Resume copies again instead of waiting on the dead task");
        assert_none(&server, "POST").await;
    }

    const DEB_DERIVED: &str = "/pulp/api/v3/repositories/deb/apt/2/";

    /// A Deb row after its copy: the derived version is recorded, the publication is not. Neither repository
    /// has signing fields, so the signing step sends nothing.
    async fn copied_deb_version(server: &wiremock::MockServer, svc: &CvService) -> (i64, CvVersionRepo) {
        let src = "/pulp/api/v3/repositories/deb/apt/1/";
        let cv = svc.store.create_cv("k", "web", "").await.unwrap();
        let repo = CvRepo { repo_href: src.into(), repo_name: "apt".into(), repo_type: RepoType::Deb, filters: vec![] };
        let v = svc.store.begin_version(cv, "", &[repo]).await.unwrap();
        let mut vr = svc.store.version_repos(v.id).await.unwrap().remove(0);
        vr.source_version = Some(format!("{src}versions/1/"));
        vr.derived_repo = Some(DEB_DERIVED.into());
        vr.derived_version = Some(format!("{DEB_DERIVED}versions/1/"));
        svc.store.save_version_repo(&vr).await.unwrap();
        serve_owner(server, DEB_DERIVED, svc.store.db_id()).await;
        serve_owner(server, src, svc.store.db_id()).await;
        (cv, vr)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_deb_publish_never_reuses_and_is_recorded_before_the_wait() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (cv, mut vr) = copied_deb_version(&server, &svc).await;
        let svc = svc.locked(cv, "publish").await.unwrap();
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/pulp/api/v3/publications/deb/apt/"))
            .respond_with(LoseLockOnPost(svc.lock.as_ref().unwrap().lost.clone()))
            .mount(&server)
            .await;
        let e = svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await.unwrap_err();
        assert!(e.to_string().starts_with("lock on web lost: "), "{e}");
        let saved = svc.store.version_repos(vr.version_id).await.unwrap().remove(0);
        assert_eq!((saved.pending_task.as_deref(), saved.publication), (Some("/pulp/api/v3/tasks/1/"), None), "Resume re-attaches");
        let sent = server.received_requests().await.unwrap();
        assert!(
            sent.iter().all(|r| r.method.as_str() == "POST" || r.url.path() != "/pulp/api/v3/publications/deb/apt/"),
            "no reuse lookup"
        );
        let posts: Vec<Value> = sent.iter().filter(|r| r.method.as_str() == "POST").map(|r| r.body_json().unwrap()).collect();
        assert_eq!(posts, [json!({ "repository_version": format!("{DEB_DERIVED}versions/1/") })], "the derived repository's keys apply");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_signing_service_needs_the_repository_field() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (_, mut vr) = copied_deb_version(&server, &svc).await;
        vr.signing = "/pulp/api/v3/signing-services/1/".into();
        let e = svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await.unwrap_err();
        assert_eq!(e.to_string(), "not supported by this server: signing_service on Deb repositories", "never published unsigned");
        assert_none(&server, "POST").await;
        vr.signing = "none".into(); // nothing to clear: the publish goes on
        let e = svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await.unwrap_err();
        assert!(!e.to_string().contains("signing_service"), "{e}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_signing_service_says_how_to_go_on() {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (_, mut vr) = copied_deb_version(&server, &svc).await;
        let derived =
            json!({ "pulp_labels": { LABEL_DB: svc.store.db_id() }, "signing_service": null, "signing_service_release_overrides": {} });
        let ok = wiremock::ResponseTemplate::new(200).set_body_json(derived);
        wiremock::Mock::given(method("GET")).and(path(DEB_DERIVED)).respond_with(ok).with_priority(1).mount(&server).await;
        let refused = json!({ "signing_service": ["Invalid hyperlink - Object does not exist."] });
        let refused = wiremock::ResponseTemplate::new(400).set_body_json(refused);
        wiremock::Mock::given(method("PATCH")).and(path(DEB_DERIVED)).respond_with(refused).mount(&server).await;
        vr.signing = "/pulp/api/v3/signing-services/1/".into();
        let e = format!("{:#}", svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await.unwrap_err());
        let want = "signing service /pulp/api/v3/signing-services/1/ was refused by the server; \
                    delete this Content View version and publish again with another choice: rejected [400]";
        assert!(e.starts_with(want), "{e}");
        assert_none(&server, "POST").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_re_attaches_to_the_deb_publish_task() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (_, mut vr) = copied_deb_version(&server, &svc).await;
        vr.pending_task = Some("/pulp/api/v3/tasks/9/".into());
        svc.store.save_version_repo(&vr).await.unwrap();
        let p = "/pulp/api/v3/publications/deb/apt/5/";
        let done = json!({ "pulp_href": "/pulp/api/v3/tasks/9/", "state": "completed", "created_resources": [p] });
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/pulp/api/v3/tasks/9/"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(done))
            .mount(&server)
            .await;
        svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await.unwrap();
        let saved = svc.store.version_repos(vr.version_id).await.unwrap().remove(0);
        assert_eq!((saved.publication.as_deref(), saved.pending_task), (Some(p), None));
        assert_none(&server, "POST").await;
        assert_none(&server, "PATCH").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_does_not_copy_into_a_repository_of_another_database() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (_, mut vr) = rpm_version(&svc).await;
        serve_owner(&server, DERIVED, "other").await;
        let e = svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await.unwrap_err();
        assert_eq!(e.to_string(), "repository pulp-tui-cv-web-os is owned by another pulp-tui database (other)");
        assert_none(&server, "POST").await;
        assert_none(&server, "PATCH").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn publish_refuses_a_snapshot_repository_as_source() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let cv = svc.store.create_cv("k", "outer", "").await.unwrap();
        let r = CvRepo { repo_href: DERIVED.into(), repo_name: "pulp-tui-cv-web-os".into(), repo_type: RepoType::Rpm, filters: vec![] };
        svc.store.set_cv_repo(cv, &r).await.unwrap();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(DERIVED))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({ "pulp_href": DERIVED, "name": "pulp-tui-cv-web-os",
                "latest_version_href": format!("{DERIVED}versions/1/"), "pulp_labels": { LABEL_CV: "web" } })))
            .mount(&server)
            .await;
        let e = svc.publish(cv, "", "", &|_| {}, &ops::no_progress).await.unwrap_err();
        assert_eq!(
            e.to_string(),
            "pulp-tui-cv-web-os is a snapshot repository of content view web; remove it from outer and add its source"
        );
        assert!(svc.store.versions(cv).await.unwrap().is_empty(), "refused before any write");
        assert_none(&server, "POST").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_version_keeps_objects_of_a_repository_of_another_database() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (_, mut vr) = rpm_version(&svc).await;
        vr.publication = Some("/pulp/api/v3/publications/rpm/rpm/3/".into());
        vr.derived_version = Some(format!("{DERIVED}versions/1/"));
        svc.store.save_version_repo(&vr).await.unwrap();
        serve_owner(&server, DERIVED, "other").await;
        let logs = Mutex::new(vec![]);
        svc.delete_version(vr.version_id, &|m| logs.lock().unwrap().push(m), &ops::no_progress).await.unwrap();
        assert_none(&server, "DELETE").await;
        let logs = logs.into_inner().unwrap();
        let kept = |h: &str| format!("keeping {h}: owned by another pulp-tui database (other)");
        assert_eq!(logs, [kept(vr.publication.as_deref().unwrap()), kept(vr.derived_version.as_deref().unwrap())]);
        assert_eq!(svc.store.version(vr.version_id).await.unwrap().state, "deleted");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unbind_does_not_touch_a_distribution_of_another_database() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let cv = svc.store.create_cv("k", "web", "").await.unwrap();
        // A version without repositories: the bound repository "old" is no longer part of it.
        let v = svc.store.begin_version(cv, "", &[]).await.unwrap();
        svc.store.finish_version(v.id, "published", None).await.unwrap();
        let lib = svc.store.ensure_library("k").await.unwrap();
        let old = BindingRepo {
            env_id: lib,
            cv_id: cv,
            repo_href: "/pulp/api/v3/repositories/rpm/rpm/9/".into(),
            repo_name: "old".into(),
            repo_type: RepoType::Rpm,
            distribution: DIST.into(),
            base_path: "cv/library/web/old".into(),
            publication: Some("/pulp/api/v3/publications/rpm/rpm/3/".into()),
        };
        svc.store.save_binding_repo(&old).await.unwrap();
        serve_owner(&server, DIST, "other").await;
        let e = svc.promote(cv, v.id, lib, Mode::Rollback, &|_| {}, &ops::no_progress).await.unwrap_err();
        let want =
            format!("unbind old: distribution {} is owned by another pulp-tui database (other)", distribution_name("web", LIBRARY, "old"));
        assert!(e.to_string().ends_with(&want), "{e}");
        assert_none(&server, "PATCH").await;
        assert_eq!(svc.store.binding_repos(lib, cv).await.unwrap()[0].publication, old.publication, "still recorded as served");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_unbind_alone_makes_the_promotion_partial() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let cv = svc.store.create_cv("k", "web", "").await.unwrap();
        let v = published(&svc, cv, "os", "/pulp/api/v3/repositories/rpm/rpm/1/").await;
        let publication = svc.store.version_repos(v.id).await.unwrap()[0].publication.clone();
        let lib = svc.store.ensure_library("k").await.unwrap();
        // "os" is already served as wanted: the bind sends nothing.
        let os = json!({ "pulp_href": "/pulp/api/v3/distributions/rpm/rpm/6/", "name": distribution_name("web", LIBRARY, "os"),
            "base_path": "library/web/os", "publication": publication,
            "pulp_labels": { LABEL_CV: "web", LABEL_ENV: LIBRARY, LABEL_DB: svc.store.db_id() } });
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/pulp/api/v3/distributions/rpm/rpm/"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({ "count": 1, "next": null, "results": [os] })))
            .mount(&server)
            .await;
        // "old" is no longer part of the version, and its distribution belongs to another database.
        let old = BindingRepo {
            env_id: lib,
            cv_id: cv,
            repo_href: "/pulp/api/v3/repositories/rpm/rpm/9/".into(),
            repo_name: "old".into(),
            repo_type: RepoType::Rpm,
            distribution: DIST.into(),
            base_path: "library/web/old".into(),
            publication: Some("/pulp/api/v3/publications/rpm/rpm/3/".into()),
        };
        svc.store.save_binding_repo(&old).await.unwrap();
        serve_owner(&server, DIST, "other").await;
        let e = svc.promote(cv, v.id, lib, Mode::Rollback, &|_| {}, &ops::no_progress).await.unwrap_err();
        assert!(e.to_string().starts_with("web in Library: partial: unbind old"), "{e}");
        assert_eq!(svc.store.binding(lib, cv).await.unwrap().unwrap().state, "partial", "os is served: not failed");
    }

    /// Scope "k": Library, Dev (label "dev"), view "web" with a published version bound to Dev.
    async fn web_in_dev(svc: &CvService) -> (i64, i64) {
        let lib = svc.store.ensure_library("k").await.unwrap();
        let dev = svc.store.create_env("k", "Dev", lib, false).await.unwrap();
        let cv = svc.store.create_cv("k", "web", "").await.unwrap();
        let v = svc.store.begin_version(cv, "", &[]).await.unwrap();
        svc.store.finish_version(v.id, "published", None).await.unwrap();
        svc.store.set_binding(dev, cv, v.id, "active").await.unwrap();
        (dev, cv)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relabel_moves_owned_rows_and_keeps_failed_ones() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (dev, cv) = web_in_dev(&svc).await;
        let owned = json!({ LABEL_CV: "web", LABEL_ENV: "Dev", LABEL_DB: svc.store.db_id() });
        // (repo, stored path, what Pulp serves and its labels; None: not requested)
        let rows = [
            ("os", "cv/dev/web/os", Some(("cv/dev/web/os", owned.clone()))),
            ("docs", "qa/web/docs", None),
            ("pkgs", "cv/dev/web/pkgs", Some(("qa/web/pkgs", owned.clone()))), // PATCH sent, row not saved
            ("tools", "cv/dev/web/tools", Some(("cv/dev/web/tools", json!({})))),
            // Our cv/env labels, but the database id of another database (not ours, not an alias).
            ("mirror", "cv/dev/web/mirror", Some(("cv/dev/web/mirror", json!({ LABEL_CV: "web", LABEL_ENV: "Dev", LABEL_DB: "other" })))),
        ];
        for (i, (repo, stored, served)) in rows.iter().enumerate() {
            let href = format!("/pulp/api/v3/distributions/rpm/rpm/{i}/");
            let b = BindingRepo {
                env_id: dev,
                cv_id: cv,
                repo_href: format!("/pulp/api/v3/repositories/rpm/rpm/{i}/"),
                repo_name: repo.to_string(),
                repo_type: RepoType::Rpm,
                distribution: href.clone(),
                base_path: stored.to_string(),
                publication: None,
            };
            svc.store.save_binding_repo(&b).await.unwrap();
            if let Some((bp, labels)) = served {
                let d = json!({ "pulp_href": href, "name": distribution_name("web", "Dev", repo), "base_path": bp, "pulp_labels": labels });
                Mock::given(method("GET")).and(path(href)).respond_with(ResponseTemplate::new(200).set_body_json(d)).mount(&server).await;
            }
        }
        Mock::given(method("PATCH")).respond_with(ResponseTemplate::new(200).set_body_json(json!({}))).mount(&server).await;
        let patches = || async {
            let all = server.received_requests().await.unwrap();
            all.into_iter()
                .filter(|r| r.method.as_str() == "PATCH")
                .map(|r| (r.url.path().to_string(), r.body_json::<Value>().unwrap()))
                .collect::<Vec<_>>()
        };

        let (lines, failed) = svc.relabel_env(dev, "qa", &|_| {}, &ops::no_progress).await.unwrap();
        assert_eq!(failed, 2, "{lines:?}");
        assert_eq!(patches().await, [("/pulp/api/v3/distributions/rpm/rpm/0/".to_string(), json!({ "base_path": "qa/web/os" }))]);
        let paths: Vec<_> = svc.store.binding_repos(dev, cv).await.unwrap().into_iter().map(|b| b.base_path).collect();
        assert_eq!(paths, ["qa/web/docs", "cv/dev/web/mirror", "qa/web/os", "qa/web/pkgs", "cv/dev/web/tools"]);
        assert!(lines.contains(&"moved os of web: /cv/dev/web/os -> /qa/web/os".to_string()), "{lines:?}");
        let tools = lines.iter().find(|l| l.starts_with("FAILED tools of web:")).expect("tools failed");
        assert!(tools.contains("lacks ownership labels") && tools.ends_with("(still at /cv/dev/web/tools)"), "{tools}");
        let mirror = lines.iter().find(|l| l.starts_with("FAILED mirror of web:")).expect("mirror failed");
        assert!(
            mirror.contains("owned by another pulp-tui database (other)") && mirror.ends_with("(still at /cv/dev/web/mirror)"),
            "{mirror}"
        );
        assert_eq!(svc.store.environments("k").await.unwrap()[1].label, "qa");

        // A second run with the same label sends no PATCH (os/pkgs/docs are skipped) and reports tools and mirror again.
        let (_, failed) = svc.relabel_env(dev, "qa", &|_| {}, &ops::no_progress).await.unwrap();
        assert_eq!(failed, 2);
        assert_eq!(patches().await.len(), 1);
        let audit: Vec<_> = svc.store.audit_log("k", 10).await.unwrap().into_iter().filter(|a| a.action == "env.relabel").collect();
        assert_eq!(audit.len(), 1, "the rerun changed nothing: {audit:?}");
        assert_eq!(audit[0].detail, "Dev: dev -> qa: 2 moved, 2 failed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relabel_refuses_while_a_view_is_locked() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (dev, cv) = web_in_dev(&svc).await;
        let _held = CvLock::acquire(svc.store.clone(), cv, "web", "b@h", "publish").await.unwrap();
        let e = svc.relabel_env(dev, "qa", &|_| {}, &ops::no_progress).await.unwrap_err();
        assert!(e.to_string().starts_with("Content View web is locked by b@h (publish) since "), "{e}");
        assert_eq!(svc.store.environments("k").await.unwrap()[1].label, "dev");
        assert_none(&server, "PATCH").await;
    }

    /// A published version of `cv` with one RPM row (derived repository DERIVED, a version and a publication).
    async fn published(svc: &CvService, cv: i64, repo: &str, href: &str) -> CvVersion {
        let r = CvRepo { repo_href: href.into(), repo_name: repo.into(), repo_type: RepoType::Rpm, filters: vec![] };
        let v = svc.store.begin_version(cv, "", std::slice::from_ref(&r)).await.unwrap();
        let vr = CvVersionRepo {
            version_id: v.id,
            repo: Some(r),
            derived_repo: Some(DERIVED.into()),
            derived_version: Some(format!("{DERIVED}versions/{}/", v.number)),
            publication: Some(format!("/pulp/api/v3/publications/rpm/rpm/{}/", v.id)),
            ..Default::default()
        };
        svc.store.save_version_repo(&vr).await.unwrap();
        svc.store.finish_version(v.id, "published", None).await.unwrap();
        svc.store.version(v.id).await.unwrap()
    }

    #[test]
    fn repo_clash_rules() {
        let r = |n: &str, h: &str| CvRepo { repo_href: h.into(), repo_name: n.into(), repo_type: RepoType::Rpm, filters: vec![] };
        let e = |p: &[(String, Vec<CvRepo>)]| check_repo_clash(p).unwrap_err().to_string();
        check_repo_clash(&[("os".into(), vec![r("baseos", "/1/")]), ("extras".into(), vec![r("docker", "/2/")])]).unwrap();
        assert_eq!(
            e(&[("os".into(), vec![r("baseos", "/1/")]), ("extras".into(), vec![r("baseos", "/1/")])]),
            "components os and extras both contain repository baseos"
        );
        assert_eq!(
            e(&[("os".into(), vec![r("base os", "/1/")]), ("extras".into(), vec![r("base-os", "/3/")])]),
            "repositories base os (os) and base-os (extras) would be served at the same path segment base-os"
        );
        assert!(check_repo_clash(&[("web".into(), vec![r("base os", "/1/"), r("base-os", "/3/")])]).is_err(), "a normal view too");
    }

    #[test]
    fn component_lines_show_changes() {
        let u = |n: &str, v: i64| UsedVersion { cv_id: 0, name: n.into(), version_id: v, number: v };
        assert!(component_lines(&[], &[]).is_empty());
        assert_eq!(
            component_lines(&[u("a", 1), u("b", 2), u("c", 1)], &[u("a", 1), u("b", 3), u("d", 4)]),
            ["== components", "  a: v1 (unchanged)", "  b: v2 -> v3", "  c: removed (was v1)", "  d: added (v4)"]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn latest_and_pinned_resolution() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let s = &svc.store;
        let href = "/pulp/api/v3/repositories/rpm/rpm/1/";
        let os = s.create_cv("k", "os", "").await.unwrap();
        let v1 = published(&svc, os, "baseos", href).await;
        published(&svc, os, "baseos", href).await;
        let v3 = s.begin_version(os, "", &[]).await.unwrap();
        s.finish_version(v3.id, "failed", None).await.unwrap();
        let extras = s.create_cv("k", "extras", "").await.unwrap();
        let c = s.create_composite("k", "rocky9", "").await.unwrap();
        let cv = s.content_view(c).await.unwrap();
        let resolved = || async {
            let parts = svc.resolve_components(&cv).await?;
            anyhow::Ok(parts.into_iter().map(|(c, v, _)| (c.name, v.number)).collect::<Vec<_>>())
        };
        assert_eq!(resolved().await.unwrap_err().to_string(), "composite rocky9 has no components");
        s.set_component(c, os, None).await.unwrap();
        assert_eq!(resolved().await.unwrap(), [("os".to_string(), 2)], "latest skips the failed v3");
        s.set_component(c, os, Some(v1.id)).await.unwrap();
        assert_eq!(resolved().await.unwrap(), [("os".to_string(), 1)]);
        s.set_component(c, extras, None).await.unwrap();
        assert_eq!(resolved().await.unwrap_err().to_string(), "component extras has no published version");
        s.remove_component(c, extras).await.unwrap();

        // Another view now holds the same repository (definitions change after a component is added).
        let dup = s.create_cv("k", "dup", "").await.unwrap();
        published(&svc, dup, "baseos", href).await;
        s.set_component(c, dup, None).await.unwrap();
        let e = svc.publish(c, "", "", &|_| {}, &ops::no_progress).await.unwrap_err();
        assert_eq!(e.to_string(), "components dup and os both contain repository baseos");
        assert!(s.versions(c).await.unwrap().is_empty(), "refused before any write");
        s.remove_component(c, dup).await.unwrap();

        s.finish_version(v1.id, "deleted", None).await.unwrap();
        assert_eq!(resolved().await.unwrap_err().to_string(), "pinned version v1 of os is deleted");

        // A normal view: two names that slug to one path segment are refused before the snapshot.
        let web = s.create_cv("k", "web", "").await.unwrap();
        for (n, h) in [("base os", "/pulp/api/v3/repositories/rpm/rpm/3/"), ("base-os", "/pulp/api/v3/repositories/rpm/rpm/4/")] {
            let r = CvRepo { repo_href: h.into(), repo_name: n.into(), repo_type: RepoType::Rpm, filters: vec![] };
            s.set_cv_repo(web, &r).await.unwrap();
        }
        let e = svc.publish(web, "", "", &|_| {}, &ops::no_progress).await.unwrap_err();
        assert_eq!(e.to_string(), "repositories base os (web) and base-os (web) would be served at the same path segment base-os");
        assert!(s.versions(web).await.unwrap().is_empty());
        assert!(server.received_requests().await.unwrap().is_empty(), "no Pulp call");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn promote_keeps_the_path_of_a_moved_distribution() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let cv = svc.store.create_cv("k", "web", "").await.unwrap();
        let v = published(&svc, cv, "os", "/pulp/api/v3/repositories/rpm/rpm/1/").await;
        let lib = svc.store.ensure_library("k").await.unwrap();
        let p = svc.store.version_repos(v.id).await.unwrap()[0].publication.clone();
        // Moved by an operator; it serves the version already, so promote changes nothing in Pulp.
        let labels = json!({ LABEL_CV: "web", LABEL_ENV: LIBRARY, LABEL_DB: svc.store.db_id() });
        let d = json!({ "pulp_href": DIST, "name": distribution_name("web", LIBRARY, "os"), "base_path": "custom/os", "publication": p, "pulp_labels": labels });
        Mock::given(method("GET"))
            .and(path("/pulp/api/v3/distributions/rpm/rpm/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "count": 1, "next": null, "results": [d] })))
            .mount(&server)
            .await;
        svc.promote(cv, v.id, lib, Mode::Promote, &|_| {}, &ops::no_progress).await.unwrap();
        assert_none(&server, "PATCH").await;
        assert_none(&server, "POST").await;
        assert_eq!(svc.store.binding_repos(lib, cv).await.unwrap()[0].base_path, "custom/os");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn composite_publish_copies_nothing_and_serves_at_its_own_path() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let os = svc.store.create_cv("k", "os", "").await.unwrap();
        let v1 = published(&svc, os, "os", "/pulp/api/v3/repositories/rpm/rpm/1/").await;
        let own = svc.store.version_repos(v1.id).await.unwrap();
        let p = own[0].publication.clone().unwrap();
        let c = svc.store.create_composite("k", "rocky9", "").await.unwrap();
        svc.store.set_component(c, os, None).await.unwrap();
        let empty = json!({ "count": 0, "next": null, "results": [] });
        for p in ["/pulp/api/v3/distributions/rpm/rpm/", "/pulp/api/v3/distributions/"] {
            Mock::given(method("GET")).and(path(p)).respond_with(ResponseTemplate::new(200).set_body_json(&empty)).mount(&server).await;
        }
        let t = "/pulp/api/v3/tasks/7/";
        Mock::given(method("POST"))
            .and(path("/pulp/api/v3/distributions/rpm/rpm/"))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({ "task": t })))
            .mount(&server)
            .await;
        let done = json!({ "pulp_href": t, "state": "completed", "created_resources": [DIST] });
        Mock::given(method("GET")).and(path(t)).respond_with(ResponseTemplate::new(200).set_body_json(done)).mount(&server).await;

        let v = svc.publish(c, "", "", &|_| {}, &ops::no_progress).await.unwrap();
        assert_eq!((v.number, v.state.as_str()), (1, "published"));
        let posts: Vec<_> = server.received_requests().await.unwrap().into_iter().filter(|r| r.method.as_str() == "POST").collect();
        assert_eq!(
            posts.iter().map(|r| r.url.path()).collect::<Vec<_>>(),
            ["/pulp/api/v3/distributions/rpm/rpm/"],
            "no repository, copy or publication"
        );
        let body: Value = posts[0].body_json().unwrap();
        assert_eq!(body["name"], "pulp-tui-cv-rocky9-Library-os");
        assert_eq!(body["base_path"], "library/rocky9/os");
        assert_eq!(body["publication"], p.as_str());
        assert_eq!(body["pulp_labels"][LABEL_CV], "rocky9");
        assert_none(&server, "PATCH").await;
        assert_none(&server, "DELETE").await;
        let rows = svc.store.version_repos(v.id).await.unwrap();
        assert!(rows.len() == 1 && rows[0].borrowed && rows[0].publication.as_deref() == Some(p.as_str()), "{rows:?}");
        assert_eq!(svc.store.version_components(v.id).await.unwrap().iter().map(|u| u.name.as_str()).collect::<Vec<_>>(), ["os"]);
        assert_eq!(svc.store.version_repos(v1.id).await.unwrap(), own, "the component version is not changed");
        let lib = svc.store.ensure_library("k").await.unwrap();
        assert_eq!(svc.store.binding(lib, c).await.unwrap().unwrap().state, "active");
        let served = svc.store.binding_repos(lib, c).await.unwrap();
        assert_eq!((served[0].distribution.as_str(), served[0].base_path.as_str()), (DIST, "library/rocky9/os"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deletes_keep_borrowed_objects_and_refuse_used_components() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let os = svc.store.create_cv("k", "os", "").await.unwrap();
        let v1 = published(&svc, os, "os", "/pulp/api/v3/repositories/rpm/rpm/1/").await;
        let c = svc.store.create_composite("k", "rocky9", "").await.unwrap();
        svc.store.set_component(c, os, None).await.unwrap();
        let rows = svc.store.version_repos(v1.id).await.unwrap();
        let cv1 = svc.store.begin_composite_version(c, "", &[v1.id], &rows).await.unwrap();
        svc.store.finish_version(cv1.id, "published", None).await.unwrap();
        let err = |r: Result<()>| r.unwrap_err().to_string();

        assert_eq!(err(svc.delete_version(v1.id, &|_| {}, &ops::no_progress).await), "version 1 is used by rocky9 v1");
        assert_eq!(svc.store.version(v1.id).await.unwrap().state, "published", "a refused delete keeps the state");
        // While a delete of v1 runs, a composite publish does not borrow it and Resume does not publish it again.
        assert!(svc.store.set_version_state(v1.id, "published", "deleting").await.unwrap());
        let e = svc.store.begin_composite_version(c, "", &[v1.id], &rows).await.unwrap_err();
        assert_eq!(e.to_string(), "version v1 of os is no longer published");
        assert_eq!(
            err(svc.resume(v1.id, &|_| {}, &ops::no_progress).await.map(drop)),
            "version 1 is being deleted; delete it again to finish"
        );
        assert!(svc.store.set_version_state(v1.id, "deleting", "published").await.unwrap());
        let e = err(svc.delete_view(os, &|_| {}, &ops::no_progress).await);
        assert!(e.starts_with("content view os is used by rocky9 (component), rocky9 v1"), "{e}");
        svc.delete_version(cv1.id, &|_| {}, &ops::no_progress).await.unwrap();
        assert_eq!(svc.store.version(cv1.id).await.unwrap().state, "deleted");
        svc.delete_view(c, &|_| {}, &ops::no_progress).await.unwrap();
        assert_none(&server, "DELETE").await;
        assert_none(&server, "GET").await; // borrowed rows are not even read
        assert_eq!(svc.store.version_repos(v1.id).await.unwrap(), rows, "the component keeps its rows");
        assert_eq!(svc.store.version(v1.id).await.unwrap().state, "published");
    }

    /// os v1 (publication P1) was used by rocky9 v1, now deleted; rocky9's Dev binding row still records P1
    /// (a later promotion could not re-point or unbind it). Returns os and the row of os v1.
    async fn served_by_composite(svc: &CvService) -> (i64, CvVersionRepo) {
        let s = &svc.store;
        let lib = s.ensure_library("k").await.unwrap();
        let dev = s.create_env("k", "Dev", lib, false).await.unwrap();
        let os = s.create_cv("k", "os", "").await.unwrap();
        let v1 = published(svc, os, "os", "/pulp/api/v3/repositories/rpm/rpm/1/").await;
        let row = s.version_repos(v1.id).await.unwrap().remove(0);
        let c = s.create_composite("k", "rocky9", "").await.unwrap();
        let cv1 = s.begin_composite_version(c, "", &[v1.id], std::slice::from_ref(&row)).await.unwrap();
        s.finish_version(cv1.id, "deleted", None).await.unwrap();
        let b = BindingRepo {
            env_id: dev,
            cv_id: c,
            repo_href: row.repo().repo_href.clone(),
            repo_name: "os".into(),
            repo_type: RepoType::Rpm,
            distribution: DIST.into(),
            base_path: "cv/dev/rocky9/os".into(),
            publication: row.publication.clone(),
        };
        s.save_binding_repo(&b).await.unwrap();
        (os, row)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_version_keeps_a_publication_another_view_serves() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (_, row) = served_by_composite(&svc).await;
        serve_owner(&server, DERIVED, svc.store.db_id()).await;
        let logs = Mutex::new(vec![]);
        svc.delete_version(row.version_id, &|m| logs.lock().unwrap().push(m), &ops::no_progress).await.unwrap();
        assert_none(&server, "DELETE").await;
        let (p, dv) = (row.publication.unwrap(), row.derived_version.unwrap());
        // Pulp deletes a repository version's publications with it, so the version is kept too.
        assert_eq!(
            logs.into_inner().unwrap(),
            [format!("keeping {p}: still served by rocky9 in Dev"), format!("keeping {dv}: still served by rocky9 in Dev")]
        );
        assert_eq!(svc.store.version(row.version_id).await.unwrap().state, "deleted");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_view_refuses_while_another_view_serves_its_publication() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (os, _) = served_by_composite(&svc).await;
        let e = svc.delete_view(os, &|_| {}, &ops::no_progress).await.unwrap_err();
        assert_eq!(
            e.to_string(),
            "content view os: publication of os is still served by rocky9 in Dev; \
             promote rocky9 in Dev again (or roll it back) so it stops serving it, or delete rocky9 first"
        );
        assert_none(&server, "DELETE").await;
        assert_none(&server, "GET").await;
        assert_eq!(svc.store.content_view(os).await.unwrap().name, "os");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_publish_reports_a_locked_composite() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let s = &svc.store;
        let os = s.create_cv("k", "os", "").await.unwrap();
        // A published version without repositories: a composite of it publishes without any Pulp call.
        let v = s.begin_version(os, "", &[]).await.unwrap();
        s.finish_version(v.id, "published", None).await.unwrap();
        let extras = s.create_cv("k", "extras", "").await.unwrap();
        let comp = |name: &'static str, auto: bool, with_extras: bool| async move {
            let c = s.create_composite("k", name, "").await.unwrap();
            s.set_component(c, os, None).await.unwrap();
            if with_extras {
                s.set_component(c, extras, None).await.unwrap();
            }
            s.set_auto_publish(c, auto).await.unwrap();
            c
        };
        let broken = comp("broken", true, true).await;
        let quiet = comp("quiet", false, false).await;
        let rocky9 = comp("rocky9", true, false).await;
        let web = comp("web", true, false).await;
        let _held = CvLock::acquire(s.clone(), rocky9, "rocky9", "b@h", "publish").await.unwrap();

        let r = svc.auto_publish(os, &|_| {}, &ops::no_progress).await;
        assert_eq!(r.len(), 3, "{r:?}");
        assert_eq!(r[0], Err("broken: component extras has no published version".to_string()));
        assert!(r[1].as_ref().unwrap_err().starts_with("rocky9: Content View rocky9 is locked by b@h"), "{r:?}");
        assert_eq!(r[2], Ok("web v1 published".to_string()));
        assert_eq!(s.versions(web).await.unwrap()[0].notes, "auto publish after os v1");
        for c in [broken, quiet, rocky9] {
            assert!(s.versions(c).await.unwrap().is_empty());
        }
        assert_eq!(s.version(v.id).await.unwrap().state, "published", "the component's publish stays");
    }

    #[test]
    fn conflict_message_shows_hours_and_minutes() {
        let h = LockHolder { owner: "alice@ws12".into(), operation: "promote to Prod".into(), acquired_at: "2026-10-23T14:02:33Z".into() };
        assert_eq!(conflict("web", &h), "Content View web is locked by alice@ws12 (promote to Prod) since 14:02 UTC");
    }
}
