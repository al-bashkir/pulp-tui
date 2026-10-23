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
use crate::store::{BindingRepo, CvRepo, CvVersion, CvVersionRepo, Environment, LIBRARY, Store};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

pub type Log<'a> = &'a (dyn Fn(String) + Send + Sync);

pub const LABEL_CV: &str = "pulp_tui_cv";
pub const LABEL_ENV: &str = "pulp_tui_env";

#[derive(Clone)]
pub struct CvService {
    pub client: PulpClient,
    pub store: Arc<Store>,
    pub prefix: String,
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
    fn profile(&self) -> &str {
        self.client.profile()
    }

    fn audit(&self, action: &str, detail: &str) {
        if let Err(e) = self.store.audit(self.profile(), action, detail) {
            tracing::warn!("audit write failed: {e:#}");
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
        let cv = self.store.content_view(cv_id)?;
        let repos = self.store.cv_repos(cv_id)?;
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
        let v = self.store.begin_version(cv_id, notes, &repos)?;
        for (mut vr, sv) in self.store.version_repos(v.id)?.into_iter().zip(sources) {
            vr.source_version = Some(sv);
            self.store.save_version_repo(&vr)?;
        }
        self.audit("cv.publish.start", &format!("{} version {}", cv.name, v.number));
        log(format!("{} v{}: publishing {} repositories", cv.name, v.number, repos.len()));
        self.resume(v.id, log, on).await
    }

    /// Run (or continue) the publish workflow of a version from its checkpoints.
    pub async fn resume(&self, version_id: i64, log: Log<'_>, on: OnTask<'_>) -> Result<CvVersion> {
        let v = self.store.version(version_id)?;
        let cv = self.store.content_view(v.cv_id)?;
        if v.state == "published" {
            return Ok(v);
        }
        if v.state == "deleted" {
            bail!("version {} was deleted", v.number);
        }
        self.store.finish_version(v.id, "publishing", None)?;
        let res = async {
            for mut vr in self.store.version_repos(v.id)? {
                self.publish_repo(&cv.name, &mut vr, log, on).await.with_context(|| format!("repository {}", vr.repo().repo_name))?;
            }
            anyhow::Ok(())
        }
        .await;
        match res {
            Ok(()) => {
                self.store.finish_version(v.id, "published", None)?;
                self.audit("cv.publish.done", &format!("{} version {}", cv.name, v.number));
                log(format!("{} v{} published", cv.name, v.number));
                let lib = self.store.ensure_library(self.profile())?;
                self.promote(v.cv_id, v.id, lib, Mode::Rollback, log, on).await?;
                Ok(self.store.version(v.id)?)
            }
            Err(e) => {
                let msg = format!("{e:#}");
                self.store.finish_version(v.id, "failed", Some(&msg))?;
                self.audit("cv.publish.failed", &format!("{} version {}: {msg}", cv.name, v.number));
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
        if vr.derived_repo.is_none() {
            let name = derived_repo_name(cv, &repo.repo_name);
            let href = match ops::find_by_name::<Repository>(c, &c.api(t.repositories()), &name).await? {
                // Never write into a repository this view does not own (name collision).
                Some(r) if r.pulp_labels.get(LABEL_CV).map(String::as_str) == Some(slug(cv).as_str()) => r.pulp_href,
                Some(_) => bail!("repository {name} exists but is not labeled {LABEL_CV}={}; refusing to modify it", slug(cv)),
                None => {
                    log(format!("creating derived repository {name}"));
                    let body = json!({
                        "name": name,
                        "description": format!("Content View {cv} snapshot of {} (managed by pulp-tui)", repo.repo_name),
                        "pulp_labels": { LABEL_CV: slug(cv) },
                    });
                    ops::create(c, &c.api(t.repositories()), &body, "/repositories/", on).await?
                }
            };
            vr.derived_repo = Some(href);
            self.store.save_version_repo(vr)?;
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
                    self.store.save_version_repo(vr)?;
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
            self.store.save_version_repo(vr)?;
        }
        let dv = vr.derived_version.clone().unwrap_or_default();
        log(format!("{}: publishing {dv}", repo.repo_name));
        vr.publication = Some(ops::ensure_publication(c, t, &dv, on).await?);
        self.store.save_version_repo(vr)?;
        Ok(())
    }

    // ---- promote / rollback --------------------------------------------------------

    /// Bind `version_id` to `env_id`. Re-points stable distributions; repositories that
    /// are bound in the environment but not part of the version stop being served.
    pub async fn promote(&self, cv_id: i64, version_id: i64, env_id: i64, mode: Mode, log: Log<'_>, on: OnTask<'_>) -> Result<()> {
        let cv = self.store.content_view(cv_id)?;
        let v = self.store.version(version_id)?;
        if v.cv_id != cv_id || v.state != "published" {
            bail!("version {} is not a published version of {}", v.number, cv.name);
        }
        let envs = self.store.environments(self.profile())?;
        let env = envs.iter().find(|e| e.id == env_id).ok_or_else(|| anyhow!("unknown environment"))?;
        if mode == Mode::Promote
            && let Some(prior) = env.prior_id
        {
            let pb = self.store.binding(prior, cv_id)?;
            if pb.as_ref().map(|b| (b.version_id, b.state.as_str())) != Some((version_id, "active")) {
                let pname = envs.iter().find(|e| e.id == prior).map(|e| e.name.as_str()).unwrap_or("?");
                bail!("version {} is not active in {pname}; promote it there first (or use rollback)", v.number);
            }
        }
        self.store.set_binding(env_id, cv_id, version_id, "promoting")?;
        log(format!("{} v{} -> {}", cv.name, v.number, env.name));
        let mut errors = vec![];
        let vrs = self.store.version_repos(version_id)?;
        for vr in &vrs {
            if let Err(e) = self.bind_repo(&cv.name, env, vr, on).await {
                errors.push(format!("{}: {e:#}", vr.repo().repo_name));
            }
        }
        // Repositories no longer part of the view stop serving (distribution kept for rollback).
        for b in self.store.binding_repos(env_id, cv_id)? {
            if vrs.iter().any(|vr| vr.repo().repo_href == b.repo_href) || b.publication.is_none() {
                continue;
            }
            let res = async {
                let m = self.client.patch(&b.distribution, &json!({ "publication": null })).await?;
                ops::settle(&self.client, m, on).await
            }
            .await;
            match res {
                Ok(_) => self.store.save_binding_repo(&BindingRepo { publication: None, ..b })?,
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
        self.store.set_binding(env_id, cv_id, version_id, state)?;
        let what = if mode == Mode::Rollback && env.name != LIBRARY { "cv.rollback" } else { "cv.promote" };
        self.audit(what, &format!("{} v{} -> {}: {state} {}", cv.name, v.number, env.name, errors.join("; ")));
        if errors.is_empty() {
            log(format!("{} v{} active in {}", cv.name, v.number, env.name));
            Ok(())
        } else {
            Err(anyhow!("{} in {}: {state}: {}", cv.name, env.name, errors.join("; ")))
        }
    }

    async fn bind_repo(&self, cv: &str, env: &Environment, vr: &CvVersionRepo, on: OnTask<'_>) -> Result<()> {
        let repo = vr.repo();
        let publication = vr.publication.as_deref().ok_or_else(|| anyhow!("version has no publication"))?;
        let name = distribution_name(cv, &env.name, &repo.repo_name);
        let bp = base_path(&self.prefix, cv, &env.name, &repo.repo_name);
        let labels = json!({ LABEL_CV: slug(cv), LABEL_ENV: slug(&env.name) });
        let href = ops::ensure_distribution(&self.client, repo.repo_type, &name, &bp, Some(publication), &labels, on).await?;
        self.store.save_binding_repo(&BindingRepo {
            env_id: env.id,
            cv_id: vr_cv(&self.store, vr.version_id)?,
            repo_href: repo.repo_href.clone(),
            repo_name: repo.repo_name.clone(),
            repo_type: repo.repo_type,
            distribution: href,
            base_path: bp,
            publication: Some(publication.into()),
        })?;
        Ok(())
    }

    // ---- verification / drift --------------------------------------------------------

    /// Check that what Pulp serves for an environment is exactly the bound version.
    /// Returns human-readable findings; empty means verified.
    pub async fn verify(&self, cv_id: i64, env_id: i64) -> Result<Vec<String>> {
        let c = &self.client;
        let Some(b) = self.store.binding(env_id, cv_id)? else { return Ok(vec!["not bound".into()]) };
        let mut issues = vec![];
        if b.state != "active" {
            issues.push(format!("binding state is {}", b.state));
        }
        let vrs = self.store.version_repos(b.version_id)?;
        for br in self.store.binding_repos(env_id, cv_id)? {
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
        let repos = self.store.cv_repos(cv_id)?;
        let old = match against {
            Some(v) => self.store.version_repos(v)?,
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
        let (va, vb) = (self.store.version_repos(a)?, self.store.version_repos(b)?);
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
        let v = self.store.version(version_id)?;
        let used = self.store.version_in_use(version_id)?;
        if !used.is_empty() {
            bail!("version {} is bound to {}", v.number, used.join(", "));
        }
        let mut shared: HashSet<String> = HashSet::new();
        for other in self.store.versions(v.cv_id)? {
            if other.id != v.id && other.state != "deleted" {
                for vr in self.store.version_repos(other.id)? {
                    shared.extend(vr.publication.into_iter().chain(vr.derived_version));
                }
            }
        }
        for b in self.store.all_binding_repos(v.cv_id)? {
            shared.extend(b.publication);
        }
        for vr in self.store.version_repos(version_id)? {
            for href in [vr.publication.as_ref(), vr.derived_version.as_ref()].into_iter().flatten() {
                if shared.contains(href) {
                    log(format!("keeping shared {href}"));
                    continue;
                }
                match self.client.delete(href).await {
                    Ok(m) => {
                        ops::settle(&self.client, m, on).await?;
                    }
                    Err(e) if e.is_not_found() => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        self.store.finish_version(version_id, "deleted", None)?;
        self.audit("cv.version.delete", &format!("cv {} version {}", v.cv_id, v.number));
        Ok(())
    }

    /// Remove a whole content view: its distributions and derived repositories.
    pub async fn delete_view(&self, cv_id: i64, log: Log<'_>, on: OnTask<'_>) -> Result<()> {
        let cv = self.store.content_view(cv_id)?;
        let mut hrefs: Vec<String> = self.store.all_binding_repos(cv_id)?.into_iter().map(|b| b.distribution).collect();
        let mut repos: BTreeSet<String> = BTreeSet::new();
        for v in self.store.versions(cv_id)? {
            repos.extend(self.store.version_repos(v.id)?.into_iter().filter_map(|vr| vr.derived_repo));
        }
        hrefs.extend(repos);
        for h in hrefs {
            log(format!("deleting {h}"));
            match self.client.delete(&h).await {
                Ok(m) => {
                    ops::settle(&self.client, m, on).await?;
                }
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.store.delete_cv(cv_id)?;
        self.audit("cv.delete", &cv.name);
        Ok(())
    }
}

fn norm_epoch(e: &str) -> String {
    if e.is_empty() || e == "null" { "0".into() } else { e.into() }
}

fn vr_cv(store: &Store, version_id: i64) -> Result<i64> {
    Ok(store.version(version_id)?.cv_id)
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
}
