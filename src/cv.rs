//! Content Views: an application-managed layer (inspired by Katello, NOT a Pulp API).
//!
//! * Publish: for every repository in the view, the selected content of the source's
//!   latest version is copied into an application-owned *derived* repository, giving a
//!   new immutable repository version, which is then published. The derived repository
//!   has no retention limit, so source-side pruning cannot remove CV snapshots.
//! * Promote/rollback: per environment and repository a distribution with a stable base
//!   path (`<prefix>/<env>/<cv>/<repo>`) is re-pointed to the version's publication.
//!   History is never deleted to roll back.
//!
//! Pulp has no cross-resource transactions. Each step is idempotent (find-or-create by
//! name, publication reuse) and checkpointed in the store, including the href of a
//! submitted task, so an interrupted publish resumes by re-attaching to that task.
//! Partial promotions are recorded as `partial` and can be retried or reverted.

use crate::filters::Selector;
use crate::pulp::models::{Distribution, Package, RepoVersion, Repository, Task};
use crate::pulp::ops::{self, OnTask};
use crate::pulp::{Mutation, PulpClient, RepoType};
use crate::store::{BindingRepo, CvRepo, CvVersion, CvVersionRepo, Environment, LIBRARY, LOCK_RENEW_SECS, LockHolder, Store};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet};
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
    pub prefix: String,
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
    renew: tokio::task::AbortHandle,
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn conflict(name: &str, h: &LockHolder) -> String {
    let since = h.acquired_at.get(11..16).unwrap_or(&h.acquired_at);
    format!("Content View {name} is locked by {} ({}) since {since}", h.owner, h.operation)
}

/// One renewal round; `false` (lock lost, reason recorded) stops the renewal task.
async fn renew_step(store: &Store, cv_id: i64, token: &str, lost: &Mutex<Option<String>>) -> bool {
    let reason = match store.renew_lock(cv_id, token, unix_now()).await {
        Ok(true) => return true,
        Ok(false) => "taken over or broken by another operator".to_string(),
        Err(e) => format!("database unreachable: {e}"),
    };
    *lost.lock().unwrap() = Some(reason);
    false
}

