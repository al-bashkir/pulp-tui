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
use crate::pulp::models::{DebPackage, Distribution, Package, Publication, RepoVersion, Repository, Task};
use crate::pulp::ops::{self, OnTask};
use crate::pulp::{Capabilities, Mutation, PulpClient, PulpError, RepoType, deb, file, rpm};
use crate::store::{
    Binding, BindingRepo, Component, ContentView, CvRepo, CvVersion, CvVersionRepo, Environment, Export, LIBRARY, LOCK_RENEW_SECS,
    LOCK_TTL_SECS, LockHolder, Store, UsedVersion, validate_label,
};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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

/// The state to show: a `publishing` version or `promoting` binding of a view without a live lock (`running`,
/// `Store::lock_live`) is `interrupted`: the client that ran it ended (quit, crash, Connect), so nothing continues it.
pub fn shown_state(state: &str, running: bool) -> &str {
    if !running && matches!(state, "publishing" | "promoting") { "interrupted" } else { state }
}

/// How binding `b` shows: the served version (`none` when there is none), then a promotion that has not completed,
/// as in `v1 (v2 failed)`. `number` gives the number of a version id.
pub fn binding_text(b: &Binding, number: impl Fn(i64) -> i64, running: bool, none: &str) -> String {
    let served = b.served.map_or_else(|| none.to_string(), |s| format!("v{}", number(s)));
    match &b.attempt {
        Some(a) => format!("{served} (v{} {})", number(a.version_id), shown_state(&a.state, running)),
        None => served,
    }
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

    /// Identifies this acquisition: rows that a workflow starts carry it, so a later write can tell its own rows.
    pub fn token(&self) -> &str {
        &self.token
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

/// The repository fields that a publication of type `t` takes from its repository when the request does not
/// set them (Deb: `deb_signing_patch`).
fn publish_fields(t: RepoType) -> &'static [&'static str] {
    match t {
        // Not package_signing_*: pulp_rpm signs the packages that `modify/` adds, so the snapshot would get other packages.
        RepoType::Rpm => &["metadata_signing_service", "checksum_type", "repo_config", "compression_type", "layout"],
        RepoType::File => &["manifest"], // sent with each publication request (`ops::publication_body`)
        RepoType::Deb => &[],
    }
}

/// The PATCH that gives a derived repository the publish settings of its source (empty: they agree), so a
/// view of a signed repository serves signed metadata, and a File view serves the manifest name of its
/// source. Fields this server does not have are never sent.
fn publish_settings(t: RepoType, source: &Value, derived: &Value) -> serde_json::Map<String, Value> {
    publish_fields(t)
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

/// What an RPM filter takes from `version` (packages `pkgs`) besides the `chosen` packages: each advisory whose
/// packages in the version are all chosen, and the other content (comps, modules, distribution trees, repo metadata
/// files) as it is. Add these with `modify/`: the copy API would also add what advisories and comps groups name.
pub async fn rpm_metadata(c: &PulpClient, version: &str, pkgs: &[Package], chosen: &[&Package]) -> Result<Vec<String>> {
    let nevra = |p: &Package| (p.name.clone(), norm_epoch(&p.epoch), p.version.clone(), p.release.clone(), p.arch.clone());
    let chosen_nevra: HashSet<_> = chosen.iter().map(|p| nevra(p)).collect();
    let all_nevra: HashSet<_> = pkgs.iter().map(nevra).collect();
    let q = [("repository_version", version.to_string()), ("fields", "pulp_href,pkglist".into())];
    let advisories: Vec<Value> = c.list_all(&c.api("content/rpm/advisories/"), &q, 1_000_000).await?;
    let mut hrefs = vec![];
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
    for h in ops::content_in(c, version).await? {
        if !h.contains("/content/rpm/packages/") && !h.contains("/content/rpm/advisories/") {
            hrefs.push(h);
        }
    }
    Ok(hrefs)
}

pub fn derived_repo_name(cv: &str, repo: &str) -> String {
    format!("pulp-tui-cv-{}-{}", slug(cv), slug(repo))
}

pub fn distribution_name(cv: &str, env: &str, repo: &str) -> String {
    format!("pulp-tui-cv-{}-{}-{}", slug(cv), slug(env), slug(repo))
}

/// The ownership labels of the distributions of view `cv` in environment `env` (a derived repository has `LABEL_CV` only).
fn dist_labels(cv: &str, env: &str) -> Value {
    json!({ LABEL_CV: slug(cv), LABEL_ENV: slug(env) })
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
    /// An earlier published version (`rollback_refusal`); in Library any published version.
    Rollback,
}

/// Why a rollback of version `number` in `env` (not Library) is refused; `active` is the number bound there.
/// A newer version, or a first one, must come through Promote: it would skip the promotion path. The active
/// version itself is allowed: a retry of a partial rollback, or a drift repair.
pub fn rollback_refusal(env: &str, number: i64, active: Option<i64>) -> Option<String> {
    match active {
        None => Some(format!("no version is active in {env}; use Promote")),
        Some(a) if number > a => Some(format!("v{number} is newer than the active v{a} in {env}; use Promote")),
        Some(_) => None,
    }
}

/// `export-cv` of the store `scope` of server `c`. A view records a repository's name when it is added, so each
/// repository gets its current name on `c` (`import` finds it by name; the store is not changed: derived object
/// names come from the recorded one). When a repository is gone, the error names each gone one.
pub async fn export(c: &PulpClient, store: &Store, scope: &str) -> Result<Export> {
    let mut ex = store.export(scope).await?;
    let (mut names, mut gone) = (HashMap::new(), vec![]);
    for v in &mut ex.content_views {
        for r in &mut v.repos {
            if !names.contains_key(&r.repo_href) {
                let name = match c.get::<Value>(&r.repo_href, &[("fields", "name".into())]).await {
                    Ok(x) => x["name"].as_str().map(str::to_string),
                    Err(e) if e.is_not_found() => None,
                    Err(e) => Err(e).with_context(|| format!("repository {}", r.repo_name))?,
                };
                names.insert(r.repo_href.clone(), name);
            }
            match &names[&r.repo_href] {
                Some(n) => r.repo_name = n.clone(),
                None => gone.push(format!("{} ({}, view {})", r.repo_name, r.repo_type.label(), v.name)),
            }
        }
    }
    if !gone.is_empty() {
        bail!("repositories not found on this server: {}; nothing was exported", gone.join(", "));
    }
    Ok(ex)
}

/// `import-cv` into the store `scope` of server `c`. An export holds the repository hrefs of its own server, so each
/// repository of a view that the import creates gets the href of the repository with its name and type on `c`.
/// When one is missing, nothing is written and the error names each missing one. Returns what `Store::import` returns.
pub async fn import(c: &PulpClient, caps: &Capabilities, store: &Store, scope: &str, ex: &Export) -> Result<(usize, usize, Vec<String>)> {
    let kept: HashSet<String> = store.content_views(scope).await?.into_iter().map(|v| v.name).collect();
    let (mut ex, mut missing) = (ex.clone(), vec![]);
    for v in ex.content_views.iter_mut().filter(|v| !kept.contains(&v.name)) {
        for r in &mut v.repos {
            let t = r.repo_type;
            let found = if caps.supports(t.feature()) {
                ops::find_by_name::<Repository>(c, &c.api(t.repositories()), &r.repo_name).await?
            } else {
                None
            };
            match found {
                Some(x) => r.repo_href = x.pulp_href,
                None => missing.push(format!("{} ({}, view {})", r.repo_name, t.label(), v.name)),
            }
        }
    }
    if !missing.is_empty() {
        bail!("repositories not found on this server (by name and type): {}; nothing was imported", missing.join(", "));
    }
    store.import(scope, &ex).await
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

    /// The token of the lock this workflow holds (`CvLock::token`); "" without one.
    fn token(&self) -> &str {
        self.lock.as_ref().map_or("", |l| l.token())
    }

    /// Why the object at `href` is not ours: another pulp-tui database owns it (database ID label), or it lacks
    /// one of the ownership `labels` (`{key: value}`). `None` when it is ours (also with no database ID label, or
    /// that of an alias) or gone. Call it before acting on a recorded href.
    async fn not_ours(&self, href: &str, labels: &Value, aliases: &[String]) -> Result<Option<String>> {
        #[derive(serde::Deserialize)]
        struct Labeled {
            #[serde(default)]
            pulp_labels: BTreeMap<String, String>,
        }
        let have = match self.client.get::<Labeled>(href, &[]).await {
            Ok(o) => o.pulp_labels,
            Err(e) if e.is_not_found() => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if let Err(o) = ops::label_owner(&have, LABEL_DB, self.store.db_id(), aliases) {
            return Ok(Some(format!("owned by another pulp-tui database ({o})")));
        }
        let lacks = labels.as_object().into_iter().flatten().find(|(k, v)| have.get(*k).map(String::as_str) != v.as_str());
        Ok(lacks.map(|(k, v)| format!("not labeled {k}={}", v.as_str().unwrap_or_default())))
    }

    /// The derived repositories of the view's own (not borrowed) version rows (href -> source repository). Refused
    /// while another view serves one of their publications: deleting the repository deletes its publications.
    async fn snapshot_repos(&self, cv: &ContentView) -> Result<BTreeMap<String, CvRepo>> {
        let served = self.store.served_publications(&self.scope).await?;
        let mut repos = BTreeMap::new();
        for v in self.store.versions(cv.id).await? {
            for vr in self.store.version_repos(v.id).await?.into_iter().filter(|vr| !vr.borrowed) {
                if let Some((_, other, env)) = served.iter().find(|(p, o, _)| *o != cv.name && vr.publication.as_ref() == Some(p)) {
                    bail!(
                        "content view {}: publication of {} is still served by {other} in {env}; \
                         promote {other} in {env} again (or roll it back) so it stops serving it, or delete {other} first",
                        cv.name,
                        vr.repo().repo_name
                    );
                }
                if let (Some(r), Some(src)) = (vr.derived_repo, vr.repo) {
                    repos.insert(r, src);
                }
            }
        }
        Ok(repos)
    }

    /// Refused while a distribution that the view does not delete (not in `own`) serves one of its snapshot `repos`:
    /// Pulp deletes a repository with its versions and publications, also served ones.
    async fn refuse_served_snapshots(&self, cv: &ContentView, repos: &BTreeMap<String, CvRepo>, own: &[(String, Value)]) -> Result<()> {
        let mut dists = HashMap::new();
        for (r, src) in repos {
            let (pubs, ds) = self.serving(r, src.repo_type, &mut dists).await?;
            let serves = |d: &&Distribution| {
                !own.iter().any(|(h, _)| *h == d.pulp_href)
                    && (d.repository.as_ref() == Some(r)
                        || d.repository_version.as_deref().is_some_and(|v| ops::repo_of_version(v) == *r)
                        || pubs.iter().any(|p| d.publication.as_ref() == Some(&p.pulp_href)))
            };
            if let Some(d) = ds.iter().find(serves) {
                bail!(
                    "content view {}: distribution {} serves the snapshot repository of {} ({r}); \
                     delete {} or point it to other content first",
                    cv.name,
                    d.name,
                    src.repo_name,
                    d.name
                );
            }
        }
        Ok(())
    }

    /// The publications of snapshot repository `repo` and the distributions of its type `t` (listed once per type in `dists`).
    // shortcut: Deb verbatim publications are not listed; list them too if operators serve snapshots verbatim.
    async fn serving<'a>(
        &self,
        repo: &str,
        t: RepoType,
        dists: &'a mut HashMap<RepoType, Vec<Distribution>>,
    ) -> Result<(Vec<Publication>, &'a [Distribution])> {
        let c = &self.client;
        let pubs = match c.list_all(&c.api(t.publications()), &[("repository", repo.into())], 100_000).await {
            Ok(p) => p,
            // Pulp answers 400 to the href of a deleted repository in a filter; a deleted repository serves nothing.
            Err(e) => match c.get::<Value>(repo, &[]).await {
                Err(g) if g.is_not_found() => vec![],
                _ => return Err(e.into()),
            },
        };
        let ds = match dists.entry(t) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => e.insert(ops::distributions(c, t).await?),
        };
        Ok((pubs, ds))
    }

    /// Delete `href` if it is ours (`not_ours` with `labels`); otherwise it is logged and kept. Already gone is fine.
    async fn delete_owned(&self, href: &str, labels: &Value, aliases: &[String], log: Log<'_>, on: OnTask<'_>) -> Result<()> {
        if let Some(why) = self.not_ours(href, labels, aliases).await? {
            log(format!("keeping {href}: {why}"));
            return Ok(());
        }
        log(format!("deleting {href}"));
        self.delete_href(href, on).await
    }

    /// Delete `href` and wait for Pulp. Already gone is fine.
    async fn delete_href(&self, href: &str, on: OnTask<'_>) -> Result<()> {
        self.alive()?;
        match self.client.delete(href).await {
            Ok(m) => {
                ops::settle(&self.client, m, on).await?;
                Ok(())
            }
            Err(e) if e.is_not_found() => Ok(()),
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
        let mut hrefs: Vec<String> = chosen.iter().map(|p| p.pulp_href.clone()).collect();
        hrefs.extend(rpm_metadata(c, source_version, &pkgs, &chosen).await?);
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
    /// Returns what `resume` returns.
    pub async fn publish(
        &self,
        cv_id: i64,
        notes: &str,
        signing: &str,
        log: Log<'_>,
        on: OnTask<'_>,
    ) -> Result<(CvVersion, Option<String>)> {
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
        let rows: Vec<CvVersionRepo> = repos
            .iter()
            .zip(sources)
            .map(|(r, sv)| CvVersionRepo {
                repo: Some(r.clone()),
                source_version: Some(sv),
                signing: signing.into(), // a checkpoint: a resumed publish signs as asked
                ..Default::default()
            })
            .collect();
        self.alive()?;
        let v = self.store.begin_snapshot_version(cv_id, notes, &rows).await?;
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
    async fn publish_composite(&self, cv: &ContentView, notes: &str, log: Log<'_>, on: OnTask<'_>) -> Result<(CvVersion, Option<String>)> {
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

    /// The composites with auto publish that use `component_id` as latest and whose newest published version lacks the
    /// component's newest published one: the auto publish after it did not run (the client stopped first) or failed.
    pub async fn lagging_composites(&self, component_id: i64) -> Result<Vec<ContentView>> {
        let newest = self.store.versions(component_id).await?.into_iter().find(|v| v.state == "published");
        let Some(newest) = newest else { return Ok(vec![]) };
        let mut out = vec![];
        for c in self.store.auto_composites(component_id).await? {
            let used = match self.store.versions(c.id).await?.into_iter().find(|v| v.state == "published") {
                Some(v) => self.store.version_components(v.id).await?,
                None => vec![],
            };
            if !used.iter().any(|u| u.version_id == newest.id) {
                out.push(c);
            }
        }
        Ok(out)
    }

    /// Publish every composite with auto publish that lags `component_id` (`lagging_composites`), each under its own
    /// lock. One result per composite; a failure does not stop the others. Never promotes beyond Library.
    pub async fn auto_publish(&self, component_id: i64, log: Log<'_>, on: OnTask<'_>) -> Vec<std::result::Result<String, String>> {
        let found = async {
            let name = self.store.content_view(component_id).await?.name;
            let latest = self.store.versions(component_id).await?.into_iter().find(|v| v.state == "published");
            let notes = format!("auto publish after {name} v{}", latest.map_or(0, |v| v.number));
            anyhow::Ok((notes, self.lagging_composites(component_id).await?, self.library_protected().await?))
        };
        let (notes, composites, protected) = match found.await {
            Ok(x) => x,
            Err(e) => return vec![Err(format!("auto publish: {e:#}"))],
        };
        let mut out = vec![];
        for c in composites {
            out.push(self.auto_publish_one(&c, &notes, protected, log, on).await);
        }
        out
    }

    /// The `auto_publish` of composite `c`: an error when it is not published, or published but not served.
    async fn auto_publish_one(
        &self,
        c: &ContentView,
        notes: &str,
        protected: bool,
        log: Log<'_>,
        on: OnTask<'_>,
    ) -> std::result::Result<String, String> {
        let res = async {
            if protected {
                bail!("not published: Library is protected (publish it yourself)"); // nobody types its confirmation
            }
            CvService { lock: None, ..self.clone() }.locked(c.id, "auto publish").await?.publish(c.id, notes, "", log, on).await
        };
        match res.await {
            Ok((v, None)) => Ok(format!("{} v{} published", c.name, v.number)),
            Ok((v, Some(note))) => Err(format!("{} v{} published; {note}", c.name, v.number)),
            Err(e) => Err(format!("{}: {e:#}", c.name)),
        }
    }

    /// Run (or continue) the publish workflow of a version from its checkpoints; of a published version, only a Library
    /// step that stopped. With the version, why Library does not serve it (`None`: it is active there, or it was
    /// published before).
    pub async fn resume(&self, version_id: i64, log: Log<'_>, on: OnTask<'_>) -> Result<(CvVersion, Option<String>)> {
        let v = self.store.version(version_id).await?;
        let cv = self.store.content_view(v.cv_id).await?;
        if v.state == "published" {
            if !self.library_interrupted(&v).await? {
                return Ok((v, None));
            }
            log(format!("{} v{}: the Library step stopped; running it again", cv.name, v.number));
            let note = self.library_step(&v, log, on).await;
            return Ok((self.store.version(v.id).await?, note));
        }
        if v.state == "deleted" {
            bail!("version {} was deleted", v.number);
        }
        if v.state == "deleting" {
            bail!("version {} is being deleted; delete it again to finish", v.number);
        }
        self.alive()?;
        self.store.start_publishing(v.id, self.token()).await?;
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
                let note = self.library_step(&v, log, on).await;
                Ok((self.store.version(v.id).await?, note))
            }
            Err(e) => {
                let msg = format!("{e:#}");
                // Runs even when the lock is lost (it records why the workflow stopped), but only over this
                // workflow's own `publishing` state (by lock token).
                self.store.fail_publishing_version(v.id, &msg, self.token()).await?;
                self.audit("cv.publish.failed", &format!("{} version {}: {msg}", cv.name, v.number)).await;
                Err(e)
            }
        }
    }

    /// Whether Library is protected: re-pointing it then needs a typed confirmation.
    pub async fn library_protected(&self) -> Result<bool> {
        Ok(self.store.environments(&self.scope).await?.iter().any(|e| e.name == LIBRARY && e.protected))
    }

    /// Whether the Library step of published `v` stopped: its promotion to Library is still `promoting`. Under the
    /// view's lock, such a row is an interrupted one.
    pub async fn library_interrupted(&self, v: &CvVersion) -> Result<bool> {
        let Some(lib) = self.store.environments(&self.scope).await?.into_iter().find(|e| e.name == LIBRARY) else { return Ok(false) };
        let b = self.store.binding(lib.id, v.cv_id).await?;
        Ok(b.and_then(|b| b.attempt).is_some_and(|a| a.version_id == v.id && a.state == "promoting"))
    }

    /// `bind_library`, with an error as the note: the version stays published, so auto publish still runs.
    async fn library_step(&self, v: &CvVersion, log: Log<'_>, on: OnTask<'_>) -> Option<String> {
        match self.bind_library(v, log, on).await {
            Ok(note) => note,
            Err(e) => Some(format!("Library step failed: {e:#}; promote v{} to Library", v.number)),
        }
    }

    /// Make the just-published `v` active in Library, unless a newer version is published: resuming an
    /// older version must not take Library back from it. Returns why Library does not serve `v`.
    async fn bind_library(&self, v: &CvVersion, log: Log<'_>, on: OnTask<'_>) -> Result<Option<String>> {
        let newest = self.store.versions(v.cv_id).await?.into_iter().find(|n| n.state == "published");
        if let Some(n) = newest.filter(|n| n.id != v.id) {
            return Ok(Some(format!("Library not changed: v{} is newer (use Rollback to serve v{})", n.number, v.number)));
        }
        let lib = self.store.ensure_library(&self.scope).await?;
        self.promote(v.cv_id, v.id, lib, Mode::Rollback, log, on).await?;
        Ok(None)
    }

    async fn publish_repo(&self, cv: &str, vr: &mut CvVersionRepo, log: Log<'_>, on: OnTask<'_>) -> Result<()> {
        if vr.publication.is_some() {
            return Ok(());
        }
        if vr.borrowed {
            bail!("the component version has no publication"); // its objects are not ours to publish
        }
        let derived = self.derived_repo(cv, vr, log, on).await?;
        // A snapshot that an earlier run recorded: that run may have sent the settings PATCH below and stopped.
        let resumed = vr.derived_version.is_some();
        if !resumed {
            self.snapshot(vr, &derived, log, on).await?;
        }
        let changed = self.copy_settings(vr, &derived, log, on).await?;
        let dv = vr.derived_version.clone().unwrap_or_default();
        log(format!("{}: publishing {dv}", vr.repo().repo_name));
        self.alive()?;
        vr.publication = Some(self.publication(vr, &dv, changed || resumed, log, on).await?);
        self.alive()?;
        self.store.save_version_repo(vr).await?;
        Ok(())
    }

    /// The derived repository of `vr`: the recorded one, if this view still owns it; else the one of its name that this
    /// view owns (labeled for this database when unlabeled), or a new one. Recorded before it is returned.
    async fn derived_repo(&self, cv: &str, vr: &mut CvVersionRepo, log: Log<'_>, on: OnTask<'_>) -> Result<String> {
        let c = &self.client;
        let repo = vr.repo().clone();
        let t = repo.repo_type;
        let name = derived_repo_name(cv, &repo.repo_name);
        let aliases = self.store.aliases().await?;
        // Resuming: another database may have adopted the repository since the checkpoint.
        if let Some(r) = &vr.derived_repo
            && let Some(why) = self.not_ours(r, &json!({ LABEL_CV: slug(cv) }), &aliases).await?
        {
            bail!("repository {name} is {why}");
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
                // The `-` join of view and repository names can collide (`a-b` + `c`, `a` + `b-c`); the format stays.
                Some(r) => match r.pulp_labels.get(LABEL_CV) {
                    Some(other) => bail!(
                        "repository {name} belongs to content view {other} (label {LABEL_CV}={other}): view {cv} with repository {} \
                         derives the same name; refusing to modify it",
                        repo.repo_name
                    ),
                    None => bail!("repository {name} exists but is not labeled {LABEL_CV}={}; refusing to modify it", slug(cv)),
                },
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
        Ok(vr.derived_repo.clone().unwrap_or_default())
    }

    /// Copy the selection of `vr` into `derived` and record the snapshot version: the copy checkpoint.
    async fn snapshot(&self, vr: &mut CvVersionRepo, derived: &str, log: Log<'_>, on: OnTask<'_>) -> Result<()> {
        let c = &self.client;
        let repo = vr.repo().clone();
        let t = repo.repo_type;
        let sv = vr.source_version.clone().ok_or_else(|| anyhow!("missing source version"))?;
        // A pending task is a step already sent; the steps before it are in its version.
        let mut task = self.pending_copy(vr, log, on).await?;
        log(format!("{}: selecting content from {sv}", repo.repo_name));
        let sel = self.select(&repo, &sv, false).await?;
        // (remove, units); `None` copies everything onto an empty version in one request.
        let steps: Vec<(bool, Option<Vec<String>>)> = match sel.hrefs {
            // Without filters the pending task was that request. One that made no version found the content equal
            // to the latest version of its time, which a newer publish may have changed since: it is sent again.
            None if task.as_ref().is_some_and(|t| t.created_matching("/versions/").is_some()) => vec![],
            None => vec![(false, None)],
            // A unit that leaves one version and comes back in a later one makes Pulp's delete of the
            // earlier version very slow (hours), so the latest version is changed only where it differs.
            Some(all) => {
                task = None; // the batches start from the latest version, also when none is left
                let (remove, add) = self.changes(t, derived, all).await?;
                let batches = |remove: bool, l: &[String]| -> Vec<(bool, Option<Vec<String>>)> {
                    ops::batches(l).into_iter().filter(|b| !b.is_empty()).map(|b| (remove, Some(b.to_vec()))).collect()
                };
                [batches(true, &remove), batches(false, &add)].concat()
            }
        };
        let n = steps.len();
        for (i, (remove, units)) in steps.into_iter().enumerate() {
            // In memory only: until the next step is saved, the row keeps the last one for Resume.
            vr.pending_task = None;
            if let Some(u) = &units {
                let what = if remove { "removing" } else { "copying" };
                log(format!("{}: {what} {} units ({}/{n})", repo.repo_name, u.len(), i + 1));
            }
            let empty = units.is_none();
            let copy = async {
                self.alive()?;
                anyhow::Ok(match t {
                    _ if remove => ops::remove_content(c, derived, units.unwrap_or_default()).await?,
                    RepoType::Rpm if empty => rpm::copy(c, &sv, derived, Some(0), None, false).await?,
                    // `rpm/copy/` would add the newest package of each comps group member that a batch lacks.
                    RepoType::Rpm | RepoType::File => ops::add_content(c, derived, units.unwrap_or_default()).await?,
                    RepoType::Deb => deb::copy(c, &sv, derived, empty.then_some(0), units).await?,
                })
            };
            task = Some(self.tracked_task(vr, "copy", copy, log, on).await?);
        }
        // No new version is created when the content equals the latest version.
        let version = match task.as_ref().and_then(|t| t.created_matching("/versions/")) {
            Some(v) => v,
            None => {
                let r: Repository = c.get(derived, &[]).await?;
                r.latest_version_href.ok_or_else(|| anyhow!("derived repository has no version"))?
            }
        };
        let rv: RepoVersion = c.get(&version, &[]).await?;
        vr.content_count = Some(rv.content_summary.present_count() as i64);
        vr.derived_version = Some(version.clone());
        vr.pending_task = None;
        self.alive()?;
        self.store.save_version_repo(vr).await?;
        if let Err(e) = self.drop_unused_versions(t, derived, &version, log, on).await {
            log(format!("{}: cleanup of unused versions stopped: {e:#}", repo.repo_name));
        }
        Ok(())
    }

    /// Give `derived` the publish settings of the source of `vr` (RPM, File) or its signing choice (Deb). Returns
    /// whether a PATCH was sent.
    async fn copy_settings(&self, vr: &CvVersionRepo, derived: &str, log: Log<'_>, on: OnTask<'_>) -> Result<bool> {
        let c = &self.client;
        let repo = vr.repo();
        let t = repo.repo_type;
        // `service`: the signing service that the PATCH sets (Deb only).
        let (what, patch, service) = match t {
            RepoType::Rpm | RepoType::File => {
                let (src, dst): (Value, Value) = (c.get(&repo.repo_href, &[]).await?, c.get(derived, &[]).await?);
                ("publish settings of the source", publish_settings(t, &src, &dst), None)
            }
            RepoType::Deb => {
                let (src, dst): (Value, Value) = (c.get(&repo.repo_href, &[]).await?, c.get(derived, &[]).await?);
                let service = Some(vr.signing.as_str()).filter(|s| !["", "none"].contains(s));
                if service.is_some() && dst.get("signing_service").is_none() {
                    bail!("not supported by this server: signing_service on Deb repositories");
                }
                ("signing", deb_signing_patch(&vr.signing, &src, &dst), service)
            }
        };
        let changed = !patch.is_empty();
        if changed {
            log(format!("{}: {what}: {}", repo.repo_name, patch.keys().cloned().collect::<Vec<_>>().join(", ")));
            self.alive()?;
            let m = match (c.patch(derived, &Value::Object(patch)).await, service) {
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
        Ok(changed)
    }

    /// The publication of snapshot `dv` of `vr` (`changed`: the publish settings can differ from those of a publication
    /// made before): a reused one, or a new one, checkpointed like the copy, so Watch task shows it and Resume re-attaches.
    async fn publication(&self, vr: &mut CvVersionRepo, dv: &str, changed: bool, log: Log<'_>, on: OnTask<'_>) -> Result<String> {
        let (c, t) = (&self.client, vr.repo().repo_type);
        let reuse = match t {
            // A publication keeps the settings the repository had when it was made: after the PATCH, a reused one
            // (same version: the content did not change) would serve the old ones. A pending task is one already sent.
            RepoType::Rpm | RepoType::File if !changed && vr.pending_task.is_none() => ops::find_publication(c, t, dv, &json!({})).await?,
            // Deb: never reused: a publication signed with the repository's keys stores `signing_service: null`, so
            // one of another signing choice cannot be told apart.
            RepoType::Rpm | RepoType::File | RepoType::Deb => None,
        };
        if let Some(p) = reuse {
            return Ok(p);
        }
        let src = vr.repo().repo_href.clone();
        // With `derived_version` set, `pending_task` is the publication task (the copy step clears it with the version).
        let post = async {
            // Deb: the snapshot is published in the mode of the source's newest publication, so clients keep their
            // paths (`simple`: dists/default/). The signing choice is the view's (`copy_settings`), not the source's.
            let mut mode = json!({});
            if t == RepoType::Deb {
                let pubs: Vec<Value> = c.list_all(&c.api(t.publications()), &[("repository", src)], 100_000).await?;
                mode = ops::newest(pubs.iter()).map_or(mode, |p| ops::publication_mode(t, p));
                mode["signing_service"] = Value::Null;
            }
            anyhow::Ok(c.post(&c.api(t.publications()), &ops::publication_body(c, t, dv, &mode).await?).await?)
        };
        let task = self.tracked_task(vr, "publish", post, log, on).await?;
        let publication = task.created_matching("/publications/").ok_or_else(|| anyhow!("the publish task created no publication"))?;
        vr.pending_task = None;
        Ok(publication)
    }

    /// The units to remove from the latest version of `derived`, and to add to it, so that it holds `all`. A Deb
    /// snapshot also keeps the release records of its packages (Pulp publishes every package record in a version,
    /// also one whose package was removed) and the release, component and architecture records.
    async fn changes(&self, t: RepoType, derived: &str, all: Vec<String>) -> Result<(Vec<String>, Vec<String>)> {
        let c = &self.client;
        let r: Repository = c.get(derived, &[]).await?;
        let latest = r.latest_version_href.ok_or_else(|| anyhow!("derived repository has no version"))?;
        let present = ops::content_in(c, &latest).await?;
        let keep: HashSet<&str> = all.iter().map(String::as_str).collect();
        // shortcut: release records of packages already in the snapshot are not refreshed from the source (a
        // changed Release comes with the next copied package); copy them too if a source changes them alone.
        let records = match t {
            RepoType::Deb => deb::release_records_of(c, &latest, &keep).await?,
            _ => HashSet::new(),
        };
        let is_deb = matches!(t, RepoType::Deb);
        let remove = present
            .iter()
            .filter(|h| !keep.contains(h.as_str()) && !records.contains(*h) && !(is_deb && deb::is_release_record(h)))
            .cloned()
            .collect();
        let present: HashSet<String> = present.into_iter().collect();
        Ok((remove, all.into_iter().filter(|h| !present.contains(h)).collect()))
    }

    /// Delete the versions of a derived repository that no Content View version uses: the earlier steps
    /// of a copy, and those of a publish that stopped between steps. Pulp merges each one into the
    /// next version (the snapshot does not change) and refuses a version that a distribution serves.
    /// Best effort: the snapshot is already recorded, and the next publish tries again.
    async fn drop_unused_versions(&self, t: RepoType, derived: &str, latest: &str, log: Log<'_>, on: OnTask<'_>) -> Result<()> {
        let c = &self.client;
        let used: HashSet<String> = self.store.used_versions(derived).await?.into_iter().collect();
        let versions: Vec<RepoVersion> =
            c.list_all(&format!("{derived}versions/"), &[("fields", "pulp_href,number".into())], 100_000).await?;
        // Pulp refuses to delete a served version (`protected_versions`, 3.63.36 and later): check first, so no refused
        // request is sent and the log names the distribution (a binding row can serve a version no view version records).
        let pubs: Vec<Publication> = c.list_all(&c.api(t.publications()), &[("repository", derived.into())], 100_000).await?;
        let ds = ops::distributions(c, t).await?;
        for v in versions.iter().filter(|v| v.number > 0 && v.pulp_href != latest && !used.contains(&v.pulp_href)) {
            self.drop_version(&v.pulp_href, &pubs, &ds, log, on).await?;
        }
        Ok(())
    }

    /// `drop_unused_versions` for one version: kept when a distribution serves it (`pubs`: the publications of
    /// its repository).
    async fn drop_version(&self, v: &str, pubs: &[Publication], ds: &[Distribution], log: Log<'_>, on: OnTask<'_>) -> Result<()> {
        if let Some(why) = served_by(&ops::protecting(v, pubs, ds)) {
            log(format!("keeping {v}: {why}"));
            return Ok(());
        }
        self.alive()?;
        match self.client.delete(v).await {
            Ok(m) => {
                ops::settle(&self.client, m, on).await?;
                log(format!("deleted unused version {v}"));
            }
            Err(e) => log(format!("keeping {v}: {e}")),
        }
        Ok(())
    }

    /// Re-attach to the copy task that `vr` records, if any. `None` also when the version it made was deleted: a newer
    /// publish deletes the versions that no Content View version records (`drop_unused_versions`), so the copy is
    /// sent again.
    async fn pending_copy(&self, vr: &mut CvVersionRepo, log: Log<'_>, on: OnTask<'_>) -> Result<Option<Task>> {
        let Some(task) = self.reattach(vr, log, on).await? else { return Ok(None) };
        match task.created_matching("/versions/") {
            Some(v) if self.client.get::<RepoVersion>(&v, &[]).await.is_err_and(|e| e.status() == Some(404)) => {
                log(format!("{}: version {v} of the interrupted copy was deleted; copying again", vr.repo().repo_name));
                Ok(None)
            }
            _ => Ok(Some(task)),
        }
    }

    /// Wait for the task that `vr` records, if any. A failed, canceled or skipped task is cleared, so the next Resume
    /// sends that step again. A task that is gone (404, for example after `tasks/purge/`) is cleared and gives `None`:
    /// the caller sends that step again at once.
    async fn reattach(&self, vr: &mut CvVersionRepo, log: Log<'_>, on: OnTask<'_>) -> Result<Option<Task>> {
        let Some(h) = vr.pending_task.clone() else { return Ok(None) };
        log(format!("{}: re-attaching to task {h}", vr.repo().repo_name));
        let task = ops::wait_task(&self.client, &h, on).await;
        // Also a task of another Pulp user (RBAC); safe: the copy is idempotent under the view's lock, a new publication harmless.
        let gone = matches!(task, Err(PulpError::TaskUnknown { .. }))
            && self.client.get::<Value>(&h, &[]).await.is_err_and(|e| e.status() == Some(404));
        if gone || matches!(task, Err(PulpError::TaskFailed { .. })) {
            vr.pending_task = None;
            self.alive()?;
            self.store.save_version_repo(vr).await?;
        }
        if gone {
            log(format!("{}: task {h} is gone (purged?); sending that step again", vr.repo().repo_name));
            return Ok(None);
        }
        Ok(Some(task?))
    }

    /// The task of one checkpointed step of `vr` (the copy, or a publication): re-attach to `vr.pending_task`
    /// (`reattach`), else run `submit` and record the task before waiting. A failed, canceled or skipped task is
    /// cleared, so the next Resume sends that step again.
    async fn tracked_task(
        &self,
        vr: &mut CvVersionRepo,
        what: &str,
        submit: impl Future<Output = Result<Mutation>>,
        log: Log<'_>,
        on: OnTask<'_>,
    ) -> Result<Task> {
        if let Some(task) = self.reattach(vr, log, on).await? {
            return Ok(task);
        }
        let Mutation::Task(h) = submit.await? else { bail!("{what} did not return a task") };
        vr.pending_task = Some(h.clone());
        // Saved even when the lock is lost: it records a task already sent, so Resume re-attaches. Only over this
        // workflow's own version (by lock token): a takeover's rows stay.
        self.store.save_own_version_repo(vr, self.token()).await?;
        self.alive()?;
        let task = ops::wait_task(&self.client, &h, on).await;
        if let Err(PulpError::TaskFailed { .. }) = &task {
            vr.pending_task = None;
            self.alive()?;
            self.store.save_version_repo(vr).await?;
        }
        Ok(task?)
    }

    // ---- promote / rollback --------------------------------------------------------

    /// Why Promote refuses version `v` in `env`: an environment after another takes only the version active there.
    pub async fn promote_refusal(&self, v: &CvVersion, env: &Environment) -> Result<Option<String>> {
        let Some(prior) = env.prior_id else { return Ok(None) };
        if self.store.binding(prior, v.cv_id).await?.and_then(|b| b.active()) == Some(v.id) {
            return Ok(None);
        }
        let envs = self.store.environments(&self.scope).await?;
        let pname = envs.iter().find(|e| e.id == prior).map_or("?", |e| e.name.as_str());
        Ok(Some(format!("version {} is not active in {pname}; promote it there first (or use rollback)", v.number)))
    }

    /// The highest version number bound in `env_id` (served or attempted): `rollback_refusal` takes it.
    pub async fn bound_number(&self, env_id: i64, cv_id: i64) -> Result<Option<i64>> {
        let mut n = None;
        for id in self.store.binding(env_id, cv_id).await?.iter().flat_map(Binding::versions) {
            n = n.max(Some(self.store.version(id).await?.number));
        }
        Ok(n)
    }

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
            && let Some(why) = self.promote_refusal(&v, env).await?
        {
            bail!(why);
        }
        if mode == Mode::Rollback
            && env.prior_id.is_some()
            && let Some(why) = rollback_refusal(&env.name, v.number, self.bound_number(env_id, cv_id).await?)
        {
            bail!(why);
        }
        self.alive()?;
        self.store.start_promotion(env_id, cv_id, version_id, self.token()).await?;
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
        errors.extend(self.unbind_removed(&cv, env, &vrs, on).await?);
        // A relabel locks only views already in the environment and lists their rows once: a row that this
        // promotion saved later keeps the old path. A rerun of the relabel moves it.
        if let Some(now) = self.store.environments(&self.scope).await?.into_iter().find(|e| e.id == env_id && e.label != env.label) {
            errors.push(format!("{0} was relabeled {1} during the promotion; edit {0} again with label {1}", env.name, now.label));
        }
        let state = if errors.is_empty() {
            "active"
        } else if bind_failed < vrs.len() {
            "partial"
        } else {
            "failed"
        };
        // Runs even when the lock is lost (it records the partial state; a retry is idempotent), but only over this
        // promotion's own row (by lock token).
        self.store.finish_promotion(env_id, cv_id, version_id, state, self.token()).await?;
        let what = if mode == Mode::Rollback && env.name != LIBRARY { "cv.rollback" } else { "cv.promote" };
        self.audit(what, &format!("{} v{} -> {}: {state} {}", cv.name, v.number, env.name, errors.join("; "))).await;
        if errors.is_empty() {
            log(format!("{} v{} active in {}", cv.name, v.number, env.name));
            Ok(())
        } else {
            Err(anyhow!("{} in {}: {state}: {}", cv.name, env.name, errors.join("; ")))
        }
    }

    /// Repositories of view `cv` that the promoted version (`vrs`) lacks stop serving in `env`; their distributions are
    /// kept for rollback. Returns the error of each one that failed.
    async fn unbind_removed(&self, cv: &ContentView, env: &Environment, vrs: &[CvVersionRepo], on: OnTask<'_>) -> Result<Vec<String>> {
        let aliases = self.store.aliases().await?;
        let mut errors = vec![];
        for b in self.store.binding_repos(env.id, cv.id).await? {
            if vrs.iter().any(|vr| vr.repo().repo_href == b.repo_href) || b.publication.is_none() {
                continue;
            }
            match self.unbind(&cv.name, env, &b, &aliases, on).await {
                Ok(()) => self.store.save_binding_repo(&BindingRepo { publication: None, ..b }).await?,
                Err(e) => errors.push(format!("unbind {}: {e}", b.repo_name)),
            }
        }
        Ok(errors)
    }

    /// Point the distribution of `b` at no publication; refused unless it is ours.
    async fn unbind(&self, cv: &str, env: &Environment, b: &BindingRepo, aliases: &[String], on: OnTask<'_>) -> Result<()> {
        if let Some(why) = self.not_ours(&b.distribution, &dist_labels(cv, &env.name), aliases).await? {
            bail!("distribution {} is {why}", distribution_name(cv, &env.name, &b.repo_name));
        }
        self.alive()?;
        let m = self.client.patch(&b.distribution, &json!({ "publication": null })).await?;
        ops::settle(&self.client, m, on).await?;
        self.alive() // before the checkpoint in unbind_removed
    }

    /// Where `repo` of view `cv` is served in `env`: an existing distribution keeps its path (older
    /// servers use a `cv/` prefix, and an operator may move a distribution), a new one gets `base_path`.
    /// Refuses the distribution of another view or environment that has the same derived name.
    pub async fn served_path(&self, cv: &str, env: &Environment, repo: &CvRepo) -> Result<String> {
        Ok(match self.named_distribution(cv, env, repo).await? {
            Some(d) => d.base_path,
            None => base_path(cv, &env.label, &repo.repo_name),
        })
    }

    /// The distribution of the derived name of `repo` (view `cv`, `env`), which a promotion re-points. Refuses the
    /// distribution of another view or environment that has the same derived name.
    async fn named_distribution(&self, cv: &str, env: &Environment, repo: &CvRepo) -> Result<Option<Distribution>> {
        let coll = self.client.api(repo.repo_type.distributions());
        let name = distribution_name(cv, &env.name, &repo.repo_name);
        let Some(d) = ops::find_by_name::<Distribution>(&self.client, &coll, &name).await? else { return Ok(None) };
        // The `-` join of view, environment and repository names can collide (`web`+`Library`+`b-c`,
        // `web-Library`+`b`+`c`); the format stays. An unlabeled one is refused by `ops::ensure_distribution`.
        let (lv, le) = (d.pulp_labels.get(LABEL_CV), d.pulp_labels.get(LABEL_ENV));
        if let (Some(v), Some(e)) = (lv, le)
            && (v, e) != (&slug(cv), &slug(&env.name))
        {
            bail!(
                "distribution {name} belongs to content view {v} in environment {e} (labels {LABEL_CV}={v}, {LABEL_ENV}={e}): \
                 view {cv} in environment {} with repository {} derives the same name; refusing to modify it",
                env.name,
                repo.repo_name
            );
        }
        Ok(Some(d))
    }

    /// The preview line of the promotion of `repo` (view `cv`) to `env`, read from Pulp: the distribution of the derived
    /// name, the one that `row` records when it still has this view's labels but another name (renamed outside
    /// pulp-tui: the promotion looks for the derived name and fails on its base path), or a new one.
    pub async fn promote_target(&self, cv: &str, env: &Environment, repo: &CvRepo, row: Option<&BindingRepo>) -> Result<String> {
        let what = format!("  {} ({})", repo.repo_name, repo.repo_type.label());
        let name = distribution_name(cv, &env.name, &repo.repo_name);
        if let Some(d) = self.named_distribution(cv, env, repo).await? {
            return Ok(format!("{what}: distribution {} at /{}", d.name, d.base_path));
        }
        let recorded = match row {
            Some(b) => match self.client.get::<Distribution>(&b.distribution, &[]).await {
                Ok(d) => Some(d),
                Err(e) if e.is_not_found() => None,
                Err(e) => return Err(e.into()),
            },
            None => None,
        };
        let ours =
            |d: &Distribution| d.pulp_labels.get(LABEL_CV) == Some(&slug(cv)) && d.pulp_labels.get(LABEL_ENV) == Some(&slug(&env.name));
        Ok(match recorded.filter(ours) {
            Some(d) => format!(
                "{what}: distribution {} at /{} was renamed outside pulp-tui; the promotion looks for {name} and fails: rename it back",
                d.name, d.base_path
            ),
            None => format!("{what}: new distribution {name} at /{}", base_path(cv, &env.label, &repo.repo_name)),
        })
    }

    async fn bind_repo(&self, cv: &str, env: &Environment, vr: &CvVersionRepo, on: OnTask<'_>) -> Result<()> {
        let repo = vr.repo();
        let publication = vr.publication.as_deref().ok_or_else(|| anyhow!("version has no publication"))?;
        let name = distribution_name(cv, &env.name, &repo.repo_name);
        let bp = self.served_path(cv, env, repo).await?;
        let labels = dist_labels(cv, &env.name);
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

    // ---- unpromote ---------------------------------------------------------------------

    /// The environment and the served rows that unpromoting view `cv_id` from `env_id` deletes. Refused while
    /// a later environment on the path still has the view: unpromote goes back along the path. Reads only.
    pub async fn unpromote_impact(&self, cv_id: i64, env_id: i64) -> Result<(Environment, Vec<BindingRepo>)> {
        let cv = self.store.content_view(cv_id).await?.name;
        let envs = self.store.environments(&self.scope).await?;
        let env = envs.iter().find(|e| e.id == env_id).ok_or_else(|| anyhow!("unknown environment"))?;
        let has = self.store.cv_env_ids(cv_id).await?;
        if !has.contains(&env_id) {
            bail!("{cv} is not in {}", env.name);
        }
        let later: Vec<&str> =
            envs.iter().filter(|e| has.contains(&e.id) && comes_after(&envs, e, env_id)).map(|e| e.name.as_str()).collect();
        if !later.is_empty() {
            bail!("{cv} is still in {}; unpromote it there first", later.join(", "));
        }
        Ok((env.clone(), self.store.binding_repos(env_id, cv_id).await?))
    }

    /// Stop serving view `cv_id` in `env_id`: delete its distributions there, then forget the binding. A rerun
    /// after a failure deletes what is left. A distribution that is not ours (`not_ours`) is kept.
    pub async fn unpromote(&self, cv_id: i64, env_id: i64, log: Log<'_>, on: OnTask<'_>) -> Result<()> {
        let (env, rows) = self.unpromote_impact(cv_id, env_id).await?;
        let cv = self.store.content_view(cv_id).await?.name;
        self.delete_all_owned(rows.iter().map(|b| (b.distribution.clone(), dist_labels(&cv, &env.name))).collect(), log, on).await?;
        self.alive()?;
        self.store.delete_binding(env_id, cv_id).await?;
        self.audit("cv.unpromote", &format!("{cv} from {}: {} distribution(s)", env.name, rows.len())).await;
        log(format!("{cv} is not in {} any more", env.name));
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
        let (mut own, mut out) = (vec![], vec![]);
        for cv_id in self.store.env_cv_ids(env_id).await? {
            let cv = self.store.content_view(cv_id).await?.name;
            for b in self.store.binding_repos(env_id, cv_id).await? {
                own.push(b.distribution.clone());
                let new = base_path(&cv, label, &b.repo_name);
                if b.base_path != new {
                    out.push((b, new));
                }
            }
        }
        self.refuse_label_overlap(label, &own).await?;
        Ok(out)
    }

    /// Refused when another distribution (not in `own`) has the base path `label` or one below it: a served path
    /// `<label>/<view>/<repo>` would overlap it, and Pulp refuses a base path that equals, contains or is inside
    /// another one (USER_GUIDE "Lifecycle Environments").
    async fn refuse_label_overlap(&self, label: &str, own: &[String]) -> Result<()> {
        let c = &self.client;
        let found: Vec<Distribution> = c.list_all(&c.api("distributions/"), &[("base_path__contains", label.into())], 100_000).await?;
        let under = |p: &str| p == label || p.strip_prefix(label).is_some_and(|r| r.starts_with('/'));
        if let Some(d) = found.iter().find(|d| !own.contains(&d.pulp_href) && under(&d.base_path)) {
            bail!(
                "label {label} is refused: distribution {} has base path /{}, the label or below it \
                 (served paths would overlap it; Pulp refuses that)",
                d.name,
                d.base_path
            );
        }
        Ok(())
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
        let moves = self.relabel_impact(env_id, label).await?; // under the locks; refuses an overlapping label
        self.store.set_env_label(env_id, label).await?; // after the calls that can fail: then the audit entry below is always written
        let (mut lines, mut moved, mut failed) = (vec![], 0, 0);
        let res = async {
            for (b, new) in moves {
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
        let Some(b) = self.store.binding(env_id, cv_id).await? else { return Ok(vec!["not bound".into()]) };
        let mut issues = vec![];
        if let Some(a) = &b.attempt {
            let running = self.store.lock_live(cv_id, unix_now()).await?;
            let n = self.store.version(a.version_id).await?.number;
            issues.push(format!("promotion of v{n} is {}", shown_state(&a.state, running)));
        }
        // The served version; with none yet, the attempted one.
        let Some(target) = b.versions().next() else { return Ok(issues) };
        let vrs = self.store.version_repos(target).await?;
        for br in self.store.binding_repos(env_id, cv_id).await? {
            let vr = vrs.iter().find(|vr| vr.repo().repo_href == br.repo_href);
            issues.extend(self.verify_repo(&br, vr).await);
        }
        issues.extend(self.retention_issues(&vrs).await);
        Ok(issues)
    }

    /// The findings for binding row `br`; `vr` is its row in the bound version (`None`: it must serve nothing).
    async fn verify_repo(&self, br: &BindingRepo, vr: Option<&CvVersionRepo>) -> Vec<String> {
        let want = vr.and_then(|v| v.publication.clone());
        let d: Distribution = match self.client.get(&br.distribution, &[]).await {
            Ok(d) => d,
            Err(e) => return vec![format!("{}: distribution {}: {e}", br.repo_name, br.distribution)],
        };
        if d.publication != want {
            return vec![format!(
                "{}: distribution serves {}, expected {}",
                br.repo_name,
                d.publication.as_deref().unwrap_or("nothing"),
                want.as_deref().unwrap_or("nothing")
            )];
        }
        let Some(vr) = vr else { return vec![] };
        self.verify_publication(br, vr, &d).await
    }

    /// The findings for the publication that `d` serves (the one of `vr`): its version, the snapshot, the served content.
    async fn verify_publication(&self, br: &BindingRepo, vr: &CvVersionRepo, d: &Distribution) -> Vec<String> {
        let c = &self.client;
        let p: Value = match c.get(d.publication.as_deref().unwrap_or_default(), &[]).await {
            Ok(p) => p,
            Err(e) => return vec![format!("{}: publication: {e}", br.repo_name)],
        };
        let mut issues = vec![];
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
            return issues;
        };
        let dv = vr.derived_version.as_deref().unwrap_or_default();
        if let Err(e) = ops::verify_served(c, br.repo_type, base, dv, &p).await {
            issues.push(format!("{}: {e}", br.repo_name));
        }
        issues
    }

    /// Retention on derived repositories would prune CV snapshots.
    async fn retention_issues(&self, vrs: &[CvVersionRepo]) -> Vec<String> {
        let mut issues = vec![];
        for vr in vrs {
            if let Some(dr) = &vr.derived_repo
                && let Ok(r) = self.client.get::<Repository>(dr, &[]).await
                && r.retain_repo_versions.is_some()
            {
                issues.push(format!("{}: retain_repo_versions is set on derived repository {}", vr.repo().repo_name, r.name));
            }
        }
        issues
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

    /// Refused while version `v` is bound to an environment or a composite version uses or pins it.
    async fn refuse_used(&self, v: &CvVersion) -> Result<()> {
        let used = self.store.version_in_use(v.id).await?;
        if !used.is_empty() {
            bail!("version {} is bound to {}", v.number, used.join(", "));
        }
        let users = self.store.composite_users(v.id).await?;
        if !users.is_empty() {
            bail!("version {} is used by {}", v.number, users.join(", "));
        }
        Ok(())
    }

    /// What `delete_version` does in Pulp now, or why it is refused. Reads only.
    pub async fn delete_version_impact(&self, version_id: i64) -> Result<Vec<(String, String, Option<String>)>> {
        let v = self.store.version(version_id).await?;
        self.refuse_used(&v).await?;
        self.version_objects(&v).await
    }

    /// Delete a version's Pulp objects unless shared with another live version or bound. Refused while a
    /// composite uses or pins it; a composite version's borrowed rows are kept (they are the component's).
    pub async fn delete_version(&self, version_id: i64, log: Log<'_>, on: OnTask<'_>) -> Result<()> {
        let v = self.store.version(version_id).await?;
        self.mark_deleting(&v).await?;
        self.delete_objects(&v, log, on).await?;
        self.alive()?;
        self.store.finish_version(version_id, "deleted", None).await?;
        let name = self.store.content_view(v.cv_id).await?.name;
        self.audit("cv.version.delete", &format!("{name} version {}", v.number)).await;
        Ok(())
    }

    /// Mark `v` `deleting`. Refused while it is used (`refuse_used`); a failed delete stays `deleting`.
    async fn mark_deleting(&self, v: &CvVersion) -> Result<()> {
        self.refuse_used(v).await?; // a bound version is never marked
        // Marked before the check again: a composite publish that starts later refuses a version that is not
        // `published` (`Store::begin`), and one that started earlier is found by it.
        self.alive()?;
        if !self.store.set_version_state(v.id, &v.state, "deleting").await? {
            bail!("version {} changed meanwhile; refresh and try again", v.number);
        }
        if let Err(e) = self.refuse_used(v).await {
            self.store.set_version_state(v.id, "deleting", &v.state).await?;
            return Err(e);
        }
        Ok(())
    }

    /// Delete the objects of `v` that `version_objects` does not keep; a kept one is logged with why.
    async fn delete_objects(&self, v: &CvVersion, log: Log<'_>, on: OnTask<'_>) -> Result<()> {
        for (_, href, keep) in self.version_objects(v).await? {
            match keep {
                Some(why) => log(format!("keeping {href}: {why}")),
                None => self.delete_href(&href, on).await?,
            }
        }
        Ok(())
    }

    /// The publication and snapshot version of each own (not borrowed) row of version `v`: (repository, href, why
    /// it is kept; `None` = deleted). Kept: shared with another live version, served by any binding row of the
    /// scope, served by a Pulp distribution, or not ours.
    async fn version_objects(&self, v: &CvVersion) -> Result<Vec<(String, String, Option<String>)>> {
        let mut shared = self.other_versions_objects(v).await?;
        // Any binding row of the scope, also a composite's, may still serve a publication of this version.
        let served = self.store.served_publications(&self.scope).await?;
        shared.extend(served.iter().map(|(p, _, _)| p.clone()));
        let aliases = self.store.aliases().await?;
        let name = self.store.content_view(v.cv_id).await?.name;
        let (mut objects, mut dists) = (vec![], HashMap::new());
        for vr in self.store.version_repos(v.id).await?.into_iter().filter(|vr| !vr.borrowed) {
            // Publications and versions belong to the derived repository.
            let not_ours =
                if let Some(r) = &vr.derived_repo { self.not_ours(r, &json!({ LABEL_CV: slug(&name) }), &aliases).await? } else { None };
            let pulp = if not_ours.is_none() { self.served_in_pulp(&vr, &mut dists).await? } else { [None, None] };
            objects.extend(row_objects(&vr, not_ours, pulp, &mut shared, &served));
        }
        Ok(objects)
    }

    /// Why a distribution serves the publication and the snapshot version of `vr` (`None`: none does), from
    /// `ops::protecting`. Pulp does not protect a served publication, so a distribution that no binding row records
    /// would lose its content. One pinned to another publication of the version serves only that one.
    async fn served_in_pulp(&self, vr: &CvVersionRepo, dists: &mut HashMap<RepoType, Vec<Distribution>>) -> Result<[Option<String>; 2]> {
        let (Some(r), Some(dv)) = (&vr.derived_repo, &vr.derived_version) else { return Ok([None, None]) };
        let (pubs, ds) = self.serving(r, vr.repo().repo_type, dists).await?;
        let mut by = ops::protecting(dv, &pubs, ds);
        let version = served_by(&by);
        by.retain(|(d, _)| d.publication.is_none() || d.publication == vr.publication);
        Ok([served_by(&by), version])
    }

    /// The publications and snapshot versions of the other live versions of `v`'s view.
    async fn other_versions_objects(&self, v: &CvVersion) -> Result<HashSet<String>> {
        let mut shared = HashSet::new();
        for other in self.store.versions(v.cv_id).await? {
            if other.id != v.id && other.state != "deleted" {
                for vr in self.store.version_repos(other.id).await? {
                    shared.extend(vr.publication.into_iter().chain(vr.derived_version));
                }
            }
        }
        Ok(shared)
    }

    /// Remove a whole content view: its distributions and derived repositories (a composite has none:
    /// its rows borrow the components' objects).
    pub async fn delete_view(&self, cv_id: i64, log: Log<'_>, on: OnTask<'_>) -> Result<()> {
        let cv = self.store.content_view(cv_id).await?;
        let objects = self.view_objects(&cv).await?;
        self.delete_all_owned(objects, log, on).await?;
        self.alive()?;
        self.store.delete_cv(cv_id).await?;
        self.audit("cv.delete", &cv.name).await;
        Ok(())
    }

    /// Why `delete_view` is refused, if it is. Reads only.
    pub async fn delete_view_impact(&self, cv_id: i64) -> Result<()> {
        self.view_objects(&self.store.content_view(cv_id).await?).await.map(drop)
    }

    /// (href, its ownership labels) of each distribution and derived repository of view `cv`. Refused while a
    /// composite uses the view or something outside it serves one of its snapshots. Reads only.
    async fn view_objects(&self, cv: &ContentView) -> Result<Vec<(String, Value)>> {
        let refs = self.store.composite_refs(cv.id).await?;
        if !refs.is_empty() {
            bail!(
                "content view {} is used by {}; remove it from the composite and delete those composite versions first",
                cv.name,
                refs.join(", ")
            );
        }
        let envs = self.store.environments(&self.scope).await?;
        let mut objects = vec![];
        for b in self.store.all_binding_repos(cv.id).await? {
            let env = envs.iter().find(|e| e.id == b.env_id).ok_or_else(|| anyhow!("unknown environment"))?;
            objects.push((b.distribution, dist_labels(&cv.name, &env.name)));
        }
        let repos = self.snapshot_repos(cv).await?;
        self.refuse_served_snapshots(cv, &repos, &objects).await?;
        objects.extend(repos.into_keys().map(|r| (r, json!({ LABEL_CV: slug(&cv.name) }))));
        Ok(objects)
    }

    /// `delete_owned` for each (href, its ownership labels), in order.
    async fn delete_all_owned(&self, objects: Vec<(String, Value)>, log: Log<'_>, on: OnTask<'_>) -> Result<()> {
        let aliases = self.store.aliases().await?;
        for (h, labels) in objects {
            self.delete_owned(&h, &labels, &aliases, log, on).await?;
        }
        Ok(())
    }
}

/// (repository, href, why it is kept) for the publication and snapshot version of `vr`: kept when not ours, when
/// `shared` has it (`served`: (publication, view, environment) of the binding rows), or when a Pulp distribution
/// serves it (`pulp`: why, for the publication and the version).
fn row_objects(
    vr: &CvVersionRepo,
    not_ours: Option<String>,
    pulp: [Option<String>; 2],
    shared: &mut HashSet<String>,
    served: &[(String, String, String)],
) -> Vec<(String, String, Option<String>)> {
    let by = served.iter().find(|(p, _, _)| vr.publication.as_ref() == Some(p));
    // Pulp deletes a repository version's publications with it: a kept publication keeps its version.
    if let (Some(p), Some(dv)) = (&vr.publication, &vr.derived_version)
        && shared.contains(p)
    {
        shared.insert(dv.clone());
    }
    let mut objects = vec![];
    for (href, pulp) in [&vr.publication, &vr.derived_version].into_iter().zip(pulp) {
        let Some(href) = href else { continue };
        let keep = not_ours
            .clone()
            .or_else(|| {
                shared.contains(href).then(|| match by {
                    Some((_, other, env)) => format!("still served by {other} in {env}"),
                    None => "shared with another version".into(),
                })
            })
            .or(pulp);
        objects.push((vr.repo().repo_name.clone(), href.clone(), keep));
    }
    objects
}

/// "served by <distribution> (<why>), …" for the distributions that `ops::protecting` gives; `None` when there are none.
fn served_by(by: &[(&Distribution, &str)]) -> Option<String> {
    let names: Vec<String> = by.iter().map(|(d, why)| format!("{} ({why})", d.name)).collect();
    (!names.is_empty()).then(|| format!("served by {}", names.join(", ")))
}

/// Whether environment `e` comes after environment `id` on its path.
fn comes_after(envs: &[Environment], e: &Environment, id: i64) -> bool {
    let mut prior = e.prior_id;
    while let Some(p) = prior {
        if p == id {
            return true;
        }
        prior = envs.iter().find(|x| x.id == p).and_then(|x| x.prior_id);
    }
    false
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
            "layout": "nested_alphabetically", "compression_type": "zstd", "retain_repo_versions": 3,
            "package_signing_service": "/s/2/", "package_signing_fingerprint": "v4:AB", "retain_checkpoints": 2 });
        let dst = json!({ "name": "web-os", "metadata_signing_service": null, "checksum_type": "sha256", "layout": "nested_alphabetically",
            "compression_type": null, "package_signing_service": null, "package_signing_fingerprint": null, "retain_checkpoints": null });
        let patch = publish_settings(RepoType::Rpm, &src, &dst);
        // Package signing would re-sign the packages that the snapshot gets through `modify/`.
        assert_eq!(
            Value::Object(patch),
            json!({ "metadata_signing_service": "/s/1/", "repo_config": { "gpgcheck": 1 }, "compression_type": "zstd" })
        );
        assert!(publish_settings(RepoType::Rpm, &src, &src).is_empty(), "nothing to send when they agree");
        let old = json!({ "name": "old server" });
        assert!(publish_settings(RepoType::Rpm, &old, &dst).is_empty(), "fields the server lacks are never sent");
        let (src, dst) = (json!({ "manifest": "CUSTOM", "autopublish": true }), json!({ "manifest": "PULP_MANIFEST" }));
        assert_eq!(Value::Object(publish_settings(RepoType::File, &src, &dst)), json!({ "manifest": "CUSTOM" }));
        assert!(publish_settings(RepoType::Deb, &src, &dst).is_empty(), "Deb: the signing choice instead");
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

    /// The object at `href` carries the ownership labels of view web in Dev and the database ID label `db`.
    async fn serve_owner(server: &wiremock::MockServer, href: &str, db: &str) {
        serve_labels(server, href, json!({ LABEL_CV: "web", LABEL_ENV: "Dev", LABEL_DB: db })).await;
    }

    async fn serve_labels(server: &wiremock::MockServer, href: &str, labels: Value) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(href))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({ "pulp_labels": labels })))
            .mount(server)
            .await;
    }

    /// No request of this HTTP method reached the server.
    async fn assert_none(server: &wiremock::MockServer, method: &str) {
        let sent: Vec<_> = server.received_requests().await.unwrap().into_iter().filter(|r| r.method.as_str() == method).collect();
        assert!(sent.is_empty(), "unexpected {method}: {sent:?}");
    }

    /// `svc` with the lock of view `cv`, running the publish of version `vid` (as `resume` starts it).
    async fn publishing(svc: CvService, cv: i64, vid: i64) -> CvService {
        let svc = svc.locked(cv, "publish").await.unwrap();
        svc.store.start_publishing(vid, svc.token()).await.unwrap();
        svc
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
        let svc = publishing(svc, cv, vr.version_id).await;
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

    #[tokio::test(flavor = "multi_thread")]
    async fn a_skipped_copy_task_fails_the_version_and_records_no_snapshot() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (_, vr) = rpm_version(&svc).await;
        svc.store.save_version_repo(&vr).await.unwrap();
        let ok = |body: Value| ResponseTemplate::new(200).set_body_json(body);
        // The derived repository still has the content of the previous publish.
        let old = format!("{DERIVED}versions/1/");
        let labels = json!({ LABEL_CV: "web", LABEL_DB: svc.store.db_id() });
        let derived = json!({ "pulp_href": DERIVED, "name": "web-os", "pulp_labels": labels, "latest_version_href": old });
        Mock::given(method("GET")).and(path(DERIVED)).respond_with(ok(derived)).mount(&server).await;
        Mock::given(method("GET")).and(path(old.as_str())).respond_with(ok(json!({ "pulp_href": old, "number": 1 }))).mount(&server).await;
        let task = json!({ "task": "/pulp/api/v3/tasks/1/" });
        Mock::given(method("POST"))
            .and(path("/pulp/api/v3/rpm/copy/"))
            .respond_with(ResponseTemplate::new(202).set_body_json(task))
            .mount(&server)
            .await;
        let skipped = json!({ "pulp_href": "/pulp/api/v3/tasks/1/", "state": "skipped", "name": "copy" });
        Mock::given(method("GET")).and(path("/pulp/api/v3/tasks/1/")).respond_with(ok(skipped)).mount(&server).await;
        let e = svc.resume(vr.version_id, &|_| {}, &ops::no_progress).await.unwrap_err();
        assert_eq!(format!("{e:#}"), "repository os: task skipped: ");
        let v = svc.store.version(vr.version_id).await.unwrap();
        assert_eq!((v.state.as_str(), v.error.as_deref()), ("failed", Some("repository os: task skipped: ")));
        let saved = svc.store.version_repos(vr.version_id).await.unwrap().remove(0);
        assert_eq!((saved.derived_version, saved.pending_task), (None, None), "the old version is not the snapshot");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_copies_again_when_a_newer_publish_deleted_the_version_of_its_copy() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (cv, mut old) = rpm_version(&svc).await;
        // The copy of the old version made version 1; the client stopped before it recorded that.
        old.pending_task = Some("/pulp/api/v3/tasks/1/".into());
        svc.store.save_version_repo(&old).await.unwrap();
        let newer = svc.store.begin_version(cv, "", &[old.repo().clone()]).await.unwrap();
        let mut new = svc.store.version_repos(newer.id).await.unwrap().remove(0);
        (new.source_version, new.derived_repo) = (old.source_version.clone(), old.derived_repo.clone());
        serve_owner(&server, DERIVED, svc.store.db_id()).await;
        let v = |n: u64| format!("{DERIVED}versions/{n}/");
        let ok = |body: Value| ResponseTemplate::new(200).set_body_json(body);
        let done = |n: u64| json!({ "pulp_href": format!("/pulp/api/v3/tasks/{n}/"), "state": "completed", "name": "copy", "created_resources": [v(n)] });
        // Every new copy is task 2 and makes version 2. Version 1 is not served: deleted.
        let copy = ResponseTemplate::new(202).set_body_json(json!({ "task": "/pulp/api/v3/tasks/2/" }));
        Mock::given(method("POST")).and(path("/pulp/api/v3/rpm/copy/")).respond_with(copy).mount(&server).await;
        for n in [1, 2] {
            Mock::given(method("GET")).and(path(format!("/pulp/api/v3/tasks/{n}/"))).respond_with(ok(done(n))).mount(&server).await;
        }
        Mock::given(method("GET")).and(path(v(2))).respond_with(ok(json!({ "pulp_href": v(2), "number": 2 }))).mount(&server).await;
        let versions: Vec<Value> = (0..3).map(|n| json!({ "pulp_href": v(n), "number": n })).collect();
        let page = json!({ "count": 3, "next": null, "results": versions });
        Mock::given(method("GET")).and(path(format!("{DERIVED}versions/"))).respond_with(ok(page)).mount(&server).await;
        for coll in ["publications/rpm/rpm/", "distributions/rpm/rpm/"] {
            let none = json!({ "count": 0, "next": null, "results": [] });
            Mock::given(method("GET")).and(path(format!("/pulp/api/v3/{coll}"))).respond_with(ok(none)).mount(&server).await;
        }
        Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(204)).mount(&server).await;

        svc.publish_repo("web", &mut new, &|_| {}, &ops::no_progress).await.unwrap_err(); // the publish step is not served
        let sent = server.received_requests().await.unwrap();
        let deleted: Vec<_> = sent.iter().filter(|r| r.method.as_str() == "DELETE").map(|r| r.url.path().to_string()).collect();
        assert_eq!(deleted, [v(1)], "the newer publish deletes the version that only the interrupted copy made");
        let e = svc.publish_repo("web", &mut old, &|_| {}, &ops::no_progress).await.unwrap_err(); // the publish step is not served
        let saved = svc.store.version_repos(old.version_id).await.unwrap().remove(0);
        assert_eq!((saved.derived_version, saved.pending_task), (Some(v(2)), None), "copied again, not stuck: {e:#}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_copies_again_when_the_interrupted_copy_made_no_version() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (_, mut vr) = rpm_version(&svc).await;
        // The copy found its content equal to the latest version of its time, so it made no version. A newer publish
        // then made version 4 with other content.
        vr.pending_task = Some("/pulp/api/v3/tasks/1/".into());
        svc.store.save_version_repo(&vr).await.unwrap();
        let v = |n: u64| format!("{DERIVED}versions/{n}/");
        let ok = |body: Value| ResponseTemplate::new(200).set_body_json(body);
        let labels = json!({ LABEL_CV: "web", LABEL_DB: svc.store.db_id() });
        let derived = json!({ "pulp_href": DERIVED, "name": "web-os", "pulp_labels": labels, "latest_version_href": v(4) });
        Mock::given(method("GET")).and(path(DERIVED)).respond_with(ok(derived)).mount(&server).await;
        let done = |n: u64, made: Value| json!({ "pulp_href": format!("/pulp/api/v3/tasks/{n}/"), "state": "completed", "created_resources": made });
        Mock::given(method("GET")).and(path("/pulp/api/v3/tasks/1/")).respond_with(ok(done(1, json!([])))).mount(&server).await;
        let copy = ResponseTemplate::new(202).set_body_json(json!({ "task": "/pulp/api/v3/tasks/2/" }));
        Mock::given(method("POST")).and(path("/pulp/api/v3/rpm/copy/")).respond_with(copy).mount(&server).await;
        Mock::given(method("GET")).and(path("/pulp/api/v3/tasks/2/")).respond_with(ok(done(2, json!([v(5)])))).mount(&server).await;
        for n in [4, 5] {
            Mock::given(method("GET")).and(path(v(n))).respond_with(ok(json!({ "pulp_href": v(n), "number": n }))).mount(&server).await;
        }
        svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await.unwrap_err(); // the publish step is not served
        let saved = svc.store.version_repos(vr.version_id).await.unwrap().remove(0);
        assert_eq!(saved.derived_version, Some(v(5)), "not the version of the newer publish");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_copies_again_when_the_copy_task_was_purged() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (_, mut vr) = rpm_version(&svc).await;
        // An admin purged the finished tasks (`tasks/purge/`): the copy task answers 404 (not mounted).
        vr.pending_task = Some("/pulp/api/v3/tasks/1/".into());
        svc.store.save_version_repo(&vr).await.unwrap();
        serve_owner(&server, DERIVED, svc.store.db_id()).await;
        let v2 = format!("{DERIVED}versions/2/");
        let ok = |body: Value| ResponseTemplate::new(200).set_body_json(body);
        let copy = ResponseTemplate::new(202).set_body_json(json!({ "task": "/pulp/api/v3/tasks/2/" }));
        Mock::given(method("POST")).and(path("/pulp/api/v3/rpm/copy/")).respond_with(copy).mount(&server).await;
        let done = json!({ "pulp_href": "/pulp/api/v3/tasks/2/", "state": "completed", "created_resources": [v2] });
        Mock::given(method("GET")).and(path("/pulp/api/v3/tasks/2/")).respond_with(ok(done)).mount(&server).await;
        Mock::given(method("GET")).and(path(v2.as_str())).respond_with(ok(json!({ "pulp_href": v2, "number": 2 }))).mount(&server).await;
        let e = svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await.unwrap_err(); // the settings step is not served
        let saved = svc.store.version_repos(vr.version_id).await.unwrap().remove(0);
        assert_eq!((saved.derived_version, saved.pending_task), (Some(v2), None), "copied again: {e:#}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_publishes_again_when_the_publication_task_was_purged() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let pubs = "/pulp/api/v3/publications/rpm/rpm/";
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (_, mut vr) = rpm_version(&svc).await;
        // The snapshot is recorded; the publication task that the earlier run sent was purged (404, not mounted).
        vr.derived_version = Some(format!("{DERIVED}versions/1/"));
        vr.pending_task = Some("/pulp/api/v3/tasks/1/".into());
        svc.store.save_version_repo(&vr).await.unwrap();
        serve_owner(&server, DERIVED, svc.store.db_id()).await;
        serve_labels(&server, &vr.repo().repo_href, json!({})).await;
        let post = ResponseTemplate::new(202).set_body_json(json!({ "task": "/pulp/api/v3/tasks/2/" }));
        Mock::given(method("POST")).and(path(pubs)).respond_with(post).mount(&server).await;
        let p = format!("{pubs}3/");
        let done = json!({ "pulp_href": "/pulp/api/v3/tasks/2/", "state": "completed", "created_resources": [p] });
        let done = ResponseTemplate::new(200).set_body_json(done);
        Mock::given(method("GET")).and(path("/pulp/api/v3/tasks/2/")).respond_with(done).mount(&server).await;
        svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await.unwrap();
        let saved = svc.store.version_repos(vr.version_id).await.unwrap().remove(0);
        assert_eq!((saved.publication.as_deref(), saved.pending_task), (Some(p.as_str()), None));
        let posts = server.received_requests().await.unwrap().iter().filter(|r| r.method.as_str() == "POST").count();
        assert_eq!(posts, 1, "one new publication");
    }

    const FILE_DERIVED: &str = "/pulp/api/v3/repositories/file/file/2/";

    /// A File row before its copy, with `n` files in the source version, and a server that completes
    /// every task with version 3 of the derived repository. The publish step after the copy is not served.
    async fn file_copy(server: &wiremock::MockServer, svc: &CvService, n: usize) -> (CvVersionRepo, Vec<String>) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let cv = svc.store.create_cv("k", "web", "").await.unwrap();
        let repo = CvRepo {
            repo_href: "/pulp/api/v3/repositories/file/file/1/".into(),
            repo_name: "iso".into(),
            repo_type: RepoType::File,
            filters: vec![],
        };
        let v = svc.store.begin_version(cv, "", &[repo]).await.unwrap();
        let mut vr = svc.store.version_repos(v.id).await.unwrap().remove(0);
        vr.source_version = Some("/pulp/api/v3/repositories/file/file/1/versions/1/".into());
        vr.derived_repo = Some(FILE_DERIVED.into());
        let hrefs: Vec<String> = (0..n).map(|i| format!("/pulp/api/v3/content/file/files/{i:036}/")).collect();
        let files: Vec<Value> = hrefs.iter().map(|h| json!({ "pulp_href": h, "relative_path": h })).collect();
        let page = |results: Vec<Value>| {
            ResponseTemplate::new(200).set_body_json(json!({ "count": results.len(), "next": null, "results": results }))
        };
        Mock::given(method("GET")).and(path("/pulp/api/v3/content/file/files/")).respond_with(page(files)).mount(server).await;
        let latest = format!("{FILE_DERIVED}versions/3/");
        let derived = json!({ "pulp_href": FILE_DERIVED, "name": "web-iso", "latest_version_href": latest, "pulp_labels": { LABEL_CV: "web", LABEL_DB: svc.store.db_id() } });
        Mock::given(method("GET"))
            .and(path(FILE_DERIVED))
            .respond_with(ResponseTemplate::new(200).set_body_json(derived))
            .mount(server)
            .await;
        let task = json!({ "task": "/pulp/api/v3/tasks/1/" });
        Mock::given(method("POST"))
            .and(path(format!("{FILE_DERIVED}modify/")))
            .respond_with(ResponseTemplate::new(202).set_body_json(task))
            .mount(server)
            .await;
        let done = json!({ "pulp_href": "/pulp/api/v3/tasks/1/", "state": "completed", "name": "modify", "created_resources": [latest] });
        Mock::given(method("GET"))
            .and(path("/pulp/api/v3/tasks/1/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(done))
            .mount(server)
            .await;
        let version = json!({ "pulp_href": latest, "number": 3 });
        Mock::given(method("GET")).and(path(latest)).respond_with(ResponseTemplate::new(200).set_body_json(version)).mount(server).await;
        (vr, hrefs)
    }

    /// The bodies of the modify requests the server received.
    async fn modify_bodies(server: &wiremock::MockServer) -> Vec<Value> {
        let sent = server.received_requests().await.unwrap();
        sent.iter().filter(|r| r.method.as_str() == "POST").map(|r| r.body_json().unwrap()).collect()
    }

    /// The content of any version is `hrefs`.
    async fn serve_content(server: &wiremock::MockServer, hrefs: &[String]) {
        let results: Vec<Value> = hrefs.iter().map(|h| json!({ "pulp_href": h })).collect();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/pulp/api/v3/content/"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(json!({ "count": results.len(), "next": null, "results": results })),
            )
            .mount(server)
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_large_selection_is_copied_in_batches() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (mut vr, hrefs) = file_copy(&server, &svc, 30_000).await;
        serve_content(&server, &[]).await;
        svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await.unwrap_err(); // the publish step is not served
        let bodies = modify_bodies(&server).await;
        assert_eq!(bodies.len(), 2, "30,000 hrefs of 73 bytes do not fit in one body");
        assert!(bodies.iter().all(|b| b.get("remove_content_units").is_none()), "each batch adds to the latest version");
        let sent: Vec<String> =
            bodies.iter().flat_map(|b| serde_json::from_value::<Vec<String>>(b["add_content_units"].clone()).unwrap()).collect();
        assert_eq!(sent, hrefs);
        let saved = svc.store.version_repos(vr.version_id).await.unwrap().remove(0);
        assert_eq!((saved.derived_version.as_deref(), saved.pending_task), (Some(format!("{FILE_DERIVED}versions/3/").as_str()), None));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_snapshot_is_published_with_the_manifest_name_of_its_source() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (mut vr, _) = file_copy(&server, &svc, 1).await;
        serve_content(&server, &[]).await;
        let ok = |body: Value| ResponseTemplate::new(200).set_body_json(body);
        let source = json!({ "name": "iso", "manifest": "CUSTOM" });
        Mock::given(method("GET")).and(path(vr.repo().repo_href.as_str())).respond_with(ok(source)).mount(&server).await;
        Mock::given(method("PATCH")).and(path(FILE_DERIVED)).respond_with(ok(json!({}))).mount(&server).await;
        // After the PATCH, the snapshot repository names the manifest of its source.
        Mock::given(method("GET"))
            .and(path(FILE_DERIVED))
            .and(query_param("fields", "manifest"))
            .respond_with(ok(json!({ "manifest": "CUSTOM" })))
            .with_priority(1)
            .mount(&server)
            .await;
        let pubs = "/pulp/api/v3/publications/file/file/";
        Mock::given(method("GET"))
            .and(path(pubs))
            .respond_with(ok(json!({ "count": 0, "next": null, "results": [] })))
            .mount(&server)
            .await;
        let task = ResponseTemplate::new(202).set_body_json(json!({ "task": "/pulp/api/v3/tasks/2/" }));
        Mock::given(method("POST")).and(path(pubs)).respond_with(task).mount(&server).await;
        let publication = format!("{pubs}1/");
        let done = json!({ "pulp_href": "/pulp/api/v3/tasks/2/", "state": "completed", "created_resources": [publication] });
        Mock::given(method("GET")).and(path("/pulp/api/v3/tasks/2/")).respond_with(ok(done)).mount(&server).await;
        svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await.unwrap();
        let sent = server.received_requests().await.unwrap();
        let bodies = |m: &str, p: &str| -> Vec<Value> {
            sent.iter().filter(|r| r.method.as_str() == m && r.url.path() == p).map(|r| r.body_json().unwrap()).collect()
        };
        assert_eq!(bodies("PATCH", FILE_DERIVED), [json!({ "manifest": "CUSTOM" })], "the snapshot repository gets the name");
        let want = json!({ "repository_version": format!("{FILE_DERIVED}versions/3/"), "manifest": "CUSTOM" });
        assert_eq!(bodies("POST", pubs), [want], "pulp_file publishes PULP_MANIFEST unless the request names one");
        assert_eq!(vr.publication, Some(publication));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn new_publish_settings_get_a_new_publication() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (_, mut vr) = rpm_version(&svc).await;
        // The content did not change: the snapshot is the version of the previous publish, with its publication.
        vr.derived_version = Some(format!("{DERIVED}versions/1/"));
        let ok = |body: Value| ResponseTemplate::new(200).set_body_json(body);
        let derived = json!({ "pulp_labels": { LABEL_CV: "web", LABEL_DB: svc.store.db_id() }, "metadata_signing_service": null });
        Mock::given(method("GET")).and(path(DERIVED)).respond_with(ok(derived)).mount(&server).await;
        let source = json!({ "metadata_signing_service": "/pulp/api/v3/signing-services/1/" });
        Mock::given(method("GET")).and(path(vr.repo().repo_href.as_str())).respond_with(ok(source)).mount(&server).await;
        Mock::given(method("PATCH")).and(path(DERIVED)).respond_with(ok(json!({}))).mount(&server).await;
        let task = json!({ "task": "/pulp/api/v3/tasks/1/" });
        Mock::given(method("POST"))
            .and(path("/pulp/api/v3/publications/rpm/rpm/"))
            .respond_with(ResponseTemplate::new(202).set_body_json(task))
            .mount(&server)
            .await;
        let publication = "/pulp/api/v3/publications/rpm/rpm/2/";
        let done =
            json!({ "pulp_href": "/pulp/api/v3/tasks/1/", "state": "completed", "name": "publish", "created_resources": [publication] });
        Mock::given(method("GET")).and(path("/pulp/api/v3/tasks/1/")).respond_with(ok(done)).mount(&server).await;
        svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await.unwrap();
        assert_eq!(vr.publication.as_deref(), Some(publication));
        let sent = server.received_requests().await.unwrap();
        assert!(
            sent.iter().all(|r| r.method.as_str() == "POST" || r.url.path() != "/pulp/api/v3/publications/rpm/rpm/"),
            "no reuse lookup"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_filtered_rpm_snapshot_adds_exactly_the_selection() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (_, mut vr) = rpm_version(&svc).await;
        vr.repo.as_mut().unwrap().filters = crate::filters::parse_rules("exclude name=*-debug", RepoType::Rpm).unwrap();
        let sv = vr.source_version.clone().unwrap();
        let page = |results: Vec<Value>| {
            ResponseTemplate::new(200).set_body_json(json!({ "count": results.len(), "next": null, "results": results }))
        };
        let h = |kind: &str, n: u32| format!("/pulp/api/v3/content/rpm/{kind}/{n}/");
        let pkg =
            |n: u32, name: &str| json!({ "pulp_href": h("packages", n), "name": name, "version": "1", "release": "1", "arch": "x86_64" });
        Mock::given(method("GET"))
            .and(path("/pulp/api/v3/content/rpm/packages/"))
            .respond_with(page(vec![pkg(1, "nginx"), pkg(2, "nginx-debug")]))
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/pulp/api/v3/content/rpm/advisories/")).respond_with(page(vec![])).mount(&server).await;
        // The comps group lists both packages; `rpm/copy/` would add the excluded one with it.
        let source = [h("packages", 1), h("packages", 2), h("packagegroups", 1)];
        let latest = format!("{DERIVED}versions/1/");
        for (version, units) in [(sv.as_str(), &source[..]), (latest.as_str(), &[])] {
            let results = units.iter().map(|u| json!({ "pulp_href": u })).collect();
            Mock::given(method("GET"))
                .and(path("/pulp/api/v3/content/"))
                .and(query_param("repository_version", version))
                .respond_with(page(results))
                .mount(&server)
                .await;
        }
        let derived = json!({ "pulp_href": DERIVED, "name": "web-os", "latest_version_href": latest, "pulp_labels": { LABEL_CV: "web", LABEL_DB: svc.store.db_id() } });
        Mock::given(method("GET")).and(path(DERIVED)).respond_with(ResponseTemplate::new(200).set_body_json(derived)).mount(&server).await;
        let task = json!({ "task": "/pulp/api/v3/tasks/1/" });
        Mock::given(method("POST"))
            .and(path(format!("{DERIVED}modify/")))
            .respond_with(ResponseTemplate::new(202).set_body_json(task))
            .mount(&server)
            .await;
        let created = format!("{DERIVED}versions/2/");
        let done = json!({ "pulp_href": "/pulp/api/v3/tasks/1/", "state": "completed", "name": "modify", "created_resources": [created] });
        Mock::given(method("GET"))
            .and(path("/pulp/api/v3/tasks/1/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(done))
            .mount(&server)
            .await;
        let version = json!({ "pulp_href": created, "number": 2 });
        Mock::given(method("GET")).and(path(&created)).respond_with(ResponseTemplate::new(200).set_body_json(version)).mount(&server).await;
        svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await.unwrap_err(); // the publish step is not served
        assert_eq!(modify_bodies(&server).await, [json!({ "add_content_units": [h("packages", 1), h("packagegroups", 1)] })]);
        let saved = svc.store.version_repos(vr.version_id).await.unwrap().remove(0);
        assert_eq!(saved.derived_version, Some(created));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_snapshot_changes_the_latest_version_only_where_it_differs() {
        // Pulp's delete of a version is very slow when its removed units come back in the next one.
        let stale: Vec<String> = (0..2).map(|i| format!("/pulp/api/v3/content/file/files/stale{i}/")).collect();
        for changed in [true, false] {
            let server = wiremock::MockServer::start().await;
            let svc = mock_service(&server).await;
            let (mut vr, hrefs) = file_copy(&server, &svc, 30_000).await;
            let present = if changed { [&stale[..], &hrefs[10_000..]].concat() } else { hrefs.clone() };
            serve_content(&server, &present).await;
            svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await.unwrap_err(); // the publish step is not served
            let bodies = modify_bodies(&server).await;
            let saved = svc.store.version_repos(vr.version_id).await.unwrap().remove(0);
            assert_eq!(saved.derived_version, Some(format!("{FILE_DERIVED}versions/3/")));
            if changed {
                assert_eq!(bodies, [json!({ "remove_content_units": stale }), json!({ "add_content_units": hrefs[..10_000] })]);
            } else {
                assert!(bodies.is_empty(), "nothing is sent when the latest version is the selection: {bodies:?}");
            }
        }
    }

    #[tokio::test]
    async fn a_deb_snapshot_removes_the_release_records_of_its_removed_packages() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let h = |kind: &str, n: u32| format!("/pulp/api/v3/content/deb/{kind}/{n}/");
        let latest = format!("{DEB_DERIVED}versions/4/");
        let derived = json!({ "pulp_href": DEB_DERIVED, "name": "web-apt", "latest_version_href": latest });
        Mock::given(method("GET"))
            .and(path(DEB_DERIVED))
            .respond_with(ResponseTemplate::new(200).set_body_json(derived))
            .mount(&server)
            .await;
        let present = [
            h("packages", 1),
            h("packages", 2),
            h("package_release_components", 1),
            h("package_release_components", 2),
            h("releases", 1),
            h("release_components", 1),
            h("release_architectures", 1),
            h("release_files", 1),
        ];
        serve_content(&server, &present).await;
        let prc = |n: u32| json!({ "pulp_href": h("package_release_components", n), "package": h("packages", n) });
        Mock::given(method("GET"))
            .and(path("/pulp/api/v3/content/deb/package_release_components/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "count": 2, "next": null, "results": [prc(1), prc(2)] })))
            .mount(&server)
            .await;
        let (remove, add) = svc.changes(RepoType::Deb, DEB_DERIVED, vec![h("packages", 1), h("packages", 3)]).await.unwrap();
        assert_eq!(remove, [h("packages", 2), h("package_release_components", 2), h("release_files", 1)]);
        assert_eq!(add, [h("packages", 3)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn versions_no_content_view_uses_are_deleted_after_the_copy() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (mut vr, _) = file_copy(&server, &svc, 3).await;
        serve_content(&server, &[]).await;
        let v = |n: u64| format!("{FILE_DERIVED}versions/{n}/");
        // Version 1 is the snapshot of another Content View version; version 2 is a batch nobody uses.
        let cv = svc.store.version(vr.version_id).await.unwrap().cv_id;
        let other = svc.store.begin_version(cv, "", &[vr.repo().clone()]).await.unwrap();
        let mut row = svc.store.version_repos(other.id).await.unwrap().remove(0);
        (row.derived_repo, row.derived_version) = (Some(FILE_DERIVED.into()), Some(v(1)));
        svc.store.save_version_repo(&row).await.unwrap();
        let versions: Vec<Value> = (0..5).map(|n| json!({ "pulp_href": v(n), "number": n })).collect();
        let page = json!({ "count": 5, "next": null, "results": versions });
        let list = format!("{FILE_DERIVED}versions/");
        Mock::given(method("GET")).and(path(list)).respond_with(ResponseTemplate::new(200).set_body_json(page)).mount(&server).await;
        // Version 4 is in no view version, but a distribution serves its publication: no delete is sent.
        let p4 = json!({ "pulp_href": "/pulp/api/v3/publications/file/file/4/", "repository_version": v(4) });
        let d =
            json!({ "pulp_href": "/d/1/", "name": "partial", "base_path": "p", "publication": "/pulp/api/v3/publications/file/file/4/" });
        for (coll, results) in [("publications/file/file/", p4), ("distributions/file/file/", d)] {
            Mock::given(method("GET"))
                .and(path(format!("/pulp/api/v3/{coll}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "count": 1, "next": null, "results": [results] })))
                .with_priority(1)
                .mount(&server)
                .await;
        }
        let task = json!({ "task": "/pulp/api/v3/tasks/2/" });
        Mock::given(method("DELETE")).and(path(v(2))).respond_with(ResponseTemplate::new(202).set_body_json(task)).mount(&server).await;
        let done = json!({ "pulp_href": "/pulp/api/v3/tasks/2/", "state": "completed", "name": "delete" });
        Mock::given(method("GET"))
            .and(path("/pulp/api/v3/tasks/2/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(done))
            .mount(&server)
            .await;
        let lines = Mutex::new(vec![]);
        svc.publish_repo("web", &mut vr, &|s| lines.lock().unwrap().push(s), &ops::no_progress).await.unwrap_err(); // publish not served
        let sent = server.received_requests().await.unwrap();
        let deleted: Vec<_> = sent.iter().filter(|r| r.method.as_str() == "DELETE").map(|r| r.url.path().to_string()).collect();
        assert_eq!(deleted, [v(2)], "not version 0, the one another view version uses, the new snapshot, or a served one");
        let lines = lines.lock().unwrap();
        assert!(lines.contains(&format!("deleted unused version {}", v(2))), "{lines:?}");
        assert!(lines.contains(&format!("keeping {}: served by partial (pinned to a publication of this version)", v(4))), "{lines:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_records_the_latest_version_when_no_batch_is_left() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (mut vr, hrefs) = file_copy(&server, &svc, 3).await;
        // The interrupted batch made version 2. A newer publish with the same selection then made version 3.
        vr.pending_task = Some("/pulp/api/v3/tasks/1/".into());
        svc.store.save_version_repo(&vr).await.unwrap();
        let v2 = format!("{FILE_DERIVED}versions/2/");
        let done = json!({ "pulp_href": "/pulp/api/v3/tasks/1/", "state": "completed", "created_resources": [v2] });
        let ok = |body: Value| ResponseTemplate::new(200).set_body_json(body);
        Mock::given(method("GET")).and(path("/pulp/api/v3/tasks/1/")).respond_with(ok(done)).with_priority(1).mount(&server).await;
        Mock::given(method("GET")).and(path(v2.as_str())).respond_with(ok(json!({ "pulp_href": v2, "number": 2 }))).mount(&server).await;
        serve_content(&server, &hrefs).await;
        svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await.unwrap_err(); // the publish step is not served
        assert!(modify_bodies(&server).await.is_empty(), "the latest version has the selection: nothing is sent");
        let saved = svc.store.version_repos(vr.version_id).await.unwrap().remove(0);
        assert_eq!(saved.derived_version, Some(format!("{FILE_DERIVED}versions/3/")), "version 2 can lack later batches");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_sends_only_the_batches_not_in_the_version() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (mut vr, hrefs) = file_copy(&server, &svc, 30_000).await;
        // The first batch was sent and completed before the client stopped; its version has those units.
        vr.pending_task = Some("/pulp/api/v3/tasks/1/".into());
        svc.store.save_version_repo(&vr).await.unwrap();
        serve_content(&server, &hrefs[..20_000]).await;
        svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await.unwrap_err(); // the publish step is not served
        let bodies = modify_bodies(&server).await;
        assert_eq!(bodies.len(), 1, "only the rest is sent");
        assert!(bodies[0].get("remove_content_units").is_none(), "added to the version of the earlier batches");
        assert_eq!(bodies[0]["add_content_units"], json!(hrefs[20_000..]));
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
        let svc = publishing(svc, cv, vr.version_id).await;
        let pubs = "/pulp/api/v3/publications/deb/apt/";
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(pubs))
            .respond_with(LoseLockOnPost(svc.lock.as_ref().unwrap().lost.clone()))
            .mount(&server)
            .await;
        // The source is published simple, flat and with its own signing service.
        let source = json!({ "pulp_href": "/p/1/", "repository_version": "/pulp/api/v3/repositories/deb/apt/1/versions/1/",
            "simple": true, "structured": false, "layout": "flat", "signing_service": "/ss/9/" });
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(pubs))
            .and(wiremock::matchers::query_param("repository", "/pulp/api/v3/repositories/deb/apt/1/"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({ "count": 1, "next": null, "results": [source] })))
            .mount(&server)
            .await;
        let e = svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await.unwrap_err();
        assert!(e.to_string().starts_with("lock on web lost: "), "{e}");
        let saved = svc.store.version_repos(vr.version_id).await.unwrap().remove(0);
        assert_eq!((saved.pending_task.as_deref(), saved.publication), (Some("/pulp/api/v3/tasks/1/"), None), "Resume re-attaches");
        let sent = server.received_requests().await.unwrap();
        assert!(!sent.iter().any(|r| r.url.query().unwrap_or_default().contains("repository_version")), "no reuse lookup");
        let posts: Vec<Value> = sent.iter().filter(|r| r.method.as_str() == "POST").map(|r| r.body_json().unwrap()).collect();
        let mode =
            json!({ "repository_version": format!("{DEB_DERIVED}versions/1/"), "simple": true, "structured": false, "layout": "flat" });
        assert_eq!(posts, [mode], "the source's mode; the derived repository's keys apply");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_after_the_copy_never_reuses_a_publication() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let pubs = "/pulp/api/v3/publications/rpm/rpm/";
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (cv, mut vr) = rpm_version(&svc).await;
        // An earlier run recorded the snapshot (the content did not change), sent the settings PATCH, and stopped. The
        // settings agree now; the publication of the previous publish has the old ones (for example no signing).
        vr.derived_version = Some(format!("{DERIVED}versions/1/"));
        svc.store.save_version_repo(&vr).await.unwrap();
        serve_owner(&server, DERIVED, svc.store.db_id()).await;
        serve_labels(&server, &vr.repo().repo_href, json!({})).await;
        let old = json!({ "count": 1, "next": null, "results": [{ "pulp_href": format!("{pubs}2/") }] });
        Mock::given(method("GET")).and(path(pubs)).respond_with(ResponseTemplate::new(200).set_body_json(old)).mount(&server).await;
        let svc = publishing(svc, cv, vr.version_id).await;
        let post = LoseLockOnPost(svc.lock.as_ref().unwrap().lost.clone());
        Mock::given(method("POST")).and(path(pubs)).respond_with(post).mount(&server).await;
        let res = svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await;
        assert!(res.is_err_and(|e| e.to_string().starts_with("lock on web lost: ")), "a new publication is sent");
        let saved = svc.store.version_repos(vr.version_id).await.unwrap().remove(0);
        assert_eq!((saved.pending_task.as_deref(), saved.publication), (Some("/pulp/api/v3/tasks/1/"), None));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_rpm_publication_is_reused_or_recorded_before_the_wait() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let pubs = "/pulp/api/v3/publications/rpm/rpm/";
        for existing in [Some("/pulp/api/v3/publications/rpm/rpm/2/"), None] {
            let server = wiremock::MockServer::start().await;
            let svc = mock_service(&server).await;
            let (cv, mut vr) = rpm_version(&svc).await;
            // The copy of this run finds the content of the previous publish (no new version); the publish settings
            // did not change.
            let ok = |body: Value| ResponseTemplate::new(200).set_body_json(body);
            let (v1, labels) = (format!("{DERIVED}versions/1/"), json!({ LABEL_CV: "web", LABEL_DB: svc.store.db_id() }));
            let derived = json!({ "pulp_href": DERIVED, "name": "web-os", "pulp_labels": labels, "latest_version_href": v1 });
            Mock::given(method("GET")).and(path(DERIVED)).respond_with(ok(derived)).mount(&server).await;
            Mock::given(method("GET"))
                .and(path(v1.as_str()))
                .respond_with(ok(json!({ "pulp_href": v1, "number": 1 })))
                .mount(&server)
                .await;
            let copy = ResponseTemplate::new(202).set_body_json(json!({ "task": "/pulp/api/v3/tasks/7/" }));
            Mock::given(method("POST")).and(path("/pulp/api/v3/rpm/copy/")).respond_with(copy).mount(&server).await;
            let done = json!({ "pulp_href": "/pulp/api/v3/tasks/7/", "state": "completed", "created_resources": [] });
            Mock::given(method("GET")).and(path("/pulp/api/v3/tasks/7/")).respond_with(ok(done)).mount(&server).await;
            serve_labels(&server, &vr.repo().repo_href, json!({})).await;
            let found: Vec<Value> = existing.iter().map(|p| json!({ "pulp_href": p })).collect();
            let page = json!({ "count": found.len(), "next": null, "results": found });
            Mock::given(method("GET")).and(path(pubs)).respond_with(ResponseTemplate::new(200).set_body_json(page)).mount(&server).await;
            let svc = publishing(svc, cv, vr.version_id).await;
            Mock::given(method("POST"))
                .and(path(pubs))
                .respond_with(LoseLockOnPost(svc.lock.as_ref().unwrap().lost.clone()))
                .mount(&server)
                .await;
            let res = svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await;
            let saved = svc.store.version_repos(vr.version_id).await.unwrap().remove(0);
            if let Some(p) = existing {
                res.unwrap();
                assert_eq!(saved.publication.as_deref(), Some(p));
                let sent = server.received_requests().await.unwrap();
                assert!(sent.iter().all(|r| r.method.as_str() != "POST" || r.url.path() != pubs), "reused: {sent:?}");
            } else {
                assert!(res.unwrap_err().to_string().starts_with("lock on web lost: "));
                let want = (Some("/pulp/api/v3/tasks/1/"), None);
                assert_eq!((saved.pending_task.as_deref(), saved.publication), want, "Watch task opens it, Resume re-attaches");
            }
        }
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
        let derived = json!({ "pulp_labels": { LABEL_CV: "web", LABEL_DB: svc.store.db_id() }, "signing_service": null, "signing_service_release_overrides": {} });
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
        let reads = server.received_requests().await.unwrap().iter().filter(|r| r.url.path() == "/pulp/api/v3/tasks/9/").count();
        assert_eq!(reads, 1, "a finished task is read once (only a task that cannot be read is checked for 404)");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_does_not_copy_into_a_repository_the_view_does_not_own() {
        // (view label, database ID label: `None` = this database, why)
        for (cv, db, why) in
            [("web", Some("other"), "is owned by another pulp-tui database (other)"), ("api", None, "is not labeled pulp_tui_cv=web")]
        {
            let server = wiremock::MockServer::start().await;
            let svc = mock_service(&server).await;
            let (_, mut vr) = rpm_version(&svc).await;
            serve_labels(&server, DERIVED, json!({ LABEL_CV: cv, LABEL_DB: db.unwrap_or(svc.store.db_id()) })).await;
            let e = svc.publish_repo("web", &mut vr, &|_| {}, &ops::no_progress).await.unwrap_err();
            assert_eq!(e.to_string(), format!("repository pulp-tui-cv-web-os {why}"));
            assert_none(&server, "POST").await;
            assert_none(&server, "PATCH").await;
        }
    }

    /// View `a` with repository `b-c` derives the name of view `a-b` with repository `c` (the format stays: existing
    /// objects keep their names). The refusal names the other view; an unlabeled repository is refused as before.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_derived_name_of_another_view_is_refused_with_that_view() {
        use wiremock::matchers::{method, path, query_param};
        let other = "repository pulp-tui-cv-a-b-c belongs to content view a-b (label pulp_tui_cv=a-b): \
                     view a with repository b-c derives the same name; refusing to modify it";
        for (labels, want) in [
            (json!({ LABEL_CV: "a-b" }), other),
            (json!({}), "repository pulp-tui-cv-a-b-c exists but is not labeled pulp_tui_cv=a; refusing to modify it"),
        ] {
            let server = wiremock::MockServer::start().await;
            let svc = mock_service(&server).await;
            let cv = svc.store.create_cv("k", "a", "").await.unwrap();
            let src = CvRepo {
                repo_href: "/pulp/api/v3/repositories/rpm/rpm/1/".into(),
                repo_name: "b-c".into(),
                repo_type: RepoType::Rpm,
                filters: vec![],
            };
            let v = svc.store.begin_version(cv, "", &[src]).await.unwrap();
            let mut vr = svc.store.version_repos(v.id).await.unwrap().remove(0);
            let found = json!({ "pulp_href": DERIVED, "name": "pulp-tui-cv-a-b-c", "latest_version_href": null, "pulp_labels": labels });
            wiremock::Mock::given(method("GET"))
                .and(path("/pulp/api/v3/repositories/rpm/rpm/"))
                .and(query_param("name", "pulp-tui-cv-a-b-c"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({ "count": 1, "next": null, "results": [found] })))
                .mount(&server)
                .await;
            let e = svc.derived_repo("a", &mut vr, &|_| {}, &ops::no_progress).await.unwrap_err();
            assert_eq!(e.to_string(), want);
            assert_eq!(vr.derived_repo, None, "nothing recorded");
            assert_none(&server, "PATCH").await;
            assert_none(&server, "POST").await;
        }
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
    async fn delete_version_keeps_objects_of_a_repository_the_view_does_not_own() {
        // (view label, database ID label: `None` = this database, why)
        for (cv, db, why) in
            [("web", Some("other"), "owned by another pulp-tui database (other)"), ("api", None, "not labeled pulp_tui_cv=web")]
        {
            let server = wiremock::MockServer::start().await;
            let svc = mock_service(&server).await;
            let (_, mut vr) = rpm_version(&svc).await;
            vr.publication = Some("/pulp/api/v3/publications/rpm/rpm/3/".into());
            vr.derived_version = Some(format!("{DERIVED}versions/1/"));
            svc.store.save_version_repo(&vr).await.unwrap();
            serve_labels(&server, DERIVED, json!({ LABEL_CV: cv, LABEL_DB: db.unwrap_or(svc.store.db_id()) })).await;
            let logs = Mutex::new(vec![]);
            svc.delete_version(vr.version_id, &|m| logs.lock().unwrap().push(m), &ops::no_progress).await.unwrap();
            assert_none(&server, "DELETE").await;
            let logs = logs.into_inner().unwrap();
            let kept = |h: &str| format!("keeping {h}: {why}");
            assert_eq!(logs, [kept(vr.publication.as_deref().unwrap()), kept(vr.derived_version.as_deref().unwrap())]);
            assert_eq!(svc.store.version(vr.version_id).await.unwrap().state, "deleted");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_version_skips_a_gone_object_and_stops_on_a_refused_delete() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (_, mut vr) = rpm_version(&svc).await;
        let (p, dv) = ("/pulp/api/v3/publications/rpm/rpm/3/".to_string(), format!("{DERIVED}versions/1/"));
        (vr.publication, vr.derived_version) = (Some(p.clone()), Some(dv.clone()));
        svc.store.save_version_repo(&vr).await.unwrap();
        svc.store.finish_version(vr.version_id, "published", None).await.unwrap();
        serve_owner(&server, DERIVED, svc.store.db_id()).await;
        serve_snapshot(&server, json!([]), json!([])).await; // no distribution serves them
        // The publication is already gone; the first delete of the version is refused.
        Mock::given(method("DELETE")).and(path(p.as_str())).respond_with(ResponseTemplate::new(404)).mount(&server).await;
        Mock::given(method("DELETE")).and(path(dv.as_str())).respond_with(ResponseTemplate::new(403)).up_to_n_times(1).mount(&server).await;
        Mock::given(method("DELETE")).and(path(dv.as_str())).respond_with(ResponseTemplate::new(204)).mount(&server).await;
        let e = svc.delete_version(vr.version_id, &|_| {}, &ops::no_progress).await.unwrap_err();
        assert_eq!(e.to_string(), "permission denied [403]");
        assert_eq!(svc.store.version(vr.version_id).await.unwrap().state, "deleting", "a failed delete stays deleting");
        svc.delete_version(vr.version_id, &|_| {}, &ops::no_progress).await.unwrap();
        assert_eq!(svc.store.version(vr.version_id).await.unwrap().state, "deleted", "the rerun deletes what is left");
        let sent = server.received_requests().await.unwrap();
        let deletes: Vec<_> = sent.iter().filter(|r| r.method.as_str() == "DELETE").map(|r| r.url.path().to_string()).collect();
        assert_eq!(deletes, [p.clone(), dv.clone(), p, dv]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreadable_ownership_label_stops_the_delete() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (_, mut vr) = rpm_version(&svc).await;
        vr.publication = Some("/pulp/api/v3/publications/rpm/rpm/3/".into());
        svc.store.save_version_repo(&vr).await.unwrap();
        // The labels of the derived repository cannot be read: whose it is stays unknown.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(DERIVED))
            .respond_with(wiremock::ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let e = svc.delete_version_impact(vr.version_id).await.unwrap_err();
        assert_eq!(e.to_string(), "permission denied [403]");
        let e = svc.delete_version(vr.version_id, &|_| {}, &ops::no_progress).await.unwrap_err();
        assert_eq!(e.to_string(), "permission denied [403]");
        assert_none(&server, "DELETE").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unbind_does_not_touch_a_distribution_the_view_does_not_own() {
        // (environment label, database ID label: `None` = this database, why)
        let cases = [
            (LIBRARY, Some("other"), "is owned by another pulp-tui database (other)"),
            ("Dev", None, "is not labeled pulp_tui_env=Library"),
        ];
        for (env, db, why) in cases {
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
            serve_labels(&server, DIST, json!({ LABEL_CV: "web", LABEL_ENV: env, LABEL_DB: db.unwrap_or(svc.store.db_id()) })).await;
            let e = svc.promote(cv, v.id, lib, Mode::Rollback, &|_| {}, &ops::no_progress).await.unwrap_err();
            let want = format!("unbind old: distribution {} {why}", distribution_name("web", LIBRARY, "old"));
            assert!(e.to_string().ends_with(&want), "{e}");
            assert_none(&server, "PATCH").await;
            assert_eq!(svc.store.binding_repos(lib, cv).await.unwrap()[0].publication, old.publication, "still recorded as served");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_repository_left_out_of_the_version_stops_serving() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let cv = svc.store.create_cv("k", "web", "").await.unwrap();
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
            base_path: "library/web/old".into(),
            publication: Some("/pulp/api/v3/publications/rpm/rpm/3/".into()),
        };
        svc.store.save_binding_repo(&old).await.unwrap();
        serve_labels(&server, DIST, json!({ LABEL_CV: "web", LABEL_ENV: LIBRARY, LABEL_DB: svc.store.db_id() })).await;
        let task = json!({ "task": "/pulp/api/v3/tasks/1/" });
        let patch = Mock::given(method("PATCH")).and(path(DIST)).and(body_json(json!({ "publication": null })));
        patch.respond_with(ResponseTemplate::new(202).set_body_json(task)).expect(1).mount(&server).await;
        let done = json!({ "pulp_href": "/pulp/api/v3/tasks/1/", "state": "completed", "name": "update" });
        Mock::given(method("GET"))
            .and(path("/pulp/api/v3/tasks/1/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(done))
            .mount(&server)
            .await;
        svc.promote(cv, v.id, lib, Mode::Rollback, &|_| {}, &ops::no_progress).await.unwrap();
        let rows = svc.store.binding_repos(lib, cv).await.unwrap();
        assert_eq!((rows.len(), rows[0].publication.as_deref()), (1, None), "the distribution is kept, it serves nothing");
        assert_eq!(svc.store.binding(lib, cv).await.unwrap().unwrap().active(), Some(v.id));
        // A rerun sends nothing: the row serves nothing now.
        svc.promote(cv, v.id, lib, Mode::Rollback, &|_| {}, &ops::no_progress).await.unwrap();
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
        assert_eq!(svc.store.binding(lib, cv).await.unwrap().unwrap().attempt.unwrap().state, "partial", "os is served: not failed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unpromote_deletes_the_owned_distributions_and_forgets_the_view() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (dev, cv) = web_in_dev(&svc).await;
        let (ours, theirs, test) =
            ("/pulp/api/v3/distributions/rpm/rpm/1/", "/pulp/api/v3/distributions/rpm/rpm/2/", "/pulp/api/v3/distributions/rpm/rpm/4/");
        for (i, (repo, href)) in [("os", ours), ("mirror", theirs), ("epel", test)].into_iter().enumerate() {
            let b = BindingRepo {
                env_id: dev,
                cv_id: cv,
                repo_href: format!("/pulp/api/v3/repositories/rpm/rpm/{i}/"),
                repo_name: repo.into(),
                repo_type: RepoType::Rpm,
                distribution: href.into(),
                base_path: format!("dev/web/{repo}"),
                publication: Some("/pulp/api/v3/publications/rpm/rpm/3/".into()),
            };
            svc.store.save_binding_repo(&b).await.unwrap();
        }
        serve_owner(&server, ours, svc.store.db_id()).await;
        serve_owner(&server, theirs, "other").await;
        // Ours by database ID, but labeled for another environment.
        serve_labels(&server, test, json!({ LABEL_CV: "web", LABEL_ENV: "Test", LABEL_DB: svc.store.db_id() })).await;
        Mock::given(method("DELETE")).and(path(ours)).respond_with(ResponseTemplate::new(204)).mount(&server).await;
        let logs = Mutex::new(vec![]);
        svc.unpromote(cv, dev, &|m| logs.lock().unwrap().push(m), &ops::no_progress).await.unwrap();
        let sent = server.received_requests().await.unwrap();
        let deleted: Vec<&str> = sent.iter().filter(|r| r.method.as_str() == "DELETE").map(|r| r.url.path()).collect();
        assert_eq!(deleted, [ours]);
        let logs = logs.into_inner().unwrap();
        assert!(logs.contains(&format!("keeping {theirs}: owned by another pulp-tui database (other)")), "{logs:?}");
        assert!(logs.contains(&format!("keeping {test}: not labeled pulp_tui_env=Dev")), "{logs:?}");
        assert!(svc.store.binding(dev, cv).await.unwrap().is_none());
        assert!(svc.store.binding_repos(dev, cv).await.unwrap().is_empty());
        let e = svc.unpromote(cv, dev, &|_| {}, &ops::no_progress).await.unwrap_err();
        assert_eq!(e.to_string(), "web is not in Dev");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_view_deletes_owned_objects_and_keeps_those_of_another_database() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (dev, cv) = web_in_dev(&svc).await;
        let (ours, theirs, gone, bare) = (
            "/pulp/api/v3/distributions/rpm/rpm/1/",
            "/pulp/api/v3/distributions/rpm/rpm/2/",
            "/pulp/api/v3/distributions/rpm/rpm/3/",
            "/pulp/api/v3/distributions/rpm/rpm/4/",
        );
        for (i, href) in [ours, theirs, gone, bare].into_iter().enumerate() {
            let b = BindingRepo {
                env_id: dev,
                cv_id: cv,
                repo_href: format!("/pulp/api/v3/repositories/rpm/rpm/{i}/"),
                repo_name: format!("r{i}"),
                repo_type: RepoType::Rpm,
                distribution: href.into(),
                base_path: format!("dev/web/r{i}"),
                publication: None,
            };
            svc.store.save_binding_repo(&b).await.unwrap();
        }
        serve_owner(&server, ours, svc.store.db_id()).await;
        serve_owner(&server, theirs, "other").await;
        serve_labels(&server, bare, json!({ LABEL_ENV: "Dev", LABEL_DB: svc.store.db_id() })).await;
        // A snapshot repository carries no environment label.
        serve_labels(&server, DERIVED, json!({ LABEL_CV: "web", LABEL_DB: svc.store.db_id() })).await;
        Mock::given(method("DELETE")).and(path(ours)).respond_with(ResponseTemplate::new(204)).mount(&server).await;
        // A snapshot of web, and another view that serves a publication that is not web's: no refusal.
        let v = published(&svc, cv, "os", "/pulp/api/v3/repositories/rpm/rpm/9/").await;
        let row = svc.store.version_repos(v.id).await.unwrap().remove(0);
        // In Pulp, web's own distribution serves the snapshot; another serves a version and a publication of another repository.
        let pubs = json!([{ "pulp_href": row.publication, "repository_version": row.derived_version }]);
        let dists = json!([
            { "pulp_href": ours, "name": "pulp-tui-cv-web-Dev-r0", "base_path": "dev/web/r0", "publication": row.publication },
            { "pulp_href": "/d/8/", "name": "other", "base_path": "o", "repository_version": "/pulp/api/v3/repositories/rpm/rpm/8/versions/1/" },
            { "pulp_href": "/d/9/", "name": "other2", "base_path": "o2", "publication": "/pulp/api/v3/publications/rpm/rpm/99/" },
        ]);
        serve_snapshot(&server, pubs, dists).await;
        let api = svc.store.create_cv("k", "api", "").await.unwrap();
        let served = BindingRepo {
            env_id: dev,
            cv_id: api,
            repo_href: "/pulp/api/v3/repositories/rpm/rpm/8/".into(),
            repo_name: "os".into(),
            repo_type: RepoType::Rpm,
            distribution: "/pulp/api/v3/distributions/rpm/rpm/8/".into(),
            base_path: "dev/api/os".into(),
            publication: Some("/pulp/api/v3/publications/rpm/rpm/99/".into()),
        };
        svc.store.save_binding_repo(&served).await.unwrap();
        let logs = Mutex::new(vec![]);
        svc.delete_view(cv, &|m| logs.lock().unwrap().push(m), &ops::no_progress).await.unwrap();
        let sent = server.received_requests().await.unwrap();
        let mut deleted: Vec<&str> = sent.iter().filter(|r| r.method.as_str() == "DELETE").map(|r| r.url.path()).collect();
        deleted.sort();
        assert_eq!(deleted, [ours, gone, DERIVED], "a 404 on delete is fine");
        let logs = logs.into_inner().unwrap();
        assert!(logs.contains(&format!("keeping {theirs}: owned by another pulp-tui database (other)")), "{logs:?}");
        assert!(logs.contains(&format!("keeping {bare}: not labeled pulp_tui_cv=web")), "{logs:?}");
        let left: Vec<String> = svc.store.content_views("k").await.unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(left, ["api"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unpromote_is_refused_while_a_later_environment_has_the_view() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        // Library -> Dev -> Test, the view in all three; later Test only gets it back (a rollback there), not Dev.
        let (dev, cv) = web_in_dev(&svc).await;
        let (lib, v) = (svc.store.ensure_library("k").await.unwrap(), svc.store.binding(dev, cv).await.unwrap().unwrap().served.unwrap());
        let test = svc.store.create_env("k", "Test", dev, false).await.unwrap();
        svc.store.promoted(lib, cv, v, "active").await.unwrap();
        svc.store.promoted(test, cv, v, "active").await.unwrap();
        let e = svc.unpromote(cv, dev, &|_| {}, &ops::no_progress).await.unwrap_err();
        assert_eq!(e.to_string(), "web is still in Test; unpromote it there first");
        svc.store.delete_binding(test, cv).await.unwrap();
        svc.unpromote(cv, dev, &|_| {}, &ops::no_progress).await.unwrap();
        svc.store.promoted(test, cv, v, "active").await.unwrap();
        let e = svc.unpromote(cv, lib, &|_| {}, &ops::no_progress).await.unwrap_err();
        assert_eq!(e.to_string(), "web is still in Test; unpromote it there first");
        assert!(svc.store.binding(lib, cv).await.unwrap().is_some());
        svc.unpromote(cv, test, &|_| {}, &ops::no_progress).await.unwrap();
        svc.unpromote(cv, lib, &|_| {}, &ops::no_progress).await.unwrap();
        assert!(svc.store.version_in_use(v).await.unwrap().is_empty(), "the version can be deleted now");
    }

    /// Scope "k": Library, Dev (label "dev"), view "web" with a published version bound to Dev.
    async fn web_in_dev(svc: &CvService) -> (i64, i64) {
        let lib = svc.store.ensure_library("k").await.unwrap();
        let dev = svc.store.create_env("k", "Dev", lib, false).await.unwrap();
        let cv = svc.store.create_cv("k", "web", "").await.unwrap();
        let v = svc.store.begin_version(cv, "", &[]).await.unwrap();
        svc.store.finish_version(v.id, "published", None).await.unwrap();
        svc.store.promoted(dev, cv, v.id, "active").await.unwrap();
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
        // Under the new label: only distributions of this environment (pkgs, moved by an earlier PATCH).
        let pkgs = json!({ "pulp_href": "/pulp/api/v3/distributions/rpm/rpm/2/", "name": "pkgs", "base_path": "qa/web/pkgs" });
        let list = json!({ "count": 1, "next": null, "results": [pkgs] });
        Mock::given(method("GET"))
            .and(path("/pulp/api/v3/distributions/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list))
            .mount(&server)
            .await;
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

    /// Pulp refuses a base path that equals, contains or is inside another one: a label whose served paths would
    /// overlap another distribution is refused before it is stored and before any PATCH.
    #[tokio::test(flavor = "multi_thread")]
    async fn relabel_is_refused_before_the_label_is_stored_when_a_base_path_overlaps() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let (dev, cv) = web_in_dev(&svc).await;
        let own = "/pulp/api/v3/distributions/rpm/rpm/1/";
        let row = BindingRepo {
            env_id: dev,
            cv_id: cv,
            repo_href: "/pulp/api/v3/repositories/rpm/rpm/1/".into(),
            repo_name: "os".into(),
            repo_type: RepoType::Rpm,
            distribution: own.into(),
            base_path: "dev/web/os".into(),
            publication: None,
        };
        svc.store.save_binding_repo(&row).await.unwrap();
        // `base_path__contains` narrows the list; the overlap is checked on each path.
        let dists = |found: &[(&str, &str)]| {
            let results: Vec<Value> =
                found.iter().map(|(h, p)| json!({ "pulp_href": h, "name": format!("d{p}"), "base_path": p })).collect();
            ResponseTemplate::new(200).set_body_json(json!({ "count": results.len(), "next": null, "results": results }))
        };
        for (label, found) in [
            ("qa", vec![("/d/1/", "qa/legacy"), ("/d/2/", "aqa/x")]),
            ("stage", vec![("/d/3/", "stage")]),
            ("dev", vec![(own, "dev/web/os")]), // its own row: a rerun with the same label
            ("prod", vec![("/d/4/", "x/prod"), ("/d/5/", "prod-old/x"), ("/d/6/", "prodx")]),
        ] {
            let q = Mock::given(method("GET")).and(path("/pulp/api/v3/distributions/")).and(query_param("base_path__contains", label));
            q.respond_with(dists(&found)).mount(&server).await;
        }
        for (label, clash) in [("qa", "dqa/legacy has base path /qa/legacy"), ("stage", "dstage has base path /stage")] {
            let want = format!(
                "label {label} is refused: distribution {clash}, the label or below it (served paths would overlap it; Pulp refuses that)"
            );
            assert_eq!(svc.relabel_impact(dev, label).await.unwrap_err().to_string(), want, "the preview refuses");
            assert_eq!(svc.relabel_env(dev, label, &|_| {}, &ops::no_progress).await.unwrap_err().to_string(), want);
            assert_eq!(svc.store.environments("k").await.unwrap()[1].label, "dev", "not stored");
        }
        assert_none(&server, "PATCH").await;
        assert!(svc.relabel_impact(dev, "dev").await.unwrap().is_empty());
        assert_eq!(svc.relabel_impact(dev, "prod").await.unwrap()[0].1, "prod/web/os", "a path that only contains the label");
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

    #[tokio::test(flavor = "multi_thread")]
    async fn a_promotion_reports_a_relabel_that_missed_its_rows() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let lib = svc.store.ensure_library("k").await.unwrap();
        let dev = svc.store.create_env("k", "Dev", lib, false).await.unwrap();
        let cv = svc.store.create_cv("k", "web", "").await.unwrap();
        let v = published(&svc, cv, "os", "/pulp/api/v3/repositories/rpm/rpm/1/").await;
        let ok = |body: Value| ResponseTemplate::new(200).set_body_json(body);
        let none = json!({ "count": 0, "next": null, "results": [] });
        for p in ["/pulp/api/v3/distributions/rpm/rpm/", "/pulp/api/v3/distributions/"] {
            Mock::given(method("GET")).and(path(p)).respond_with(ok(none.clone())).mount(&server).await;
        }
        let task = ResponseTemplate::new(202).set_body_json(json!({ "task": "/pulp/api/v3/tasks/1/" }));
        Mock::given(method("POST")).and(path("/pulp/api/v3/distributions/rpm/rpm/")).respond_with(task).mount(&server).await;
        let done = json!({ "pulp_href": "/pulp/api/v3/tasks/1/", "state": "completed", "name": "create", "created_resources": [DIST] });
        Mock::given(method("GET")).and(path("/pulp/api/v3/tasks/1/")).respond_with(ok(done)).mount(&server).await;
        // The view is not in Dev yet, so a relabel does not lock it. The relabel stores `qa` and lists its rows
        // after this promotion read `dev` and before it saves its row.
        let relabel = |_: String| {
            tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(svc.store.set_env_label(dev, "qa"))).unwrap()
        };
        svc.store.promoted(lib, cv, v.id, "active").await.unwrap();
        let e = svc.promote(cv, v.id, dev, Mode::Promote, &relabel, &ops::no_progress).await.unwrap_err();
        assert!(e.to_string().ends_with("partial: Dev was relabeled qa during the promotion; edit Dev again with label qa"), "{e}");
        assert_eq!(svc.store.binding(dev, cv).await.unwrap().unwrap().attempt.unwrap().state, "partial");
        let moves: Vec<_> = svc.relabel_impact(dev, "qa").await.unwrap().into_iter().map(|(b, new)| (b.base_path, new)).collect();
        assert_eq!(moves, [("dev/web/os".to_string(), "qa/web/os".to_string())], "a rerun of the relabel moves the row");
    }

    #[tokio::test]
    async fn rollback_after_library_goes_back_only() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let lib = svc.store.ensure_library("k").await.unwrap();
        let test = svc.store.create_env("k", "Test", lib, false).await.unwrap();
        let cv = svc.store.create_cv("k", "web", "").await.unwrap();
        let mut v = vec![];
        for _ in 0..3 {
            v.push(published(&svc, cv, "os", "/pulp/api/v3/repositories/rpm/rpm/1/").await);
        }
        let rollback = |n: usize, env: i64| svc.promote(cv, v[n].id, env, Mode::Rollback, &|_| {}, &ops::no_progress);
        let refused = async |n, env| rollback(n, env).await.unwrap_err().to_string();
        assert_eq!(refused(2, test).await, "no version is active in Test; use Promote", "rollback is not a first promotion");
        svc.store.promoted(test, cv, v[1].id, "active").await.unwrap();
        assert_eq!(refused(2, test).await, "v3 is newer than the active v2 in Test; use Promote", "it would skip the path");
        assert_eq!(svc.store.binding(test, cv).await.unwrap().unwrap().active(), Some(v[1].id));
        assert!(server.received_requests().await.unwrap().is_empty(), "refused before any request");

        // Older, the active version again (a retry), and Library (any published version) go on.
        let ok = |body: Value| ResponseTemplate::new(200).set_body_json(body);
        let none = json!({ "count": 0, "next": null, "results": [] });
        for p in ["/pulp/api/v3/distributions/rpm/rpm/", "/pulp/api/v3/distributions/"] {
            Mock::given(method("GET")).and(path(p)).respond_with(ok(none.clone())).mount(&server).await;
        }
        let task = ResponseTemplate::new(202).set_body_json(json!({ "task": "/pulp/api/v3/tasks/1/" }));
        Mock::given(method("POST")).and(path("/pulp/api/v3/distributions/rpm/rpm/")).respond_with(task).mount(&server).await;
        let done = json!({ "pulp_href": "/pulp/api/v3/tasks/1/", "state": "completed", "name": "create", "created_resources": [DIST] });
        Mock::given(method("GET")).and(path("/pulp/api/v3/tasks/1/")).respond_with(ok(done)).mount(&server).await;
        for (n, env) in [(0, test), (0, test), (2, lib), (0, lib)] {
            rollback(n, env).await.unwrap();
            assert_eq!(svc.store.binding(env, cv).await.unwrap().unwrap().active(), Some(v[n].id));
        }
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
        svc.store.promoted(lib, cv, v.id, "promoting").await.unwrap(); // an interrupted promotion
        svc.promote(cv, v.id, lib, Mode::Promote, &|_| {}, &ops::no_progress).await.unwrap();
        assert_eq!(svc.store.binding(lib, cv).await.unwrap().unwrap().active(), Some(v.id), "promote again recovers it");
        assert_none(&server, "PATCH").await;
        assert_none(&server, "POST").await;
        assert_eq!(svc.store.binding_repos(lib, cv).await.unwrap()[0].base_path, "custom/os");
    }

    /// View `web` in Library with repository `b-c` derives the distribution name of view `web-Library` in environment
    /// `b` with repository `c` (the format stays). The refusal names the other view; an unlabeled one is refused as before.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_derived_distribution_name_of_another_view_is_refused_with_that_view() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let other = "web in Library: failed: b-c: distribution pulp-tui-cv-web-Library-b-c belongs to content view web-Library in \
                     environment b (labels pulp_tui_cv=web-Library, pulp_tui_env=b): view web in environment Library with \
                     repository b-c derives the same name; refusing to modify it";
        let unlabeled = r#"web in Library: failed: b-c: conflict [409]: distribution pulp-tui-cv-web-Library-b-c exists but lacks ownership labels {"pulp_tui_cv":"web","pulp_tui_env":"Library"}"#;
        for (labels, want) in [(json!({ LABEL_CV: "web-Library", LABEL_ENV: "b" }), other), (json!({}), unlabeled)] {
            let server = wiremock::MockServer::start().await;
            let svc = mock_service(&server).await;
            let cv = svc.store.create_cv("k", "web", "").await.unwrap();
            let v = published(&svc, cv, "b-c", "/pulp/api/v3/repositories/rpm/rpm/1/").await;
            let lib = svc.store.ensure_library("k").await.unwrap();
            let d =
                json!({ "pulp_href": DIST, "name": "pulp-tui-cv-web-Library-b-c", "base_path": "b/web-library/c", "pulp_labels": labels });
            Mock::given(method("GET"))
                .and(path("/pulp/api/v3/distributions/rpm/rpm/"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "count": 1, "next": null, "results": [d] })))
                .mount(&server)
                .await;
            let e = svc.promote(cv, v.id, lib, Mode::Promote, &|_| {}, &ops::no_progress).await.unwrap_err();
            assert_eq!(e.to_string(), want);
            assert_none(&server, "PATCH").await;
            assert_none(&server, "POST").await;
        }
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

        let (v, _) = svc.publish(c, "", "", &|_| {}, &ops::no_progress).await.unwrap();
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
        assert_eq!(svc.store.binding(lib, c).await.unwrap().unwrap().active(), Some(v.id));
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
        serve_labels(&server, DERIVED, json!({ LABEL_CV: "os", LABEL_DB: svc.store.db_id() })).await;
        // Pulp shows the composite's distribution too: the reason of the binding row comes first.
        let pubs = json!([{ "pulp_href": row.publication, "repository_version": row.derived_version }]);
        let dists = json!([{ "pulp_href": DIST, "name": "pulp-tui-cv-rocky9-Dev-os", "base_path": "x", "publication": row.publication }]);
        serve_snapshot(&server, pubs, dists).await;
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

    /// Pulp lists `pubs` as the publications of DERIVED and `dists` as the RPM distributions.
    async fn serve_snapshot(server: &wiremock::MockServer, pubs: Value, dists: Value) {
        for (p, results) in [("/pulp/api/v3/publications/rpm/rpm/", pubs), ("/pulp/api/v3/distributions/rpm/rpm/", dists)] {
            let page = json!({ "count": results.as_array().unwrap().len(), "next": null, "results": results });
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path(p))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(page))
                .mount(server)
                .await;
        }
    }

    /// A distribution named "partial" that no binding row records, serving `href` through `field`.
    fn plain_dist(field: &str, href: &str) -> Value {
        json!({ "pulp_href": "/pulp/api/v3/distributions/rpm/rpm/7/", "name": "partial", "base_path": "partial", field: href })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_version_keeps_what_a_plain_distribution_serves() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let other = "/pulp/api/v3/publications/rpm/rpm/9/";
        // (field of the distribution, what it serves: `None` = the row's publication, why, the publication is kept too)
        let cases = [
            ("publication", None, "pinned to a publication of this version", true),
            ("repository", Some(DERIVED), "follows the repository; this is its newest published version", true),
            // Pulp serves only the pinned publication: another publication of the version can go.
            ("publication", Some(other), "pinned to a publication of this version", false),
        ];
        for (field, serves, why, keeps_pub) in cases {
            let server = wiremock::MockServer::start().await;
            let svc = mock_service(&server).await;
            let cv = svc.store.create_cv("k", "web", "").await.unwrap();
            let v = published(&svc, cv, "os", "/pulp/api/v3/repositories/rpm/rpm/1/").await;
            let row = svc.store.version_repos(v.id).await.unwrap().remove(0);
            let (p, dv) = (row.publication.unwrap(), row.derived_version.unwrap());
            serve_labels(&server, DERIVED, json!({ LABEL_CV: "web", LABEL_DB: svc.store.db_id() })).await;
            let pubs = json!([{ "pulp_href": p, "repository_version": dv }, { "pulp_href": other, "repository_version": dv }]);
            serve_snapshot(&server, pubs, json!([plain_dist(field, serves.unwrap_or(&p))])).await;
            Mock::given(method("DELETE")).and(path(p.as_str())).respond_with(ResponseTemplate::new(204)).mount(&server).await;
            let logs = Mutex::new(vec![]);
            svc.delete_version(v.id, &|m| logs.lock().unwrap().push(m), &ops::no_progress).await.unwrap();
            let kept = |h: &str| format!("keeping {h}: served by partial ({why})");
            let sent = server.received_requests().await.unwrap();
            let deleted: Vec<&str> = sent.iter().filter(|r| r.method.as_str() == "DELETE").map(|r| r.url.path()).collect();
            if keeps_pub {
                assert_eq!(logs.into_inner().unwrap(), [kept(&p), kept(&dv)], "{field}");
                assert!(deleted.is_empty(), "{deleted:?}");
            } else {
                assert_eq!(logs.into_inner().unwrap(), [kept(&dv)]);
                assert_eq!(deleted, [p.as_str()]);
            }
            assert_eq!(svc.store.version(v.id).await.unwrap().state, "deleted");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_view_refuses_while_a_plain_distribution_serves_its_snapshot() {
        for field in ["repository", "repository_version", "publication"] {
            let server = wiremock::MockServer::start().await;
            let svc = mock_service(&server).await;
            let cv = svc.store.create_cv("k", "web", "").await.unwrap();
            let v = published(&svc, cv, "os", "/pulp/api/v3/repositories/rpm/rpm/1/").await;
            let row = svc.store.version_repos(v.id).await.unwrap().remove(0);
            let (p, dv) = (row.publication.unwrap(), row.derived_version.unwrap());
            let serves = match field {
                "repository" => DERIVED,
                "repository_version" => dv.as_str(),
                _ => p.as_str(),
            };
            // web in Dev: its own distribution serves the snapshot too, and the view deletes it.
            let lib = svc.store.ensure_library("k").await.unwrap();
            let dev = svc.store.create_env("k", "Dev", lib, false).await.unwrap();
            let b = BindingRepo {
                env_id: dev,
                cv_id: cv,
                repo_href: "/pulp/api/v3/repositories/rpm/rpm/1/".into(),
                repo_name: "os".into(),
                repo_type: RepoType::Rpm,
                distribution: DIST.into(),
                base_path: "dev/web/os".into(),
                publication: Some(p.clone()),
            };
            svc.store.save_binding_repo(&b).await.unwrap();
            let own = json!({ "pulp_href": DIST, "name": "pulp-tui-cv-web-Dev-os", "base_path": "dev/web/os", "publication": p });
            let pubs = json!([{ "pulp_href": p, "repository_version": dv }]);
            serve_snapshot(&server, pubs, json!([own, plain_dist(field, serves)])).await;
            let e = svc.delete_view(cv, &|_| {}, &ops::no_progress).await.unwrap_err();
            assert_eq!(
                e.to_string(),
                format!(
                    "content view web: distribution partial serves the snapshot repository of os ({DERIVED}); \
                     delete partial or point it to other content first"
                ),
                "{field}"
            );
            assert_none(&server, "DELETE").await;
            assert_eq!(svc.store.content_view(cv).await.unwrap().name, "web");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deletes_go_on_when_the_snapshot_repository_is_gone() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        for exists in [false, true] {
            let server = wiremock::MockServer::start().await;
            let svc = mock_service(&server).await;
            let cv = svc.store.create_cv("k", "web", "").await.unwrap();
            let v = published(&svc, cv, "os", "/pulp/api/v3/repositories/rpm/rpm/1/").await;
            // Pulp answers 400 to the href of a deleted repository in a filter; the repository answers 404 (not mounted).
            let bad = ResponseTemplate::new(400).set_body_json(json!([format!("URI {DERIVED} not found for repository.")]));
            Mock::given(method("GET")).and(path("/pulp/api/v3/publications/rpm/rpm/")).respond_with(bad).mount(&server).await;
            let none = ResponseTemplate::new(200).set_body_json(json!({ "count": 0, "next": null, "results": [] }));
            Mock::given(method("GET")).and(path("/pulp/api/v3/distributions/rpm/rpm/")).respond_with(none).mount(&server).await;
            if exists {
                // Another 400 while the repository is there: what serves it is unknown, so nothing is deleted.
                serve_labels(&server, DERIVED, json!({ LABEL_CV: "web", LABEL_DB: svc.store.db_id() })).await;
                assert!(svc.delete_view(cv, &|_| {}, &ops::no_progress).await.is_err());
                assert_none(&server, "DELETE").await;
                assert_eq!(svc.store.content_view(cv).await.unwrap().name, "web");
            } else {
                // A rerun after an interrupted delete: the repository is already gone.
                svc.delete_version(v.id, &|_| {}, &ops::no_progress).await.unwrap();
                svc.delete_view(cv, &|_| {}, &ops::no_progress).await.unwrap();
                assert!(svc.store.content_views("k").await.unwrap().is_empty());
            }
        }
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
        let names = |cs: Vec<ContentView>| cs.into_iter().map(|c| c.name).collect::<Vec<_>>();
        assert_eq!(names(svc.lagging_composites(os).await.unwrap()), ["broken", "rocky9"], "web has os v1; quiet does not follow");
        // A new version of os: every composite with auto publish lacks it.
        let v2 = s.begin_version(os, "", &[]).await.unwrap();
        s.finish_version(v2.id, "published", None).await.unwrap();
        assert_eq!(names(svc.lagging_composites(os).await.unwrap()), ["broken", "rocky9", "web"]);

        // Nobody types the confirmation of a protected Library during an auto publish.
        let lib = s.ensure_library("k").await.unwrap();
        s.set_env_protected(lib, true).await.unwrap();
        let r = svc.auto_publish(os, &|_| {}, &ops::no_progress).await;
        assert_eq!(r[2], Err("web: not published: Library is protected (publish it yourself)".to_string()));
        assert_eq!(s.versions(web).await.unwrap().len(), 1);

        // Library still serves "old" of web, through a distribution of another database: the Library step fails.
        s.set_env_protected(lib, false).await.unwrap();
        let old = BindingRepo {
            env_id: lib,
            cv_id: web,
            repo_href: "/pulp/api/v3/repositories/rpm/rpm/9/".into(),
            repo_name: "old".into(),
            repo_type: RepoType::Rpm,
            distribution: DIST.into(),
            base_path: "library/web/old".into(),
            publication: Some("/pulp/api/v3/publications/rpm/rpm/3/".into()),
        };
        s.save_binding_repo(&old).await.unwrap();
        serve_owner(&server, DIST, "other").await;
        let r = svc.auto_publish(os, &|_| {}, &ops::no_progress).await;
        let e = r[2].as_ref().unwrap_err();
        assert!(e.starts_with("web v2 published; Library step failed: ") && e.ends_with("; promote v2 to Library"), "{e}");
        assert_eq!(s.versions(web).await.unwrap()[0].state, "published", "the composite version stays published");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_of_an_older_version_keeps_the_newer_one_in_library() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let s = &svc.store;
        let web = s.create_cv("k", "web", "").await.unwrap();
        // Versions without repositories: resume and the Library binding send nothing to Pulp.
        let v1 = s.begin_version(web, "", &[]).await.unwrap();
        s.finish_version(v1.id, "failed", Some("copy failed")).await.unwrap();
        let v2 = s.begin_version(web, "", &[]).await.unwrap();
        assert_eq!(svc.resume(v2.id, &|_| {}, &ops::no_progress).await.unwrap().1, None, "the newest version goes to Library");
        let lib = s.ensure_library("k").await.unwrap();
        assert_eq!(s.binding(lib, web).await.unwrap().unwrap().active(), Some(v2.id));

        let (v1, note) = svc.resume(v1.id, &|_| {}, &ops::no_progress).await.unwrap();
        assert_eq!(v1.state, "published");
        assert_eq!(note.as_deref(), Some("Library not changed: v2 is newer (use Rollback to serve v1)"));
        assert_eq!(s.binding(lib, web).await.unwrap().unwrap().active(), Some(v2.id), "Library keeps the newer v2");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn verify_reads_the_manifest_of_the_served_file_publication() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let s = &svc.store;
        let cv = s.create_cv("k", "docs", "").await.unwrap();
        let (src, derived) = ("/pulp/api/v3/repositories/file/file/1/", "/pulp/api/v3/repositories/file/file/2/");
        let r = CvRepo { repo_href: src.into(), repo_name: "files".into(), repo_type: RepoType::File, filters: vec![] };
        let v = s.begin_version(cv, "", std::slice::from_ref(&r)).await.unwrap();
        let (dv, publication, dist) =
            (format!("{derived}versions/1/"), "/pulp/api/v3/publications/file/file/1/", "/pulp/api/v3/distributions/file/file/1/");
        let row = CvVersionRepo {
            version_id: v.id,
            repo: Some(r),
            derived_repo: Some(derived.into()),
            derived_version: Some(dv.clone()),
            publication: Some(publication.into()),
            ..Default::default()
        };
        s.save_version_repo(&row).await.unwrap();
        s.finish_version(v.id, "published", None).await.unwrap();
        let lib = s.ensure_library("k").await.unwrap();
        s.promoted(lib, cv, v.id, "active").await.unwrap();
        let b = BindingRepo {
            env_id: lib,
            cv_id: cv,
            repo_href: src.into(),
            repo_name: "files".into(),
            repo_type: RepoType::File,
            distribution: dist.into(),
            base_path: "library/docs/files".into(),
            publication: Some(publication.into()),
        };
        s.save_binding_repo(&b).await.unwrap();
        let get = |p: String, body: Value| {
            let m = wiremock::Mock::given(wiremock::matchers::method("GET")).and(wiremock::matchers::path(p));
            m.respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body)).mount(&server)
        };
        let base = format!("{}/pulp/content/library/docs/files/", server.uri());
        get(
            dist.into(),
            json!({ "pulp_href": dist, "name": "d", "base_path": "library/docs/files", "base_url": base, "publication": publication }),
        )
        .await;
        // The snapshot was published with the custom manifest name of its source repository.
        get(publication.into(), json!({ "pulp_href": publication, "repository_version": dv, "manifest": "CUSTOM" })).await;
        get(dv.clone(), json!({ "pulp_href": dv })).await;
        let files = json!({ "count": 1, "next": null, "results": [{ "pulp_href": "/f/a/", "relative_path": "a.txt", "sha256": "ab" }] });
        get("/pulp/api/v3/content/file/files/".into(), files).await;
        let issues = svc.verify(cv, lib).await.unwrap();
        assert_eq!(issues, ["files: served content differs: CUSTOM returned HTTP 404"], "the manifest of the publication is fetched");
        let served = wiremock::ResponseTemplate::new(200).set_body_string("a.txt,ab,3\n");
        let manifest = wiremock::Mock::given(wiremock::matchers::path("/pulp/content/library/docs/files/CUSTOM"));
        manifest.respond_with(served).mount(&server).await;
        assert_eq!(svc.verify(cv, lib).await.unwrap(), Vec::<String>::new(), "served as bound");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn verify_names_each_way_a_served_repository_differs() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let s = &svc.store;
        let cv = s.create_cv("k", "docs", "").await.unwrap();
        let h = |kind: &str, n: &str| format!("/pulp/api/v3/{kind}/file/file/{n}/");
        let repo = |n: &str| CvRepo { repo_href: h("repositories", n), repo_name: n.into(), repo_type: RepoType::File, filters: vec![] };
        // a: distribution unreadable, b: serves another publication, c: publication unreadable,
        // d: publication of another version, snapshot gone, no base_url. e is not in the version and serves nothing.
        let names = ["a", "b", "c", "d"];
        let v = s.begin_version(cv, "", &names.map(repo)).await.unwrap();
        for vr in s.version_repos(v.id).await.unwrap() {
            let n = vr.repo().repo_name.clone();
            let row = CvVersionRepo {
                derived_repo: Some(h("repositories", &format!("d{n}"))),
                derived_version: Some(h("repositories", &format!("d{n}/versions/1"))),
                publication: Some(h("publications", &n)),
                ..vr
            };
            s.save_version_repo(&row).await.unwrap();
        }
        s.finish_version(v.id, "published", None).await.unwrap();
        let lib = s.ensure_library("k").await.unwrap();
        s.promoted(lib, cv, v.id, "partial").await.unwrap();
        for n in ["a", "b", "c", "d", "e"] {
            let b = BindingRepo {
                env_id: lib,
                cv_id: cv,
                repo_href: h("repositories", n),
                repo_name: n.into(),
                repo_type: RepoType::File,
                distribution: h("distributions", n),
                base_path: format!("library/docs/{n}"),
                publication: Some(h("publications", n)),
            };
            s.save_binding_repo(&b).await.unwrap();
        }
        let get = |p: String, body: Value| {
            let m = wiremock::Mock::given(wiremock::matchers::method("GET")).and(wiremock::matchers::path(p));
            m.respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body)).mount(&server)
        };
        let dist = |n: &str, publication: Value| json!({ "pulp_href": h("distributions", n), "name": n, "base_path": n, "base_url": null, "publication": publication });
        get(h("distributions", "b"), dist("b", json!("/other/"))).await;
        for n in ["c", "d"] {
            get(h("distributions", n), dist(n, json!(h("publications", n)))).await;
        }
        get(h("distributions", "e"), dist("e", Value::Null)).await;
        get(h("publications", "d"), json!({ "pulp_href": h("publications", "d"), "repository_version": "/old/" })).await;
        let da = json!({ "pulp_href": h("repositories", "da"), "name": "docs-a", "retain_repo_versions": 2 });
        get(h("repositories", "da"), da).await;
        assert_eq!(
            svc.verify(cv, lib).await.unwrap(),
            [
                "promotion of v1 is partial".to_string(),
                format!("a: distribution {}: not found [404]", h("distributions", "a")),
                format!("b: distribution serves /other/, expected {}", h("publications", "b")),
                "c: publication: not found [404]".into(),
                format!("d: publication is of /old/, expected {}", h("repositories", "dd/versions/1")),
                format!("d: snapshot {} missing (pruned?): not found [404]", h("repositories", "dd/versions/1")),
                "d: distribution has no base_url".into(),
                "a: retain_repo_versions is set on derived repository docs-a".into(),
            ]
        );
    }

    /// Server B of the import tests: RPM repository os at `href`, no other RPM or File repository, no pulp_deb.
    async fn import_target(server: &wiremock::MockServer, href: &str) -> Capabilities {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, ResponseTemplate};
        let page = |r: Value| json!({ "count": r.as_array().unwrap().len(), "next": null, "results": r });
        let os = json!([{ "pulp_href": href, "name": "os", "latest_version_href": null }]);
        Mock::given(method("GET"))
            .and(path("/pulp/api/v3/repositories/rpm/rpm/"))
            .and(query_param("name", "os"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(os)))
            .with_priority(1)
            .mount(server)
            .await;
        for t in ["rpm/rpm", "file/file"] {
            Mock::given(method("GET"))
                .and(path(format!("/pulp/api/v3/repositories/{t}/")))
                .respond_with(ResponseTemplate::new(200).set_body_json(page(json!([]))))
                .mount(server)
                .await;
        }
        let mut caps = Capabilities::default();
        caps.versions.extend([("rpm".into(), "3.29.0".into()), ("file".into(), "3.29.0".into())]);
        caps
    }

    #[tokio::test]
    async fn import_stores_the_href_of_the_repository_with_that_name_on_this_server() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let b = "/pulp/api/v3/repositories/rpm/rpm/b1/";
        let caps = import_target(&server, b).await;
        let st = &svc.store;
        let os = CvRepo {
            repo_href: "/pulp/api/v3/repositories/rpm/rpm/a1/".into(),
            repo_name: "os".into(),
            repo_type: RepoType::Rpm,
            filters: vec![],
        };
        let web = st.create_cv("a", "web", "").await.unwrap();
        st.set_cv_repo(web, &os).await.unwrap();
        // A view that the target has is kept, so its repositories (gone: not on B) are not looked up.
        let old = st.create_cv("a", "old", "").await.unwrap();
        st.set_cv_repo(old, &CvRepo { repo_name: "gone".into(), ..os.clone() }).await.unwrap();
        st.create_cv("b", "old", "").await.unwrap();
        let (cvs, _, warnings) = import(&svc.client, &caps, st, "b", &st.export("a").await.unwrap()).await.unwrap();
        assert_eq!((cvs, warnings), (1, vec![]));
        let id = st.content_views("b").await.unwrap().into_iter().find(|v| v.name == "web").unwrap().id;
        assert_eq!(st.cv_repos(id).await.unwrap(), [CvRepo { repo_href: b.into(), ..os }]);
    }

    #[tokio::test]
    async fn import_names_every_missing_repository_and_writes_nothing() {
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let caps = import_target(&server, "/pulp/api/v3/repositories/rpm/rpm/b1/").await;
        let st = &svc.store;
        let r = |n: &str, t: RepoType| CvRepo {
            repo_href: format!("/a/{}/{n}/", t.label()),
            repo_name: n.into(),
            repo_type: t,
            filters: vec![],
        };
        let web = st.create_cv("a", "web", "").await.unwrap();
        // os as File: B has os only as RPM. apt: B has no pulp_deb, so nothing is asked.
        for repo in [r("os", RepoType::Rpm), r("os", RepoType::File), r("apt", RepoType::Deb)] {
            st.set_cv_repo(web, &repo).await.unwrap();
        }
        let db = st.create_cv("a", "db", "").await.unwrap();
        st.set_cv_repo(db, &r("pkgs", RepoType::Rpm)).await.unwrap();
        let e = import(&svc.client, &caps, st, "b", &st.export("a").await.unwrap()).await.unwrap_err();
        assert_eq!(
            e.to_string(),
            "repositories not found on this server (by name and type): pkgs (RPM, view db), apt (Deb, view web), \
             os (File, view web); nothing was imported"
        );
        assert!(st.content_views("b").await.unwrap().is_empty() && st.environments("b").await.unwrap().is_empty());
        assert_none(&server, "POST").await;
    }

    #[tokio::test]
    async fn export_carries_the_current_pulp_name_and_keeps_the_store() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let svc = mock_service(&server).await;
        let href = "/pulp/api/v3/repositories/rpm/rpm/1/";
        Mock::given(method("GET"))
            .and(path(href))
            .and(query_param("fields", "name"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "name": "new" })))
            .expect(1)
            .mount(&server)
            .await;
        let st = &svc.store;
        let old = CvRepo { repo_href: href.into(), repo_name: "old".into(), repo_type: RepoType::Rpm, filters: vec![] };
        for name in ["web", "db"] {
            let id = st.create_cv("a", name, "").await.unwrap();
            st.set_cv_repo(id, &old).await.unwrap();
        }
        let ex = export(&svc.client, st, "a").await.unwrap();
        let names: Vec<&str> = ex.content_views.iter().flat_map(|v| &v.repos).map(|r| r.repo_name.as_str()).collect();
        assert_eq!(names, ["new", "new"], "renamed in Pulp; one GET for both views");
        assert_eq!(st.export("a").await.unwrap().content_views[0].repos, [old], "the store keeps the recorded name");
    }

    #[tokio::test]
    async fn export_names_every_repository_that_is_gone() {
        let server = wiremock::MockServer::start().await; // every GET is 404
        let svc = mock_service(&server).await;
        let st = &svc.store;
        let r =
            |n: &str, t: RepoType| CvRepo { repo_href: format!("/pulp/api/v3/{n}/"), repo_name: n.into(), repo_type: t, filters: vec![] };
        let web = st.create_cv("a", "web", "").await.unwrap();
        st.set_cv_repo(web, &r("os", RepoType::Rpm)).await.unwrap();
        let docs = st.create_cv("a", "docs", "").await.unwrap();
        st.set_cv_repo(docs, &r("iso", RepoType::File)).await.unwrap();
        let e = export(&svc.client, st, "a").await.unwrap_err();
        assert_eq!(e.to_string(), "repositories not found on this server: iso (File, view docs), os (RPM, view web); nothing was exported");
    }

    #[test]
    fn conflict_message_shows_hours_and_minutes() {
        let h = LockHolder { owner: "alice@ws12".into(), operation: "promote to Prod".into(), acquired_at: "2026-10-23T14:02:33Z".into() };
        assert_eq!(conflict("web", &h), "Content View web is locked by alice@ws12 (promote to Prod) since 14:02 UTC");
    }
}
