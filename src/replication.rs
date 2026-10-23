//! Replication from an upstream Pulp.
//!
//! * Native: pulpcore's Upstream Pulp API (technology preview). It replicates distributions,
//!   repositories and remotes as Pulp metadata, governed by the server-side `policy`.
//! * Fallback: content synchronization only. For each selected upstream RPM/File
//!   distribution an ordinary remote (pointing at the published upstream content), a
//!   repository and a pinned distribution are created on the destination and synced.
//!   It does not replicate content guards, labels or other distribution metadata, and it
//!   never deletes destination objects. Objects are matched by name and owned via the
//!   `pulp_tui_replica` label, so reruns are idempotent and foreign objects are not touched.

use crate::pulp::RepoType;
use crate::pulp::client::{Mutation, PulpClient};
use crate::pulp::models::{Distribution, Repository, TaskGroup};
use crate::pulp::ops::{self, OnTask};
use crate::store::Store;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

pub const LABEL_REPLICA: &str = "pulp_tui_replica";

/// Trigger native replication and wait for its task group.
pub async fn replicate_native(c: &PulpClient, upstream_href: &str, on: &(dyn Fn(&TaskGroup) + Send + Sync)) -> Result<Vec<String>> {
    let Mutation::TaskGroup(g) = c.post(&format!("{upstream_href}replicate/"), &json!({})).await? else {
        bail!("replicate did not return a task group");
    };
    let group = ops::wait_task_group(c, &g, on).await?;
    let mut lines = vec![
        format!("{} ({})", group.description, g),
        format!("completed {}  failed {}  canceled {}  skipped {}", group.completed, group.failed, group.canceled, group.skipped),
        String::new(),
    ];
    for t in &group.tasks {
        let name = t["name"].as_str().unwrap_or_default().rsplit('.').next().unwrap_or_default().to_string();
        let mut line = format!("  {:<10} {name}  {}", t["state"].as_str().unwrap_or_default(), t["pulp_href"].as_str().unwrap_or_default());
        if t["state"] == "failed"
            && let Some(h) = t["pulp_href"].as_str()
            && let Ok(full) = c.get::<crate::pulp::models::Task>(h, &[]).await
        {
            line.push_str(&format!("\n      error: {}", full.error_message().unwrap_or_default()));
        }
        lines.push(line);
    }
    if group.failed > 0 {
        bail!("{}", lines.join("\n"));
    }
    Ok(lines)
}

/// Refuse replicating a server into itself.
pub fn guard_self(src_origin: &str, dst_origin: &str) -> Result<()> {
    if src_origin.trim_end_matches('/') == dst_origin.trim_end_matches('/') {
        bail!("source and destination are the same server ({src_origin}); refusing self-replication");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlanAction {
    Create,
    Update,
    Skip,
}

#[derive(Debug, Clone)]
pub struct PlanItem {
    pub t: RepoType,
    pub name: String,
    pub base_path: String,
    pub remote_url: String,
    pub action: PlanAction,
    pub note: String,
}

impl std::fmt::Display for PlanItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let a = match self.action {
            PlanAction::Create => "CREATE",
            PlanAction::Update => "SYNC  ",
            PlanAction::Skip => "SKIP  ",
        };
        write!(f, "{a} {:<4} {} -> /{}  {}", self.t.label(), self.name, self.base_path, self.note)
    }
}

fn owned(labels: &std::collections::BTreeMap<String, String>, src: &str) -> bool {
    labels.get(LABEL_REPLICA).map(String::as_str) == Some(src)
}

/// Dry run: what the fallback would do for each upstream distribution.
pub async fn plan(
    src: &PulpClient,
    dst: &PulpClient,
    types: &[RepoType],
    name_filter: &str,
    dst_content_base: Option<&str>,
) -> Result<Vec<PlanItem>> {
    let src_name = src.profile();
    let mut out = vec![];
    for &t in types {
        let mut q = vec![("ordering", "name".to_string())];
        if !name_filter.is_empty() {
            q.push(("name__icontains", name_filter.to_string()));
        }
        let dists: Vec<Distribution> = src.list_all(&src.api(t.distributions()), &q, 100_000).await?;
        for d in dists {
            let mut it = PlanItem {
                t,
                name: d.name.clone(),
                base_path: d.base_path.clone(),
                remote_url: String::new(),
                action: PlanAction::Skip,
                note: String::new(),
            };
            let Some(base) =
                d.base_url.clone().filter(|_| d.publication.is_some() || d.repository.is_some() || d.repository_version.is_some())
            else {
                it.note = "upstream distribution serves nothing".into();
                out.push(it);
                continue;
            };
            if dst_content_base.is_some_and(|b| base.starts_with(b)) {
                it.note = "upstream content is served by the destination itself (loop)".into();
                out.push(it);
                continue;
            }
            let base = if base.ends_with('/') { base } else { format!("{base}/") };
            it.remote_url = match t {
                RepoType::Rpm => base,
                RepoType::File => {
                    let manifest = match &d.publication {
                        Some(p) => src
                            .get::<Value>(p, &[("fields", "manifest".into())])
                            .await
                            .ok()
                            .and_then(|v| v["manifest"].as_str().map(str::to_string)),
                        None => None,
                    };
                    format!("{base}{}", manifest.unwrap_or_else(|| "PULP_MANIFEST".into()))
                }
            };
            let repo = ops::find_by_name::<Repository>(dst, &dst.api(t.repositories()), &d.name).await?;
            let remote = ops::find_by_name::<Value>(dst, &dst.api(t.remotes()), &d.name).await?;
            let dist = ops::find_by_name::<Distribution>(dst, &dst.api(t.distributions()), &d.name).await?;
            let path_owner: Option<Value> = dst
                .list::<Value>(&dst.api("distributions/"), &[("base_path", d.base_path.clone()), ("limit", "1".into())])
                .await?
                .results
                .into_iter()
                .next();
            let remote_labels: std::collections::BTreeMap<String, String> =
                remote.as_ref().and_then(|r| serde_json::from_value(r["pulp_labels"].clone()).ok()).unwrap_or_default();
            let foreign = [
                repo.as_ref().filter(|r| !owned(&r.pulp_labels, src_name)).map(|_| "repository"),
                remote.as_ref().filter(|_| !owned(&remote_labels, src_name)).map(|_| "remote"),
                dist.as_ref().filter(|x| !owned(&x.pulp_labels, src_name)).map(|_| "distribution"),
            ];
            if let Some(kind) = foreign.into_iter().flatten().next() {
                it.note = format!("conflict: a local {kind} named {} exists and is not managed by this replication", d.name);
            } else if let Some(o) = path_owner.filter(|o| o["name"].as_str() != Some(d.name.as_str())) {
                it.note = format!("conflict: base path used by local distribution {}", o["name"].as_str().unwrap_or("?"));
            } else if repo.is_some() {
                it.action = PlanAction::Update;
                it.note = "re-sync existing replica".into();
            } else {
                it.action = PlanAction::Create;
                it.note = "new remote + repository + distribution".into();
            }
            out.push(it);
        }
    }
    Ok(out)
}