impl CvLock {
    pub async fn acquire(store: Arc<Store>, cv_id: i64, name: &str, owner: &str, operation: &str) -> Result<Arc<CvLock>> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let token = format!("{}-{nanos}", std::process::id());
        if let Err(h) = store.try_lock(cv_id, owner, &token, operation, unix_now()).await? {
            bail!("{}", conflict(name, &h));
        }
        let lost = Arc::new(Mutex::new(None));
        let (st, tok, l) = (store.clone(), token.clone(), lost.clone());
        let renew = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(LOCK_RENEW_SECS)).await;
                if !renew_step(&st, cv_id, &tok, &l).await {
                    break;
                }
            }
        })
        .abort_handle();
        Ok(Arc::new(CvLock { store, cv_id, name: name.into(), token, lost, renew }))
    }

    pub fn check(&self) -> Result<()> {
        match &*self.lost.lock().unwrap() {
            Some(reason) => bail!("lock on {} lost: {reason}", self.name),
            None => Ok(()),
        }
    }

    #[cfg(test)]
    async fn renew_now(&self) {
        renew_step(&self.store, self.cv_id, &self.token, &self.lost).await;
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

pub fn derived_repo_name(cv: &str, repo: &str) -> String {
    format!("pulp-tui-cv-{}-{}", slug(cv), slug(repo))
}

pub fn distribution_name(cv: &str, env: &str, repo: &str) -> String {
    format!("pulp-tui-cv-{}-{}-{}", slug(cv), slug(env), slug(repo))
}

pub fn base_path(prefix: &str, cv: &str, env: &str, repo: &str) -> String {
    format!("{}/{}/{}/{}", slug(prefix), slug(env).to_lowercase(), slug(cv), slug(repo))
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
        let c = &self.client;
        let sel = Selector::new(&repo.filters);
        match repo.repo_type {
            RepoType::Rpm => {
                if repo.filters.is_empty() && !with_keys {
                    return Ok(Selection { hrefs: None, keys: BTreeSet::new() });
                }
                let pkgs = ops::packages_in(c, source_version).await?;
                let chosen: Vec<&Package> = pkgs.iter().filter(|p| sel.package(p)).collect();
                let keys = chosen.iter().map(|p| p.nevra()).collect();
                if repo.filters.is_empty() {
                    return Ok(Selection { hrefs: None, keys });
                }
                let chosen_nevra: HashSet<(String, String, String, String, String)> = chosen
                    .iter()
                    .map(|p| (p.name.clone(), norm_epoch(&p.epoch), p.version.clone(), p.release.clone(), p.arch.clone()))
                    .collect();
                let all_nevra: HashSet<_> = pkgs
                    .iter()
                    .map(|p| (p.name.clone(), norm_epoch(&p.epoch), p.version.clone(), p.release.clone(), p.arch.clone()))
                    .collect();
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
            RepoType::File => {
                let files = ops::files_in(c, source_version).await?;
                let chosen: Vec<_> = files.iter().filter(|f| sel.file(f)).collect();
                Ok(Selection {
                    keys: chosen.iter().map(|f| ops::file_key(f)).collect(),
                    hrefs: Some(chosen.into_iter().map(|f| f.pulp_href.clone()).collect()),
                })
            }
        }
    }

    // ---- publish -------------------------------------------------------------------

    /// Create a new numbered version from the current definition and publish it.
    pub async fn publish(&self, cv_id: i64, notes: &str, log: Log<'_>, on: OnTask<'_>) -> Result<CvVersion> {
        let cv = self.store.content_view(cv_id).await?;
        let repos = self.store.cv_repos(cv_id).await?;
        if repos.is_empty() {
            bail!("content view {} has no repositories", cv.name);
        }
        // Snapshot all source versions first so the view is consistent in time.
        let mut sources = vec![];
        for r in &repos {
            let repo: Repository =
                self.client.get(&r.repo_href, &[]).await.with_context(|| format!("source repository {}", r.repo_name))?;
            let sv = repo.latest_version_href.ok_or_else(|| anyhow!("{} has no versions", r.repo_name))?;
            sources.push(sv);
        }
        self.alive()?;
        let v = self.store.begin_version(cv_id, notes, &repos).await?;
        for (mut vr, sv) in self.store.version_repos(v.id).await?.into_iter().zip(sources) {
            vr.source_version = Some(sv);
            self.alive()?;
            self.store.save_version_repo(&vr).await?;
        }
        self.audit("cv.publish.start", &format!("{} version {}", cv.name, v.number)).await;
        log(format!("{} v{}: publishing {} repositories", cv.name, v.number, repos.len()));
        self.resume(v.id, log, on).await
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
                // Runs even when the lock is lost: it records why the workflow stopped.
                self.store.finish_version(v.id, "failed", Some(&msg)).await?;
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
            let task = match vr.pending_task.clone() {
                Some(h) => {
                    log(format!("{}: re-attaching to task {h}", repo.repo_name));
                    ops::wait_task(c, &h, on).await
                }
                None => {
                    let sv = vr.source_version.clone().ok_or_else(|| anyhow!("missing source version"))?;
                    log(format!("{}: selecting content from {sv}", repo.repo_name));
                    let sel = self.select(&repo, &sv, false).await?;
                    self.alive()?;
                    let m = match (t, sel.hrefs) {
                        (RepoType::Rpm, hrefs) => {
                            let mut cfg = json!({ "source_repo_version": sv, "dest_repo": derived, "dest_base_version": 0 });
                            if let Some(h) = hrefs {
                                cfg["content"] = h.into();
                            }
                            c.post(&c.api("rpm/copy/"), &json!({ "config": [cfg], "dependency_solving": false })).await?
                        }
                        (RepoType::File, hrefs) => {
                            let body = json!({ "remove_content_units": ["*"], "add_content_units": hrefs.unwrap_or_default() });
                            c.post(&format!("{derived}modify/"), &body).await?
                        }
                    };
                    let Mutation::Task(h) = m else { bail!("copy did not return a task") };
                    vr.pending_task = Some(h.clone());
                    // Saved even when the lock is lost: it records a task already sent, so Resume re-attaches.
                    self.store.save_version_repo(vr).await?;
                    self.alive()?;
                    ops::wait_task(c, &h, on).await
                }
            };
            let task: Task = task?;
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
        log(format!("{}: publishing {dv}", repo.repo_name));
        self.alive()?;
        vr.publication = Some(ops::ensure_publication(c, t, &dv, on).await?);
        self.alive()?;
        self.store.save_version_repo(vr).await?;
        Ok(())
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
        // The first prefix used for this server stays: a changed setting must not move served paths.
        let prefix = self.store.cv_prefix(&self.scope, &self.prefix).await?;
        if prefix != self.prefix {
            let msg = format!("cv_base_path_prefix {} differs from the stored {prefix} for this server; using {prefix}", self.prefix);
            tracing::warn!("{msg}");
            log(msg);
        }
        self.alive()?;
        self.store.set_binding(env_id, cv_id, version_id, "promoting").await?;
        log(format!("{} v{} -> {}", cv.name, v.number, env.name));
        let mut errors = vec![];
        let vrs = self.store.version_repos(version_id).await?;
        for vr in &vrs {
            if let Err(e) = self.bind_repo(&prefix, &cv.name, env, vr, on).await {
                errors.push(format!("{}: {e:#}", vr.repo().repo_name));
            }
        }
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
        } else if errors.len() < vrs.len() {
            "partial"
        } else {
            "failed"
        };
        // Runs even when the lock is lost: it records the partial state (a retry is idempotent).
        self.store.set_binding(env_id, cv_id, version_id, state).await?;
        let what = if mode == Mode::Rollback && env.name != LIBRARY { "cv.rollback" } else { "cv.promote" };
        self.audit(what, &format!("{} v{} -> {}: {state} {}", cv.name, v.number, env.name, errors.join("; "))).await;
        if errors.is_empty() {
            log(format!("{} v{} active in {}", cv.name, v.number, env.name));
            Ok(())
        } else {
            Err(anyhow!("{} in {}: {state}: {}", cv.name, env.name, errors.join("; ")))
        }
    }

    async fn bind_repo(&self, prefix: &str, cv: &str, env: &Environment, vr: &CvVersionRepo, on: OnTask<'_>) -> Result<()> {
        let repo = vr.repo();
        let publication = vr.publication.as_deref().ok_or_else(|| anyhow!("version has no publication"))?;
        let name = distribution_name(cv, &env.name, &repo.repo_name);
        let bp = base_path(prefix, cv, &env.name, &repo.repo_name);
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
        let repos = self.store.cv_repos(cv_id).await?;
        let old = match against {
            Some(v) => self.store.version_repos(v).await?,
            None => vec![],
        };
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
            out.extend(d.lines(if against.is_some() { "published" } else { "empty" }, "draft"));
            out.push(String::new());
        }
        Ok(out)
    }

    pub async fn compare_versions(&self, a: i64, b: i64) -> Result<Vec<String>> {
        let (va, vb) = (self.store.version_repos(a).await?, self.store.version_repos(b).await?);
        let mut hrefs: Vec<&CvRepo> = va.iter().chain(vb.iter()).map(|v| v.repo()).collect();
        hrefs.sort_by(|x, y| x.repo_name.cmp(&y.repo_name));
        hrefs.dedup_by(|x, y| x.repo_href == y.repo_href);
        let mut out = vec![];
        for r in hrefs {
            let dv = |vs: &[CvVersionRepo]| vs.iter().find(|v| v.repo().repo_href == r.repo_href).and_then(|v| v.derived_version.clone());
            let ka = ops::content_keys(&self.client, r.repo_type, dv(&va).as_deref()).await?;
            let kb = ops::content_keys(&self.client, r.repo_type, dv(&vb).as_deref()).await?;
            out.push(format!("== {} ({})", r.repo_name, r.repo_type.label()));
            out.extend(ops::diff(&ka, &kb).lines("older", "newer"));
            out.push(String::new());
        }
        Ok(out)
    }

    // ---- cleanup -------------------------------------------------------------------------

    /// Delete a version's Pulp objects unless shared with another live version or bound.
    pub async fn delete_version(&self, version_id: i64, log: Log<'_>, on: OnTask<'_>) -> Result<()> {
        let v = self.store.version(version_id).await?;
        let used = self.store.version_in_use(version_id).await?;
        if !used.is_empty() {
            bail!("version {} is bound to {}", v.number, used.join(", "));
        }
        let mut shared: HashSet<String> = HashSet::new();
        for other in self.store.versions(v.cv_id).await? {
            if other.id != v.id && other.state != "deleted" {
                for vr in self.store.version_repos(other.id).await? {
                    shared.extend(vr.publication.into_iter().chain(vr.derived_version));
                }
            }
        }
        for b in self.store.all_binding_repos(v.cv_id).await? {
            shared.extend(b.publication);
        }
        let aliases = self.store.aliases().await?;
        for vr in self.store.version_repos(version_id).await? {
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
                    log(format!("keeping shared {href}"));
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
        self.audit("cv.version.delete", &format!("cv {} version {}", v.cv_id, v.number)).await;
        Ok(())
    }

    /// Remove a whole content view: its distributions and derived repositories.
    pub async fn delete_view(&self, cv_id: i64, log: Log<'_>, on: OnTask<'_>) -> Result<()> {
        let cv = self.store.content_view(cv_id).await?;
        let mut hrefs: Vec<String> = self.store.all_binding_repos(cv_id).await?.into_iter().map(|b| b.distribution).collect();
        let mut repos: BTreeSet<String> = BTreeSet::new();
        for v in self.store.versions(cv_id).await? {
            repos.extend(self.store.version_repos(v.id).await?.into_iter().filter_map(|vr| vr.derived_repo));
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
        assert_eq!(base_path("cv", "web", "Prod", "os"), "cv/prod/web/os");
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
        s.break_lock(cv).await.unwrap();
        l.renew_now().await; // one renewal round without waiting 30 s
        assert!(l.check().unwrap_err().to_string().starts_with("lock on web lost: taken over or broken"));
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
    async fn mock_service(server: &wiremock::MockServer) -> CvService {
        let p = crate::config::Profile { url: server.uri(), auth: crate::config::AuthMethod::None, ..Default::default() };
        let client = PulpClient::new("t", &p, &crate::config::Settings::default()).unwrap();
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        CvService { client, store, prefix: "cv".into(), scope: "k".into(), actor: "a@h".into(), lock: None }
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

    #[test]
    fn conflict_message_shows_hours_and_minutes() {
        let h = LockHolder { owner: "alice@ws12".into(), operation: "promote to Prod".into(), acquired_at: "2026-10-23T14:02:33Z".into() };
        assert_eq!(conflict("web", &h), "Content View web is locked by alice@ws12 (promote to Prod) since 14:02");
    }
}
