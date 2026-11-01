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

use crate::pulp::client::{Mutation, PulpClient};
use crate::pulp::models::{Distribution, Repository, TaskGroup};
use crate::pulp::ops::{self, OnTask};
use crate::pulp::{Capabilities, RepoType};
use crate::store::Store;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

pub const LABEL_REPLICA: &str = "pulp_tui_replica";

/// Trigger native replication and wait for its task group. A failed task is an error;
/// `on` gets each snapshot, whose `TaskGroup::outcome` tells canceled or skipped tasks.
pub async fn replicate_native(c: &PulpClient, upstream_href: &str, on: &(dyn Fn(&TaskGroup) + Send + Sync)) -> Result<Vec<String>> {
    let Mutation::TaskGroup(g) = c.post(&format!("{upstream_href}replicate/"), &json!({})).await? else {
        bail!("replicate did not return a task group");
    };
    let (group, lines) = ops::group_report(c, &g, on).await?;
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
    /// File: the manifest name that the upstream publication serves; the replica publishes under it too.
    pub manifest: Option<String>,
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

/// The `pulp_labels` of an object read as JSON.
fn labels_of(o: &Value) -> std::collections::BTreeMap<String, String> {
    serde_json::from_value(o["pulp_labels"].clone()).unwrap_or_default()
}

/// Refused unless the server of `c` (the `side` of the replication) has the plugin of each of `types`.
async fn require_plugins(c: &PulpClient, types: &[RepoType], side: &str) -> Result<()> {
    let mut caps = Capabilities::default();
    caps.set_status(c.get(&c.api("status/"), &[]).await?);
    for t in types {
        caps.require(t.feature()).map_err(|e| anyhow!("{} ({side}): {e}", c.profile()))?;
    }
    Ok(())
}

/// Dry run: what the fallback would do for each upstream distribution.
pub async fn plan(
    src: &PulpClient,
    dst: &PulpClient,
    types: &[RepoType],
    name_filter: &str,
    dst_content_base: Option<&str>,
) -> Result<Vec<PlanItem>> {
    require_plugins(src, types, "source").await?;
    require_plugins(dst, types, "destination").await?;
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
                manifest: None,
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
                    // pulp_file serves only from publications: only a served manifest name makes a remote URL.
                    // The capabilities matter only for Deb publications, which this File branch never reads.
                    let m = match ops::served_publication(src, &Capabilities::default(), t, &serde_json::to_value(&d)?).await {
                        Err(e) => Err(format!("upstream publication not readable: {e}")),
                        Ok(None) => Err("upstream distribution serves nothing (no publication)".to_string()),
                        Ok(Some(p)) => p["manifest"].as_str().map(str::to_string).ok_or("upstream publication has no manifest".to_string()),
                    };
                    match m {
                        Ok(m) => {
                            it.manifest = Some(m.clone());
                            format!("{base}{m}")
                        }
                        Err(note) => {
                            it.note = note;
                            out.push(it);
                            continue;
                        }
                    }
                }
                RepoType::Deb => {
                    it.note = "Deb replication is not supported".into();
                    out.push(it);
                    continue;
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
            let foreign = [
                repo.as_ref().filter(|r| !owned(&r.pulp_labels, src_name)).map(|_| "repository"),
                remote.as_ref().filter(|r| !owned(&labels_of(r), src_name)).map(|_| "remote"),
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
    // First, so that an unsupported type changes nothing on the server.
    let policy = match (t, mirror) {
        (RepoType::Rpm, true) => "mirror_content_only",
        (RepoType::File, true) => "mirror",
        (RepoType::Rpm | RepoType::File, false) => "additive",
        (RepoType::Deb, _) => bail!("Deb replication is not supported"),
    };
    let labels = json!({ LABEL_REPLICA: src_profile });
    let remote = replica_remote(c, src_profile, it, &labels, on).await?;
    let repo = replica_repo(c, src_profile, it, &remote, &labels, on).await?;
    ops::settle(c, ops::sync(c, t, &repo, Some(&remote), policy).await?, on).await?;
    let r: Repository = c.get(&repo, &[]).await?;
    let version = r.latest_version_href.ok_or_else(|| anyhow!("no repository version after sync"))?;
    let publication = ops::ensure_publication(c, t, &version, None, on).await?;
    let dist = ops::ensure_distribution(c, t, &it.name, &it.base_path, Some(&publication), &labels, None, on).await?;
    let d: Distribution = c.get(&dist, &[]).await?;
    let p: Value = c.get(&publication, &[]).await?;
    let check = ops::verify_served(c, t, d.base_url.as_deref().unwrap_or_default(), &version, &p).await?;
    Ok(format!("{} served at {} ({check})", short_version(&version), d.base_url.unwrap_or_default()))
}

/// Checked again at run time: an object with the name of `it` may have been created since the dry run.
fn foreign(kind: &str, it: &PlanItem) -> anyhow::Error {
    anyhow!("a local {kind} named {} exists and is not managed by this replication", it.name)
}

/// The replica remote of `it`, pointed at the upstream URL.
async fn replica_remote(c: &PulpClient, src_profile: &str, it: &PlanItem, labels: &Value, on: OnTask<'_>) -> Result<String> {
    match ops::find_by_name::<Value>(c, &c.api(it.t.remotes()), &it.name).await? {
        Some(r) if !owned(&labels_of(&r), src_profile) => Err(foreign("remote", it)),
        Some(r) => {
            let h = r["pulp_href"].as_str().unwrap_or_default().to_string();
            if r["url"].as_str() != Some(it.remote_url.as_str()) {
                ops::settle(c, c.patch(&h, &json!({ "url": it.remote_url })).await?, on).await?;
            }
            Ok(h)
        }
        None => {
            let body = json!({ "name": it.name, "url": it.remote_url, "policy": "immediate", "pulp_labels": labels });
            Ok(ops::create(c, &c.api(it.t.remotes()), &body, "/remotes/", on).await?)
        }
    }
}

/// The replica repository of `it`. File: it publishes under the manifest name that upstream serves
/// (`ensure_publication` reuses only a publication with the repository's name).
async fn replica_repo(c: &PulpClient, src_profile: &str, it: &PlanItem, remote: &str, labels: &Value, on: OnTask<'_>) -> Result<String> {
    let manifest = it.manifest.as_ref().map(|m| json!({ "manifest": m }));
    match ops::find_by_name::<Value>(c, &c.api(it.t.repositories()), &it.name).await? {
        Some(r) if !owned(&labels_of(&r), src_profile) => Err(foreign("repository", it)),
        Some(r) => {
            let h = r["pulp_href"].as_str().unwrap_or_default().to_string();
            if let Some(m) = manifest.filter(|m| r["manifest"] != m["manifest"]) {
                ops::settle(c, c.patch(&h, &m).await?, on).await?;
            }
            Ok(h)
        }
        None => {
            let mut body = json!({ "name": it.name, "remote": remote, "pulp_labels": labels });
            if let Some(m) = &it.manifest {
                body["manifest"] = m.as_str().into();
            }
            Ok(ops::create(c, &c.api(it.t.repositories()), &body, "/repositories/", on).await?)
        }
    }
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

    #[tokio::test]
    async fn run_refuses_a_remote_created_after_the_dry_run() {
        let server = wiremock::MockServer::start().await;
        let p = crate::config::Profile { url: server.uri(), auth: crate::config::AuthMethod::None, ..Default::default() };
        let c = PulpClient::new("dst", &p, &crate::config::Settings::default()).unwrap();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/pulp/api/v3/remotes/rpm/rpm/"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({ "count": 1, "next": null,
                "results": [{ "pulp_href": "/pulp/api/v3/remotes/rpm/rpm/1/", "name": "os", "url": "https://other/", "pulp_labels": {} }] })))
            .mount(&server)
            .await;
        let it = PlanItem {
            t: RepoType::Rpm,
            name: "os".into(),
            base_path: "os".into(),
            remote_url: "https://up/os/".into(),
            manifest: None,
            action: PlanAction::Create,
            note: String::new(),
        };
        let e = replicate_one(&c, "up", &it, true, &ops::no_progress).await.unwrap_err();
        assert_eq!(e.to_string(), "a local remote named os exists and is not managed by this replication");
        let sent = server.received_requests().await.unwrap();
        assert!(sent.iter().all(|r| r.method.as_str() == "GET"), "nothing changed: {sent:?}");
    }

    #[tokio::test]
    async fn deb_distributions_are_skipped_and_never_replicated() {
        let server = wiremock::MockServer::start().await;
        let p = crate::config::Profile { url: server.uri(), auth: crate::config::AuthMethod::None, ..Default::default() };
        let c = PulpClient::new("up", &p, &crate::config::Settings::default()).unwrap();
        serve_status(&server, &["deb"]).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/pulp/api/v3/distributions/deb/apt/"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({ "count": 1, "next": null,
                "results": [{ "pulp_href": "/pulp/api/v3/distributions/deb/apt/1/", "name": "apt", "base_path": "apt",
                    "base_url": "https://up/pulp/content/apt", "publication": "/pulp/api/v3/publications/deb/apt/1/" }] })))
            .mount(&server)
            .await;
        let plan = plan(&c, &c, &[RepoType::Deb], "", None).await.unwrap();
        assert_eq!((plan.len(), &plan[0].action, plan[0].note.as_str()), (1, &PlanAction::Skip, "Deb replication is not supported"));
        let it = PlanItem { action: PlanAction::Create, ..plan[0].clone() };
        let e = replicate_one(&c, "up", &it, true, &ops::no_progress).await.unwrap_err();
        assert_eq!(e.to_string(), "Deb replication is not supported");
        let sent = server.received_requests().await.unwrap();
        assert!(sent.iter().all(|r| r.method.as_str() == "GET"), "nothing changed: {sent:?}");
    }

    fn client(s: &wiremock::MockServer, name: &str) -> PulpClient {
        let p = crate::config::Profile { url: s.uri(), auth: crate::config::AuthMethod::None, ..Default::default() };
        PulpClient::new(name, &p, &crate::config::Settings::default()).unwrap()
    }

    /// `status/` of a server with `plugins` (before any catch-all mock).
    async fn serve_status(s: &wiremock::MockServer, plugins: &[&str]) {
        let versions: Vec<Value> = plugins.iter().map(|p| json!({ "component": p, "version": "3.0.0" })).collect();
        wiremock::Mock::given(wiremock::matchers::path("/pulp/api/v3/status/"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({ "versions": versions })))
            .with_priority(1)
            .mount(s)
            .await;
    }

    #[tokio::test]
    async fn a_plugin_missing_on_one_side_stops_the_plan() {
        let (up, down) = (wiremock::MockServer::start().await, wiremock::MockServer::start().await);
        serve_status(&up, &["core", "rpm", "file"]).await;
        serve_status(&down, &["core", "rpm"]).await;
        let empty = json!({ "count": 0, "next": null, "results": [] });
        for s in [&up, &down] {
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(empty.clone()))
                .mount(s)
                .await;
        }
        let both = [RepoType::Rpm, RepoType::File];
        let e = plan(&client(&up, "up"), &client(&down, "down"), &both, "", None).await.unwrap_err();
        assert_eq!(e.to_string(), "down (destination): not supported by this server: pulp_file plugin");
        let e = plan(&client(&down, "down"), &client(&up, "up"), &both, "", None).await.unwrap_err();
        assert_eq!(e.to_string(), "down (source): not supported by this server: pulp_file plugin");
        for s in [&up, &down] {
            let sent = s.received_requests().await.unwrap();
            assert!(sent.iter().all(|r| r.url.path() == "/pulp/api/v3/status/"), "nothing listed or changed: {sent:?}");
        }
        assert_eq!(plan(&client(&up, "up"), &client(&down, "down"), &[RepoType::Rpm], "", None).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn a_skipped_sync_is_not_published_or_reported_as_done() {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        let p = crate::config::Profile { url: server.uri(), auth: crate::config::AuthMethod::None, ..Default::default() };
        let c = PulpClient::new("dst", &p, &crate::config::Settings::default()).unwrap();
        let ok = |b: Value| wiremock::ResponseTemplate::new(200).set_body_json(b);
        let (remote, repo) = ("/pulp/api/v3/remotes/file/file/1/", "/pulp/api/v3/repositories/file/file/1/");
        let one = |o: Value| json!({ "count": 1, "next": null, "results": [o] });
        let labels = json!({ LABEL_REPLICA: "up" });
        let r = json!({ "pulp_href": remote, "name": "files", "url": "https://up/files/PULP_MANIFEST", "pulp_labels": labels.clone() });
        wiremock::Mock::given(method("GET")).and(path("/pulp/api/v3/remotes/file/file/")).respond_with(ok(one(r))).mount(&server).await;
        // The repository has the version of an earlier run.
        let v1 = format!("{repo}versions/1/");
        let r =
            json!({ "pulp_href": repo, "name": "files", "latest_version_href": v1, "manifest": "PULP_MANIFEST", "pulp_labels": labels });
        wiremock::Mock::given(method("GET"))
            .and(path("/pulp/api/v3/repositories/file/file/"))
            .respond_with(ok(one(r.clone())))
            .mount(&server)
            .await;
        wiremock::Mock::given(method("GET")).and(path(repo)).respond_with(ok(r)).mount(&server).await;
        wiremock::Mock::given(method("POST"))
            .and(path(format!("{repo}sync/")))
            .respond_with(wiremock::ResponseTemplate::new(202).set_body_json(json!({ "task": "/pulp/api/v3/tasks/1/" })))
            .mount(&server)
            .await;
        let skipped = json!({ "pulp_href": "/pulp/api/v3/tasks/1/", "state": "skipped", "name": "sync" });
        wiremock::Mock::given(method("GET")).and(path("/pulp/api/v3/tasks/1/")).respond_with(ok(skipped)).mount(&server).await;
        let it = PlanItem {
            t: RepoType::File,
            name: "files".into(),
            base_path: "files".into(),
            remote_url: "https://up/files/PULP_MANIFEST".into(),
            manifest: Some("PULP_MANIFEST".into()),
            action: PlanAction::Update,
            note: String::new(),
        };
        let e = replicate_one(&c, "up", &it, false, &ops::no_progress).await.unwrap_err();
        assert_eq!(e.to_string(), "task skipped: ");
        let sent = server.received_requests().await.unwrap();
        let changes: Vec<_> = sent.iter().filter(|r| r.method.as_str() != "GET").map(|r| r.url.path().to_string()).collect();
        assert_eq!(changes, [format!("{repo}sync/")], "no publication, no distribution");
    }

    #[tokio::test]
    async fn a_file_replica_syncs_from_the_manifest_the_upstream_serves() {
        use wiremock::matchers::{method, path, query_param};
        let (up, down) = (wiremock::MockServer::start().await, wiremock::MockServer::start().await);
        serve_status(&up, &["file"]).await;
        serve_status(&down, &["file"]).await;
        let page = |r: Value| json!({ "count": r.as_array().map_or(0, Vec::len), "next": null, "results": r });
        let ok = |b: Value| wiremock::ResponseTemplate::new(200).set_body_json(b);
        let repo = "/pulp/api/v3/repositories/file/file/1/";
        wiremock::Mock::given(method("GET"))
            .and(path("/pulp/api/v3/distributions/file/file/"))
            .respond_with(ok(page(json!([{ "pulp_href": "/pulp/api/v3/distributions/file/file/1/", "name": "files",
                "base_path": "files", "base_url": "https://up/pulp/content/files/", "repository": repo }]))))
            .mount(&up)
            .await;
        // The upstream repository was published with a custom manifest name.
        wiremock::Mock::given(method("GET"))
            .and(path("/pulp/api/v3/publications/file/file/"))
            .and(query_param("repository", repo))
            .respond_with(ok(page(json!([{ "pulp_href": "/p/1/", "repository_version": format!("{repo}versions/1/"),
                "pulp_created": "2026-01-01T00:00:00Z", "manifest": "CUSTOM" }]))))
            .mount(&up)
            .await;
        wiremock::Mock::given(method("GET")).respond_with(ok(page(json!([])))).mount(&down).await;
        let plan = plan(&client(&up, "up"), &client(&down, "down"), &[RepoType::File], "", None).await.unwrap();
        assert_eq!((plan[0].remote_url.as_str(), &plan[0].action), ("https://up/pulp/content/files/CUSTOM", &PlanAction::Create));
        assert_eq!(plan[0].manifest.as_deref(), Some("CUSTOM"), "the replica publishes under that name too");
    }

    /// Only a manifest that the upstream serves makes a remote URL; the plan goes on with the other distributions.
    #[tokio::test]
    async fn a_file_distribution_without_a_served_manifest_is_skipped_with_the_reason() {
        use wiremock::matchers::{method, path};
        let (up, down) = (wiremock::MockServer::start().await, wiremock::MockServer::start().await);
        serve_status(&up, &["file"]).await;
        serve_status(&down, &["file"]).await;
        let page = |r: Value| json!({ "count": r.as_array().map_or(0, Vec::len), "next": null, "results": r });
        let ok = |b: Value| wiremock::ResponseTemplate::new(200).set_body_json(b);
        let dist = |name: &str, k: &str, v: &str| {
            json!({ "pulp_href": format!("/pulp/api/v3/distributions/file/file/{name}/"), "name": name, "base_path": name,
                "base_url": format!("https://up/pulp/content/{name}/"), k: v })
        };
        let dists = json!([
            dist("a", "publication", "/pulp/api/v3/publications/file/file/denied/"),
            dist("b", "repository", "/pulp/api/v3/repositories/file/file/empty/"),
            dist("c", "publication", "/pulp/api/v3/publications/file/file/nomf/"),
            dist("d", "publication", "/pulp/api/v3/publications/file/file/ok/"),
        ]);
        wiremock::Mock::given(method("GET"))
            .and(path("/pulp/api/v3/distributions/file/file/"))
            .respond_with(ok(page(dists)))
            .mount(&up)
            .await;
        let forbidden = wiremock::ResponseTemplate::new(403).set_body_json(json!({ "detail": "no access" }));
        wiremock::Mock::given(method("GET"))
            .and(path("/pulp/api/v3/publications/file/file/denied/"))
            .respond_with(forbidden)
            .mount(&up)
            .await;
        wiremock::Mock::given(method("GET"))
            .and(path("/pulp/api/v3/publications/file/file/"))
            .respond_with(ok(page(json!([]))))
            .mount(&up)
            .await;
        for (p, m) in [("nomf", Value::Null), ("ok", json!("CUSTOM"))] {
            let body = json!({ "pulp_href": format!("/pulp/api/v3/publications/file/file/{p}/"), "manifest": m });
            wiremock::Mock::given(method("GET"))
                .and(path(format!("/pulp/api/v3/publications/file/file/{p}/")))
                .respond_with(ok(body))
                .mount(&up)
                .await;
        }
        wiremock::Mock::given(method("GET")).respond_with(ok(page(json!([])))).mount(&down).await;
        let plan = plan(&client(&up, "up"), &client(&down, "down"), &[RepoType::File], "", None).await.unwrap();
        let rows: Vec<_> = plan.iter().map(|i| (i.name.as_str(), &i.action, i.note.as_str(), i.remote_url.as_str())).collect();
        assert_eq!(
            rows,
            [
                ("a", &PlanAction::Skip, "upstream publication not readable: permission denied [403]: no access", ""),
                ("b", &PlanAction::Skip, "upstream distribution serves nothing (no publication)", ""),
                ("c", &PlanAction::Skip, "upstream publication has no manifest", ""),
                ("d", &PlanAction::Create, "new remote + repository + distribution", "https://up/pulp/content/d/CUSTOM"),
            ]
        );
    }

    #[tokio::test]
    async fn a_file_replica_publishes_under_the_upstream_manifest_name() {
        use wiremock::matchers::{method, path, query_param};
        let server = wiremock::MockServer::start().await;
        let c = client(&server, "dst");
        let ok = |b: Value| wiremock::ResponseTemplate::new(200).set_body_json(b);
        let task = |n: u32| wiremock::ResponseTemplate::new(202).set_body_json(json!({ "task": format!("/pulp/api/v3/tasks/{n}/") }));
        let (remote, repo) = ("/pulp/api/v3/remotes/file/file/1/", "/pulp/api/v3/repositories/file/file/1/");
        let one = |o: Value| json!({ "count": 1, "next": null, "results": [o] });
        let labels = json!({ LABEL_REPLICA: "up" });
        let url = "https://up/files/CUSTOM";
        let r = json!({ "pulp_href": remote, "name": "files", "url": url, "pulp_labels": labels.clone() });
        wiremock::Mock::given(method("GET")).and(path("/pulp/api/v3/remotes/file/file/")).respond_with(ok(one(r))).mount(&server).await;
        // A replica of an earlier run: it publishes PULP_MANIFEST, and its latest version has such a publication.
        let v1 = format!("{repo}versions/1/");
        let r =
            json!({ "pulp_href": repo, "name": "files", "latest_version_href": v1, "manifest": "PULP_MANIFEST", "pulp_labels": labels });
        wiremock::Mock::given(method("GET"))
            .and(path("/pulp/api/v3/repositories/file/file/"))
            .respond_with(ok(one(r.clone())))
            .mount(&server)
            .await;
        wiremock::Mock::given(method("GET")).and(path(repo)).respond_with(ok(r)).mount(&server).await;
        wiremock::Mock::given(method("PATCH")).and(path(repo)).respond_with(ok(json!({}))).mount(&server).await;
        // After the PATCH, the repository names the upstream manifest.
        wiremock::Mock::given(method("GET"))
            .and(path(repo))
            .and(query_param("fields", "manifest"))
            .respond_with(ok(json!({ "manifest": "CUSTOM" })))
            .with_priority(1)
            .mount(&server)
            .await;
        // The sync finds no change: no new version.
        wiremock::Mock::given(method("POST")).and(path(format!("{repo}sync/"))).respond_with(task(1)).mount(&server).await;
        let done = |n: u32, made: Value| json!({ "pulp_href": format!("/pulp/api/v3/tasks/{n}/"), "state": "completed", "created_resources": made });
        let get_task = |n: u32, made: Value| {
            wiremock::Mock::given(method("GET")).and(path(format!("/pulp/api/v3/tasks/{n}/"))).respond_with(ok(done(n, made)))
        };
        get_task(1, json!([])).mount(&server).await;
        let pubs = "/pulp/api/v3/publications/file/file/";
        let old = json!({ "pulp_href": format!("{pubs}1/"), "repository_version": v1, "manifest": "PULP_MANIFEST" });
        wiremock::Mock::given(method("GET")).and(path(pubs)).respond_with(ok(one(old))).mount(&server).await;
        wiremock::Mock::given(method("POST")).and(path(pubs)).respond_with(task(2)).mount(&server).await;
        get_task(2, json!([format!("{pubs}2/")])).mount(&server).await;
        let it = PlanItem {
            t: RepoType::File,
            name: "files".into(),
            base_path: "files".into(),
            remote_url: url.into(),
            manifest: Some("CUSTOM".into()),
            action: PlanAction::Update,
            note: String::new(),
        };
        replicate_one(&c, "up", &it, false, &ops::no_progress).await.unwrap_err(); // the distribution step is not served
        let sent = server.received_requests().await.unwrap();
        let bodies = |m: &str, p: &str| -> Vec<Value> {
            sent.iter().filter(|r| r.method.as_str() == m && r.url.path() == p).map(|r| r.body_json().unwrap()).collect()
        };
        assert_eq!(bodies("PATCH", repo), [json!({ "manifest": "CUSTOM" })]);
        assert_eq!(bodies("POST", pubs), [json!({ "repository_version": v1, "manifest": "CUSTOM" })], "the old publication is not reused");
    }
}