/// Execute a plan. `mirror` makes new versions match upstream (old versions are kept).
/// Returns one result line per item; failures do not stop other items.
pub async fn run(
    dst: &PulpClient,
    store: &Store,
    src_profile: &str,
    items: &[PlanItem],
    mirror: bool,
    log: &(dyn Fn(String) + Send + Sync),
    on: OnTask<'_>,
) -> Result<Vec<String>> {
    let mut lines = vec![];
    for it in items.iter().filter(|i| i.action != PlanAction::Skip) {
        log(format!("replicating {} {}", it.t.label(), it.name));
        let res = replicate_one(dst, src_profile, it, mirror, on).await;
        let (state, detail) = match &res {
            Ok(m) => ("completed", m.clone()),
            Err(e) => ("failed", format!("{e:#}")),
        };
        if let Err(e) = store.record_replication(dst.profile(), src_profile, &it.base_path, state, &detail).await {
            tracing::warn!("cannot record replication run: {e:#}");
        }
        lines.push(format!("{state:<9} {} {}: {detail}", it.t.label(), it.name));
    }
    Ok(lines)
}

async fn replicate_one(c: &PulpClient, src_profile: &str, it: &PlanItem, mirror: bool, on: OnTask<'_>) -> Result<String> {
    let t = it.t;
    let labels = json!({ LABEL_REPLICA: src_profile });
    let remote = match ops::find_by_name::<Value>(c, &c.api(t.remotes()), &it.name).await? {
        Some(r) => {
            let h = r["pulp_href"].as_str().unwrap_or_default().to_string();
            if r["url"].as_str() != Some(it.remote_url.as_str()) {
                ops::settle(c, c.patch(&h, &json!({ "url": it.remote_url })).await?, on).await?;
            }
            h
        }
        None => {
            ops::create(
                c,
                &c.api(t.remotes()),
                &json!({ "name": it.name, "url": it.remote_url, "policy": "immediate", "pulp_labels": labels }),
                "/remotes/",
                on,
            )
            .await?
        }
    };
    let repo = match ops::find_by_name::<Repository>(c, &c.api(t.repositories()), &it.name).await? {
        Some(r) => r.pulp_href,
        None => {
            ops::create(
                c,
                &c.api(t.repositories()),
                &json!({ "name": it.name, "remote": remote, "pulp_labels": labels }),
                "/repositories/",
                on,
            )
            .await?
        }
    };
    let policy = match (t, mirror) {
        (RepoType::Rpm, true) => "mirror_content_only",
        (RepoType::File, true) => "mirror",
        _ => "additive",
    };
    ops::settle(c, ops::sync(c, t, &repo, Some(&remote), policy).await?, on).await?;
    let r: Repository = c.get(&repo, &[]).await?;
    let version = r.latest_version_href.ok_or_else(|| anyhow!("no repository version after sync"))?;
    let publication = ops::ensure_publication(c, t, &version, on).await?;
    let dist = ops::ensure_distribution(c, t, &it.name, &it.base_path, Some(&publication), &labels, on).await?;
    let d: Distribution = c.get(&dist, &[]).await?;
    let check = ops::verify_served(c, t, d.base_url.as_deref().unwrap_or_default(), &version, "").await?;
    Ok(format!("{} served at {} ({check})", short_version(&version), d.base_url.unwrap_or_default()))
}

fn short_version(v: &str) -> String {
    v.trim_end_matches('/').rsplit_once("/versions/").map(|(_, n)| format!("version {n}")).unwrap_or_else(|| v.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_replication_is_refused() {
        assert!(guard_self("http://a:8080", "http://a:8080/").is_err());
        assert!(guard_self("http://a:8080", "http://b:8080").is_ok());
        assert_eq!(short_version("/r/versions/4/"), "version 4");
    }
}
