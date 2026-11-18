//! Replication from an upstream Pulp.
//!
//! * Native: pulpcore's Upstream Pulp API (technology preview). It replicates distributions,
//!   repositories and remotes as Pulp metadata, governed by the server-side `policy`.
//! * Fallback: content synchronization only. For each selected upstream RPM, File or Deb
//!   distribution an ordinary remote (pointing at the published upstream content), a
//!   repository and a pinned distribution are created on the destination and synced.
//!   It does not replicate content guards, labels or other distribution metadata, and it
//!   never deletes destination objects. Objects are matched by name and owned via the
//!   `pulp_tui_replica` label, so reruns are idempotent and foreign objects are not touched.

use crate::pulp::client::{Mutation, PulpClient};
use crate::pulp::models::{Distribution, Repository, TaskGroup};
use crate::pulp::ops::{self, OnTask};
use crate::pulp::{Capabilities, PulpError, RepoType, deb};
use crate::store::Store;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

pub const LABEL_REPLICA: &str = "pulp_tui_replica";

/// Trigger native replication and wait for its task group; `on` gets each snapshot. Any outcome
/// but `completed` (a failed, canceled or skipped task) is `PulpError::TaskFailed` with that state.
pub async fn replicate_native(c: &PulpClient, upstream_href: &str, on: &(dyn Fn(&TaskGroup) + Send + Sync)) -> Result<Vec<String>> {
    let Mutation::TaskGroup(g) = c.post(&format!("{upstream_href}replicate/"), &json!({})).await? else {
        bail!("replicate did not return a task group");
    };
    let (group, lines) = ops::group_report(c, &g, on).await?;
    if group.outcome() != "completed" {
        return Err(PulpError::TaskFailed { href: g, state: group.outcome().into(), message: lines.join("\n") }.into());
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
    /// Deb: the `dists/` directories that the upstream publication serves (space separated); the replica remote syncs them.
    pub distributions: Option<String>,
    /// The publication mode of the replica (`ops::publication_mode`). Deb: the mode of the upstream publication.
    pub mode: Value,
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
                distributions: None,
                mode: json!({}),
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
            // The destination downloads from here: a path is on the server of the source profile.
            let base = src.content_url(&base);
            if dst_content_base.is_some_and(|b| base.starts_with(b)) {
                it.note = "upstream content is served by the destination itself (loop)".into();
                out.push(it);
                continue;
            }
            let base = if base.ends_with('/') { base } else { format!("{base}/") };
            // The served publication: pulp_file serves only from publications (its manifest name makes the URL), the
            // Deb layout depends on it, and a checkpoint distribution serves its newest checkpoint publication at
            // `<base>/<timestamp>/` only (its bare base path lists the timestamps). An RPM one that is not a
            // checkpoint distribution needs no read. No OpenAPI: unknown endpoints count as present, so the Deb
            // verbatim publications of a repository are read too.
            let served = if t == RepoType::Rpm && !d.checkpoint {
                Value::Null
            } else {
                match ops::served_publication(src, &Capabilities::default(), t, &json!(d)).await {
                    Ok(Some(p)) => p,
                    Ok(None) => {
                        it.note = "upstream distribution serves nothing (no publication)".into();
                        out.push(it);
                        continue;
                    }
                    Err(e) => {
                        it.note = format!("upstream publication not readable: {e}");
                        out.push(it);
                        continue;
                    }
                }
            };
            let url = ops::served_url(&base, d.checkpoint, &served);
            let mut upstream_signed = false;
            it.remote_url = match t {
                RepoType::Rpm => url.clone(),
                RepoType::File => match served["manifest"].as_str() {
                    Some(m) => {
                        it.manifest = Some(m.into());
                        format!("{url}{m}")
                    }
                    None => {
                        it.note = "upstream publication has no manifest".into();
                        out.push(it);
                        continue;
                    }
                },
                RepoType::Deb => match deb_source(src, &served).await {
                    Ok((dists, mode, signed)) => {
                        (it.distributions, it.mode, upstream_signed) = (Some(dists), mode, signed);
                        url.clone()
                    }
                    Err(note) => {
                        it.note = note;
                        out.push(it);
                        continue;
                    }
                },
            };
            let repo = ops::find_by_name::<Repository>(dst, &dst.api(t.repositories()), &d.name).await?;
            let remote = ops::find_by_name::<Value>(dst, &dst.api(t.remotes()), &d.name).await?;
            if let Some(why) = conflict(dst, &it, src_name, repo.as_ref(), remote.as_ref()).await? {
                it.note = format!("conflict: {why}");
            } else if repo.is_some() {
                it.action = PlanAction::Update;
                it.note = "re-sync existing replica".into();
            } else {
                it.action = PlanAction::Create;
                it.note = "new remote + repository + distribution".into();
            }
            // pulp_deb signs a publication without a service with the repository's (COMPATIBILITY.md).
            let replica_signs = repo.as_ref().is_some_and(|r| r.extra.get("signing_service").is_some_and(|s| !s.is_null()));
            if it.action != PlanAction::Skip && upstream_signed && !replica_signs {
                it.note += "; upstream Release is signed, the replica will be unsigned (the destination repository has no signing service)";
            }
            if it.action != PlanAction::Skip && d.checkpoint {
                let at = url.trim_end_matches('/').rsplit('/').next().unwrap_or_default();
                it.note += &format!(
                    "; upstream is a checkpoint distribution: the replica follows the newest checkpoint at the time of the run ({at})"
                );
            }
            out.push(it);
        }
    }
    Ok(out)
}

/// Why the run stops for the replica `it` on `dst`, before it changes anything: a local `repo` or `remote` of its name
/// without the label of `src`, or that pulp-tui cannot show it made with the create body of the run
/// (`ops::refuse_foreign`, as in `replica_repo` and `replica_remote`), or the refusal of the distribution step
/// (`ops::distribution_target`, as in `ops::ensure_distribution`: labels, base path, checkpoint, roles and fields, and
/// a base path that overlaps another one for a new distribution). `None`: the run can make or update the replica.
async fn conflict(dst: &PulpClient, it: &PlanItem, src: &str, repo: Option<&Repository>, remote: Option<&Value>) -> Result<Option<String>> {
    let foreign = [
        repo.filter(|r| !owned(&r.pulp_labels, src)).map(|_| "repository"),
        remote.filter(|r| !owned(&labels_of(r), src)).map(|_| "remote"),
    ];
    if let Some(kind) = foreign.into_iter().flatten().next() {
        return Ok(Some(format!("a local {kind} named {} exists and is not managed by this replication", it.name)));
    }
    let labels = json!({ LABEL_REPLICA: src });
    // Without a remote, the run makes one first; a repository without a default remote is fine (`ops::refuse_foreign`).
    let remote_href = remote.and_then(|r| r["pulp_href"].as_str()).unwrap_or_default();
    let found = [
        ("repository", repo.map(|r| json!(r)), repo_body(it, remote_href, &labels)),
        ("remote", remote.cloned(), remote_body(it, &labels)),
    ];
    let checks = async {
        for (kind, obj, body) in found.into_iter().filter_map(|(k, o, b)| o.map(|o| (k, o, b))) {
            ops::refuse_foreign(dst, &format!("{kind} {}", it.name), &obj, &body, false).await?;
        }
        ops::distribution_target(dst, it.t, &it.name, &it.base_path, &labels, None).await
    };
    match checks.await {
        Ok(_) => Ok(None),
        // Both only read; a 409 is their refusal.
        Err(PulpError::Api { status: 409, message }) => Ok(Some(message)),
        Err(e) => Err(e.into()),
    }
}

/// What the Deb replica of the upstream publication `p` (the one its distribution serves) syncs and how it publishes:
/// the `dists/` directories of `p` (`deb::layout`), the publication's mode without its signing service (an upstream
/// href), and whether upstream signs that APT publication. pulp_deb signs with the publication's service, else with
/// the repository's release override (structured releases only) or service. A verbatim publication serves the synced
/// signatures as they are, so its replica serves them too. Only the API is read: `base_url` is the upstream content
/// origin as the destination server reaches it, which this client may not resolve. `Err`: the note of a SKIP row.
/// A verbatim publication of a flat repository serves it at the root: the replica remote syncs `/` (`deb::layout`).
async fn deb_source(src: &PulpClient, p: &Value) -> std::result::Result<(String, Value, bool), String> {
    let unreadable = |e: PulpError| format!("upstream publication not readable: {e}");
    let version = p["repository_version"].as_str().unwrap_or_default();
    let (dists, _) = deb::layout(src, version, p).await.map_err(unreadable)?;
    if dists.is_empty() {
        return Err("upstream verbatim publication serves no Release file (no sync made its version); publish it as APT".into());
    }
    let mut mode = ops::publication_mode(RepoType::Deb, p);
    if let Some(m) = mode.as_object_mut() {
        m.remove("signing_service");
    }
    let signed = mode[deb::VERBATIM] != true
        && (!p["signing_service"].is_null() || {
            let q = [("fields", "signing_service,signing_service_release_overrides".to_string())];
            let r: Value = src.get(&ops::repo_of_version(version), &q).await.map_err(unreadable)?;
            let overrides = r["signing_service_release_overrides"].as_object().is_some_and(|o| !o.is_empty());
            !r["signing_service"].is_null() || (p["structured"] != false && overrides)
        });
    Ok((dists.join(" "), mode, signed))
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
    let labels = json!({ LABEL_REPLICA: src_profile });
    let remote = replica_remote(c, src_profile, it, &labels, on).await?;
    let repo = replica_repo(c, src_profile, it, &remote, &labels, on).await?;
    ops::settle(c, ops::sync(c, it.t, &repo, Some(&remote), sync_policy(it.t, mirror)).await?, on).await?;
    let r: Repository = c.get(&repo, &[]).await?;
    let version = r.latest_version_href.ok_or_else(|| anyhow!("no repository version after sync"))?;
    serve_replica(c, it, &version, &labels, on).await
}

/// The sync policy of a replica: additive, or mirror (`mirror_content_only` for RPM).
fn sync_policy(t: RepoType, mirror: bool) -> &'static str {
    match (t, mirror) {
        (_, false) => "additive",
        (RepoType::Rpm, true) => "mirror_content_only",
        (_, true) => "mirror",
    }
}

/// The publication of `version` in the mode of `it`: an existing one, else a new one. Not `ensure_publication`: a
/// signing replica would get a new publication on every run. A reused unsigned RPM one fails `verify_served` in
/// `serve_replica` (repomd.xml.asc); Deb verify does not check signatures. Only one that is as pulp-tui makes it is
/// reused (`ops::own_publication`; replicas are not recorded): the notes say why one was not.
async fn replica_publication(c: &PulpClient, it: &PlanItem, version: &str, on: OnTask<'_>) -> Result<(String, Vec<String>)> {
    Ok(match ops::own_publication(c, it.t, version, &it.mode, &[]).await? {
        (Some(p), notes) => (p, notes),
        (None, notes) => (ops::create_publication(c, it.t, version, &it.mode, on).await?, notes),
    })
}

/// Publish `version` of the replica of `it`, distribute it at the base path of `it` and check what it serves.
async fn serve_replica(c: &PulpClient, it: &PlanItem, version: &str, labels: &Value, on: OnTask<'_>) -> Result<String> {
    let t = it.t;
    let (publication, skipped) = replica_publication(c, it, version, on).await?;
    let dist = ops::ensure_distribution(c, t, &it.name, &it.base_path, Some(&publication), labels, None, on).await?;
    let d: Distribution = c.get(&dist, &[]).await?;
    let p: Value = c.get(&publication, &[]).await?;
    let base = c.content_url(d.base_url.as_deref().unwrap_or_default());
    let check = match ops::verify_served(c, t, &base, version, &p).await {
        Err(e @ PulpError::NotCompared(_)) => e.to_string(),
        r => r?,
    };
    let notes: String = skipped.iter().map(|why| format!("; {why}")).collect();
    Ok(format!("{} served at {base} ({check}){notes}", short_version(version)))
}

/// The replica `kind` (remote, repository) of `it` in `collection`, with its href, when there is one. Checked again at
/// run time (an object with the name of `it` may have been created since the dry run): refused without the label of
/// `src`, or when pulp-tui cannot show that it made it with the create `body` (`ops::refuse_foreign`; replicas are not
/// recorded).
async fn own_replica(
    c: &PulpClient,
    (collection, kind): (&str, &str),
    src: &str,
    it: &PlanItem,
    body: &Value,
) -> Result<Option<(String, Value)>> {
    let Some(r) = ops::find_by_name::<Value>(c, &c.api(collection), &it.name).await? else { return Ok(None) };
    if !owned(&labels_of(&r), src) {
        bail!("a local {kind} named {} exists and is not managed by this replication", it.name);
    }
    ops::refuse_foreign(c, &format!("{kind} {}", it.name), &r, body, false).await?;
    Ok(Some((r["pulp_href"].as_str().unwrap_or_default().to_string(), r)))
}

/// The fields of the replica remote of `it` that every run keeps in step with upstream: its URL (Deb: and the dists
/// that upstream serves).
fn remote_fields(it: &PlanItem) -> Value {
    let mut want = json!({ "url": it.remote_url });
    if let Some(d) = &it.distributions {
        want["distributions"] = d.as_str().into();
    }
    want
}

/// The create body of the replica remote of `it`: `remote_fields`, its name, the `immediate` policy and `labels`.
fn remote_body(it: &PlanItem, labels: &Value) -> Value {
    let mut body = remote_fields(it);
    (body["name"], body["policy"], body["pulp_labels"]) = (it.name.as_str().into(), "immediate".into(), labels.clone());
    body
}

/// The create body of the replica repository of `it`, with `remote` as its default remote. File: it publishes under the
/// manifest name that upstream serves (`ensure_publication` reuses only a publication with the repository's name).
fn repo_body(it: &PlanItem, remote: &str, labels: &Value) -> Value {
    let mut body = json!({ "name": it.name, "remote": remote, "pulp_labels": labels });
    if let Some(m) = &it.manifest {
        body["manifest"] = m.as_str().into();
    }
    body
}

/// The replica remote of `it`, pointed at the upstream URL (`remote_fields`).
async fn replica_remote(c: &PulpClient, src_profile: &str, it: &PlanItem, labels: &Value, on: OnTask<'_>) -> Result<String> {
    let (want, body) = (remote_fields(it), remote_body(it, labels));
    match own_replica(c, (it.t.remotes(), "remote"), src_profile, it, &body).await? {
        Some((h, r)) => {
            if want.as_object().into_iter().flatten().any(|(k, v)| r[k] != *v) {
                ops::settle(c, c.patch(&h, &want).await?, on).await?;
            }
            Ok(h)
        }
        None => Ok(ops::create(c, &c.api(it.t.remotes()), &body, "/remotes/", on).await?),
    }
}

/// The replica repository of `it` (`repo_body`), with the manifest name that upstream serves.
async fn replica_repo(c: &PulpClient, src_profile: &str, it: &PlanItem, remote: &str, labels: &Value, on: OnTask<'_>) -> Result<String> {
    let body = repo_body(it, remote, labels);
    match own_replica(c, (it.t.repositories(), "repository"), src_profile, it, &body).await? {
        Some((h, r)) => {
            if let Some(m) = it.manifest.as_ref().map(|m| json!({ "manifest": m })).filter(|m| r["manifest"] != m["manifest"]) {
                ops::settle(c, c.patch(&h, &m).await?, on).await?;
            }
            Ok(h)
        }
        None => Ok(ops::create(c, &c.api(it.t.repositories()), &body, "/repositories/", on).await?),
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
            distributions: None,
            mode: json!({}),
            action: PlanAction::Create,
            note: String::new(),
        };
        let e = replicate_one(&c, "up", &it, true, &ops::no_progress).await.unwrap_err();
        assert_eq!(e.to_string(), "a local remote named os exists and is not managed by this replication");
        let sent = server.received_requests().await.unwrap();
        assert!(sent.iter().all(|r| r.method.as_str() == "GET"), "nothing changed: {sent:?}");
    }

    /// A rerun with no upstream change syncs no new version, so it may find a publication of that version that another
    /// Pulp user made (a publication creator role and view permission on the repository are enough), with other
    /// settings (`gpgcheck` off). It is not reused: a new publication is made, and the result says why. One that only
    /// this Pulp user (`op`) holds a role on and that has the settings of a new one is reused.
    #[tokio::test]
    async fn a_replica_publication_that_another_user_made_is_not_reused() {
        use wiremock::matchers::{method, path};
        let (repo, pubs) = ("/pulp/api/v3/repositories/rpm/rpm/1/", "/pulp/api/v3/publications/rpm/rpm/");
        let (version, found, made) = (format!("{repo}versions/2/"), format!("{pubs}2/"), format!("{pubs}9/"));
        let settings = |config: Value| json!({ "checksum_type": "sha256", "compression_type": null, "layout": "nested_alphabetically", "repo_config": config });
        let (own, planted) = (settings(json!({})), settings(json!({ "gpgcheck": 0 })));
        let role = "user eve has role rpm.rpmpublication_owner on it";
        let field = r#"its field differs from a new one (repo_config is {"gpgcheck":0}, not {})"#;
        // (holders of its owner role, its settings, why it is not reused)
        for (holders, has, skipped) in [(vec!["eve"], &own, Some(role)), (vec!["op"], &planted, Some(field)), (vec!["op"], &own, None)] {
            let server = wiremock::MockServer::start().await;
            let c = client(&server, "dst");
            let get = |p: &str| wiremock::Mock::given(method("GET")).and(path(p.to_string()));
            let ok = |b: Value| wiremock::ResponseTemplate::new(200).set_body_json(b);
            get("/pulp/api/v3/login/").respond_with(ok(json!({ "username": "op" }))).mount(&server).await;
            let roles = json!({ "roles": [{ "role": "rpm.rpmpublication_owner", "users": holders, "groups": [] }] });
            get(&format!("{found}list_roles/")).respond_with(ok(roles)).mount(&server).await;
            get(repo).respond_with(ok(json!({ "pulp_href": repo, "repo_config": {}, "checksum_type": null }))).mount(&server).await;
            let mut candidate = json!({ "pulp_href": found, "repository_version": version });
            candidate.as_object_mut().unwrap().extend(has.as_object().unwrap().clone());
            get(pubs).respond_with(ok(json!({ "count": 1, "next": null, "results": [candidate] }))).mount(&server).await;
            let task = wiremock::ResponseTemplate::new(202).set_body_json(json!({ "task": "/pulp/api/v3/tasks/1/" }));
            wiremock::Mock::given(method("POST")).and(path(pubs)).respond_with(task).mount(&server).await;
            let done = json!({ "pulp_href": "/pulp/api/v3/tasks/1/", "state": "completed", "created_resources": [made] });
            get("/pulp/api/v3/tasks/1/").respond_with(ok(done)).mount(&server).await;
            let it = PlanItem {
                t: RepoType::Rpm,
                name: "os".into(),
                base_path: "os".into(),
                remote_url: "https://up/os/".into(),
                manifest: None,
                distributions: None,
                mode: json!({}),
                action: PlanAction::Update,
                note: String::new(),
            };
            let (p, notes) = replica_publication(&c, &it, &version, &ops::no_progress).await.unwrap();
            let posts = server.received_requests().await.unwrap().iter().filter(|r| r.method.as_str() == "POST").count();
            let Some(why) = skipped else {
                assert_eq!((p.as_str(), posts, notes.len()), (found.as_str(), 0, 0), "{holders:?} {has}: reused");
                continue;
            };
            assert_eq!((p.as_str(), posts), (made.as_str(), 1), "{holders:?} {has}: a new one");
            assert_eq!(notes, [format!("publication {found} is not reused: {why}")]);
        }
    }

    /// Another Pulp user made the remote or the repository of the replica with the replica label (before the first
    /// run): Pulp gave that user the owner role of it, so the run refuses it before any change. Roles alone do not prove
    /// who made it (its maker can hand the owner role to this user, or drop it), so a remote or repository with fields
    /// that the replication does not set (a proxy, no TLS check, a secret, another default remote, a retention) is
    /// refused too, whoever holds roles on it.
    #[tokio::test]
    async fn run_refuses_a_replica_remote_or_repository_that_another_user_made() {
        use wiremock::matchers::{method, path};
        let get = |p: &str| wiremock::Mock::given(method("GET")).and(path(format!("/pulp/api/v3/{p}")));
        let ok = |b: Value| wiremock::ResponseTemplate::new(200).set_body_json(b);
        let role = |kind: &str, coll: &str| {
            format!(
                "{kind} os exists, but this database does not record it and user eve has role {coll}_owner on it: delete it, or remove that role"
            )
        };
        let fields = |kind: &str, n: &str, list: &str| {
            let fix = if n == "s" { "them" } else { "it" };
            format!(
                "{kind} os exists, but this database does not record it and its field{n} differ{} from a new one ({list}): delete it, or set {fix} back",
                if n == "s" { "" } else { "s" }
            )
        };
        let (none, mine) = (json!({}), vec!["op"]);
        let proxy = json!({ "tls_validation": false, "proxy_url": "http://evil:3128", "ca_cert": null });
        let secret = json!({ "hidden_fields": [{ "name": "username", "is_set": false }, { "name": "password", "is_set": true }] });
        let other = json!({ "remote": "/pulp/api/v3/remotes/rpm/rpm/9/", "retain_repo_versions": 2, "autopublish": false });
        // (remote: holders, other fields; repository: holders, other fields; the refusal)
        for (remote_by, remote_has, repo_by, repo_has, want) in [
            (vec!["eve"], &none, mine.clone(), &none, role("remote", "remotes")),
            (mine.clone(), &none, vec!["eve"], &none, role("repository", "repositories")),
            (
                mine.clone(),
                &proxy,
                mine.clone(),
                &none,
                fields("remote", "s", "tls_validation is false, not true; proxy_url is \"http://evil:3128\", not null"),
            ),
            (vec![], &secret, mine.clone(), &none, fields("remote", "", "password is set")),
            (
                vec![],
                &none,
                vec![],
                &other,
                fields(
                    "repository",
                    "s",
                    "remote is \"/pulp/api/v3/remotes/rpm/rpm/9/\", not \"/pulp/api/v3/remotes/rpm/rpm/1/\"; retain_repo_versions is 2, not null",
                ),
            ),
        ] {
            let server = wiremock::MockServer::start().await;
            let c = client(&server, "dst");
            get("login/").respond_with(ok(json!({ "username": "op" }))).mount(&server).await;
            for (coll, by, has) in [("remotes", remote_by, remote_has), ("repositories", repo_by, repo_has)] {
                let mut o = json!({ "pulp_href": format!("/pulp/api/v3/{coll}/rpm/rpm/1/"), "name": "os", "url": "https://up/os/",
                    "pulp_labels": { LABEL_REPLICA: "up" } });
                o.as_object_mut().unwrap().extend(has.as_object().unwrap().clone());
                get(&format!("{coll}/rpm/rpm/")).respond_with(ok(json!({ "count": 1, "next": null, "results": [o] }))).mount(&server).await;
                let roles = json!({ "roles": [{ "role": format!("{coll}_owner"), "users": by, "groups": [] }] });
                get(&format!("{coll}/rpm/rpm/1/list_roles/")).respond_with(ok(roles)).mount(&server).await;
            }
            let it = PlanItem {
                t: RepoType::Rpm,
                name: "os".into(),
                base_path: "os".into(),
                remote_url: "https://up/os/".into(),
                manifest: None,
                distributions: None,
                mode: json!({}),
                action: PlanAction::Update,
                note: String::new(),
            };
            let e = replicate_one(&c, "up", &it, true, &ops::no_progress).await.unwrap_err();
            assert_eq!(e.to_string(), format!("conflict [409]: {want}, then try again"));
            let sent = server.received_requests().await.unwrap();
            assert!(sent.iter().all(|r| r.method.as_str() == "GET"), "nothing changed: {sent:?}");
        }
    }

    /// A Deb replica syncs the `dists/` directories that the upstream publication serves (`deb::layout`) and
    /// publishes in its mode, without the upstream signing service. The note says when the replica loses the
    /// upstream signature: an APT publication signed upstream (by its own service, else by its repository's service
    /// or, when structured, a release override), and no signing service on the replica repository. The client may
    /// not reach the upstream content origin, so only the API is read.
    #[tokio::test]
    async fn a_deb_replica_syncs_the_served_dists_and_takes_the_upstream_mode() {
        use wiremock::matchers::{method, path, query_param};
        let (up, down) = (wiremock::MockServer::start().await, wiremock::MockServer::start().await);
        serve_status(&up, &["deb"]).await;
        serve_status(&down, &["deb"]).await;
        let page = |r: Value| json!({ "count": r.as_array().map_or(0, Vec::len), "next": null, "results": r });
        let ok = |b: Value| wiremock::ResponseTemplate::new(200).set_body_json(b);
        let get = |p: &str| wiremock::Mock::given(method("GET")).and(path(format!("/pulp/api/v3/{p}")));
        // Served from a content origin that this client cannot resolve (only the destination server can).
        let base = |name: &str| format!("http://unresolvable.invalid/pulp/content/{name}/");
        let dist = |name: &str, k: &str, v: &str| {
            json!({ "pulp_href": format!("/pulp/api/v3/distributions/deb/apt/{name}/"), "name": name, "base_path": name,
                "base_url": base(name), k: v })
        };
        let repo = |n: u32| format!("/pulp/api/v3/repositories/deb/apt/{n}/");
        let publication = |n: u32| format!("/pulp/api/v3/publications/deb/apt/{n}/");
        let dists = json!([
            dist("apt", "publication", &publication(1)),
            dist("flat", "publication", &publication(2)),
            dist("plain", "publication", &publication(4)),
            dist("simple", "publication", &publication(3)),
            dist("verb", "repository", &repo(9)),
            dist("verbflat", "publication", "/pulp/api/v3/publications/deb/verbatim/5/"),
            dist("verbnone", "publication", "/pulp/api/v3/publications/deb/verbatim/6/"),
        ]);
        get("distributions/deb/apt/").respond_with(ok(page(dists))).mount(&up).await;
        // Publications that sign with a service of the upstream server (1), with a release override of their
        // repository (2, structured), with the service of their repository (3, simple). A simple release does not
        // use a release override (4).
        let apt = |n: u32, mode: Value, service: Value| {
            let mut p = json!({ "pulp_href": publication(n), "repository_version": format!("{}versions/1/", repo(n)),
                "signing_service": service });
            p.as_object_mut().unwrap().extend(mode.as_object().unwrap().clone());
            p
        };
        let structured = json!({ "simple": false, "structured": true, "publish_upstream_release_fields": true });
        let service = |n: u32| json!(format!("/pulp/api/v3/signing-services/{n}/"));
        get("publications/deb/apt/1/").respond_with(ok(apt(1, structured.clone(), service(7)))).mount(&up).await;
        get("publications/deb/apt/2/").respond_with(ok(apt(2, structured.clone(), Value::Null))).mount(&up).await;
        let simple = json!({ "simple": true, "structured": false });
        get("publications/deb/apt/3/").respond_with(ok(apt(3, simple.clone(), Value::Null))).mount(&up).await;
        get("publications/deb/apt/4/").respond_with(ok(apt(4, simple.clone(), Value::Null))).mount(&up).await;
        let signs =
            |service: Value, overrides: Value| ok(json!({ "signing_service": service, "signing_service_release_overrides": overrides }));
        get("repositories/deb/apt/2/").respond_with(signs(Value::Null, json!({ "fixture": service(7) }))).mount(&up).await;
        get("repositories/deb/apt/3/").respond_with(signs(service(8), json!({}))).mount(&up).await;
        get("repositories/deb/apt/4/").respond_with(signs(Value::Null, json!({ "default": service(7) }))).mount(&up).await;
        // The repository of "verb" serves its newest publication: a verbatim one, which serves the synced signatures.
        get("repositories/deb/apt/9/").respond_with(signs(service(9), json!({}))).mount(&up).await;
        get("publications/deb/apt/").and(query_param("repository", repo(9))).respond_with(ok(page(json!([])))).mount(&up).await;
        let v9 = format!("{}versions/3/", repo(9));
        let p9 = json!([{ "pulp_href": "/pulp/api/v3/publications/deb/verbatim/3/", "repository_version": v9,
            "pulp_created": "2026-01-01T00:00:00Z" }]);
        get("publications/deb/verbatim/").and(query_param("repository", repo(9))).respond_with(ok(page(p9))).mount(&up).await;
        // "verbflat" serves a verbatim publication of a flat repository synced with distributions "/": pulp_deb stores
        // it as distribution flat-repo, and its Release file is at the root (relative path "Release").
        // "verbnone" serves a verbatim publication of a version that no sync made: no Release file, nothing to sync.
        let (v5, v6) = (format!("{}versions/1/", repo(5)), format!("{}versions/1/", repo(6)));
        for (n, v) in [(5, &v5), (6, &v6)] {
            let p = json!({ "pulp_href": format!("/pulp/api/v3/publications/deb/verbatim/{n}/"), "repository_version": v });
            get(&format!("publications/deb/verbatim/{n}/")).respond_with(ok(p)).mount(&up).await;
        }
        let comp = |d: &str| json!({ "distribution": d, "plain_component": "main" });
        let flat = json!([{ "distribution": "flat-repo", "plain_component": "flat-repo-component" }]);
        for (version, comps) in [
            (format!("{}versions/1/", repo(1)), json!([comp("fixture")])),
            (format!("{}versions/1/", repo(2)), json!([comp("fixture"), comp("/")])),
            (v9.clone(), json!([comp("fixture")])),
            (v5.clone(), flat),
            (v6.clone(), json!([])),
        ] {
            get("content/deb/release_components/")
                .and(query_param("repository_version", version.as_str()))
                .respond_with(ok(page(comps)))
                .mount(&up)
                .await;
        }
        let file = |d: &str, p: &str| json!([{ "distribution": d, "relative_path": p }]);
        for (version, files) in [(v9, file("fixture", "dists/fixture/Release")), (v5, file("flat-repo", "Release")), (v6, json!([]))] {
            get("content/deb/release_files/")
                .and(query_param("repository_version", version.as_str()))
                .respond_with(ok(page(files)))
                .mount(&up)
                .await;
        }
        // The destination has a replica of "apt", whose repository signs with its own service.
        let r = json!({ "pulp_href": "/pulp/api/v3/repositories/deb/apt/1/", "name": "apt",
            "signing_service": "/pulp/api/v3/signing-services/1/", "pulp_labels": { LABEL_REPLICA: "up" } });
        get("repositories/deb/apt/").and(query_param("name", "apt")).respond_with(ok(page(json!([r])))).mount(&down).await;
        wiremock::Mock::given(method("GET")).respond_with(ok(page(json!([])))).with_priority(10).mount(&down).await;
        let plan = plan(&client(&up, "up"), &client(&down, "down"), &[RepoType::Deb], "", None).await.unwrap();
        let rows: Vec<_> =
            plan.iter().map(|i| (i.name.as_str(), &i.action, i.remote_url.clone(), i.distributions.as_deref(), i.mode.clone())).collect();
        assert_eq!(
            rows,
            [
                ("apt", &PlanAction::Update, base("apt"), Some("fixture"), structured.clone()),
                ("flat", &PlanAction::Create, base("flat"), Some("fixture flat-repo"), structured),
                ("plain", &PlanAction::Create, base("plain"), Some("default"), simple.clone()),
                ("simple", &PlanAction::Create, base("simple"), Some("default"), simple),
                ("verb", &PlanAction::Create, base("verb"), Some("fixture"), json!({ "verbatim": true })),
                // A pulp_deb remote syncs a flat repository at the root as distribution "/".
                ("verbflat", &PlanAction::Create, base("verbflat"), Some("/"), json!({ "verbatim": true })),
                ("verbnone", &PlanAction::Skip, String::new(), None, json!({})),
            ]
        );
        let new = "new remote + repository + distribution";
        let loses_signature =
            format!("{new}; upstream Release is signed, the replica will be unsigned (the destination repository has no signing service)");
        let none = "upstream verbatim publication serves no Release file (no sync made its version); publish it as APT";
        let notes: Vec<_> = plan.iter().map(|i| i.note.as_str()).collect();
        assert_eq!(notes, ["re-sync existing replica", &loses_signature, new, &loses_signature, new, new, none]);
    }

    /// A Deb replica: the remote syncs the served dists, the publication has the upstream mode, the distribution
    /// is made, and what it serves is verified.
    #[tokio::test]
    async fn a_deb_replica_is_synced_published_in_the_upstream_mode_distributed_and_verified() {
        use sha2::Digest;
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        let c = client(&server, "dst");
        let page = |r: Value| json!({ "count": r.as_array().map_or(0, Vec::len), "next": null, "results": r });
        let ok = |b: Value| wiremock::ResponseTemplate::new(200).set_body_json(b);
        let task = |n: u32| wiremock::ResponseTemplate::new(202).set_body_json(json!({ "task": format!("/pulp/api/v3/tasks/{n}/") }));
        let at = |m: &str, p: &str| wiremock::Mock::given(method(m)).and(path(p.to_string()));
        let (remotes, repos, pubs, dists) = (
            "/pulp/api/v3/remotes/deb/apt/",
            "/pulp/api/v3/repositories/deb/apt/",
            "/pulp/api/v3/publications/deb/apt/",
            "/pulp/api/v3/distributions/deb/apt/",
        );
        let (remote, repo, publication, dist) = (format!("{remotes}1/"), format!("{repos}1/"), format!("{pubs}1/"), format!("{dists}1/"));
        let v1 = format!("{repo}versions/1/");
        let base = format!("{}/pulp/content/apt/", server.uri());
        // Nothing exists yet: every list is empty unless a mock below answers.
        wiremock::Mock::given(method("GET")).respond_with(ok(page(json!([])))).with_priority(10).mount(&server).await;
        let created = |h: &str| wiremock::ResponseTemplate::new(201).set_body_json(json!({ "pulp_href": h }));
        at("POST", remotes).respond_with(created(&remote)).mount(&server).await;
        at("POST", repos).respond_with(created(&repo)).mount(&server).await;
        at("POST", &format!("{repo}sync/")).respond_with(task(1)).mount(&server).await;
        at("POST", pubs).respond_with(task(2)).mount(&server).await;
        at("POST", dists).respond_with(task(3)).mount(&server).await;
        for (n, made) in [(1, json!([])), (2, json!([publication])), (3, json!([dist]))] {
            let t = json!({ "pulp_href": format!("/pulp/api/v3/tasks/{n}/"), "state": "completed", "created_resources": made });
            at("GET", &format!("/pulp/api/v3/tasks/{n}/")).respond_with(ok(t)).mount(&server).await;
        }
        at("GET", &repo).respond_with(ok(json!({ "pulp_href": repo, "name": "apt", "latest_version_href": v1 }))).mount(&server).await;
        let d = json!({ "pulp_href": dist, "name": "apt", "base_path": "apt", "base_url": base, "publication": publication });
        at("GET", &dist).respond_with(ok(d)).mount(&server).await;
        let p = json!({ "pulp_href": publication, "repository_version": v1, "simple": true, "structured": false });
        at("GET", &publication).respond_with(ok(p)).mount(&server).await;
        // What a simple publication serves (`deb::verify`).
        let pkg = json!([{ "pulp_href": "/p/1/", "package": "fixture-beta", "version": "1.0-1", "architecture": "amd64",
            "relative_path": "pool/x.deb", "sha256": hex::encode(sha2::Sha256::digest(b"deb bytes")) }]);
        at("GET", "/pulp/api/v3/content/deb/packages/").respond_with(ok(page(pkg))).mount(&server).await;
        let file = "pool/all/f/fixture-beta_1.0-1_amd64.deb";
        let index = format!("Package: fixture-beta\nVersion: 1.0-1\nArchitecture: amd64\nFilename: {file}\n");
        for (p, body) in [
            ("dists/default/Release", "Suite: default\n"),
            ("dists/default/all/binary-amd64/Packages", index.as_str()),
            (file, "deb bytes"),
        ] {
            let served = wiremock::ResponseTemplate::new(200).set_body_string(body);
            at("GET", &format!("/pulp/content/apt/{p}")).respond_with(served).mount(&server).await;
        }
        let it = PlanItem {
            t: RepoType::Deb,
            name: "apt".into(),
            base_path: "apt".into(),
            remote_url: "https://up/pulp/content/apt/".into(),
            manifest: None,
            distributions: Some("default".into()),
            mode: json!({ "simple": true, "structured": false }),
            action: PlanAction::Create,
            note: String::new(),
        };
        let done = replicate_one(&c, "up", &it, true, &ops::no_progress).await.unwrap();
        assert_eq!(done, format!("version 1 served at {base} (Release and {file} served with matching digest (1 packages))"));
        let sent = server.received_requests().await.unwrap();
        let posts = |p: &str| -> Vec<Value> {
            sent.iter().filter(|r| r.method.as_str() == "POST" && r.url.path() == p).map(|r| r.body_json().unwrap()).collect()
        };
        let labels = json!({ LABEL_REPLICA: "up" });
        let remote_body = json!({ "name": "apt", "url": "https://up/pulp/content/apt/", "distributions": "default",
            "policy": "immediate", "pulp_labels": labels });
        assert_eq!(posts(remotes), [remote_body]);
        assert_eq!(posts(&format!("{repo}sync/")), [json!({ "remote": remote, "mirror": true })]);
        assert_eq!(posts(pubs), [json!({ "repository_version": v1, "simple": true, "structured": false })]);
        assert_eq!(posts(dists), [json!({ "name": "apt", "base_path": "apt", "publication": publication, "pulp_labels": labels })]);
    }

    /// An existing Deb replica remote follows the upstream: a PATCH only when its URL or dists differ.
    #[tokio::test]
    async fn a_deb_replica_remote_is_patched_only_when_the_served_dists_change() {
        use wiremock::matchers::{method, path};
        for (had, patched) in [("fixture", true), ("default", false)] {
            let server = wiremock::MockServer::start().await;
            let c = client(&server, "dst");
            let one = |o: Value| wiremock::ResponseTemplate::new(200).set_body_json(json!({ "count": 1, "next": null, "results": [o] }));
            let (remote, url) = ("/pulp/api/v3/remotes/deb/apt/1/", "https://up/pulp/content/apt/");
            let r = json!({ "pulp_href": remote, "name": "apt", "url": url, "distributions": had, "pulp_labels": { LABEL_REPLICA: "up" } });
            wiremock::Mock::given(method("GET")).and(path("/pulp/api/v3/remotes/deb/apt/")).respond_with(one(r)).mount(&server).await;
            let done = wiremock::ResponseTemplate::new(200).set_body_json(json!({ "pulp_href": remote }));
            wiremock::Mock::given(method("PATCH")).and(path(remote)).respond_with(done).mount(&server).await;
            serve_made_by_me(&server, &[remote]).await;
            // A repository of the same name that is not this replication's: the run stops there.
            let r = json!({ "pulp_href": "/pulp/api/v3/repositories/deb/apt/1/", "name": "apt", "pulp_labels": {} });
            wiremock::Mock::given(method("GET")).and(path("/pulp/api/v3/repositories/deb/apt/")).respond_with(one(r)).mount(&server).await;
            let it = PlanItem {
                t: RepoType::Deb,
                name: "apt".into(),
                base_path: "apt".into(),
                remote_url: url.into(),
                manifest: None,
                distributions: Some("default".into()),
                mode: json!({}),
                action: PlanAction::Update,
                note: String::new(),
            };
            let e = replicate_one(&c, "up", &it, false, &ops::no_progress).await.unwrap_err();
            assert_eq!(e.to_string(), "a local repository named apt exists and is not managed by this replication");
            let sent = server.received_requests().await.unwrap();
            let patches: Vec<Value> = sent.iter().filter(|r| r.method.as_str() == "PATCH").map(|r| r.body_json().unwrap()).collect();
            let want = if patched { vec![json!({ "url": url, "distributions": "default" })] } else { vec![] };
            assert_eq!(patches, want, "the remote had {had}");
        }
    }

    fn client(s: &wiremock::MockServer, name: &str) -> PulpClient {
        let p = crate::config::Profile { url: s.uri(), auth: crate::config::AuthMethod::None, ..Default::default() };
        PulpClient::new(name, &p, &crate::config::Settings::default()).unwrap()
    }

    /// Only this Pulp user (`op`, from `login/`) holds a role on the objects at `hrefs`: replicas that it made
    /// (`ops::refuse_foreign`).
    async fn serve_made_by_me(s: &wiremock::MockServer, hrefs: &[&str]) {
        let get = |p: String| wiremock::Mock::given(wiremock::matchers::method("GET")).and(wiremock::matchers::path(p));
        let ok = |b: Value| wiremock::ResponseTemplate::new(200).set_body_json(b);
        get("/pulp/api/v3/login/".into()).respond_with(ok(json!({ "username": "op" }))).mount(s).await;
        for h in hrefs {
            get(format!("{h}list_roles/")).respond_with(ok(json!({ "roles": [{ "role": "owner", "users": ["op"] }] }))).mount(s).await;
        }
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

    /// The sync has the policy of the type and mode; a skipped one stops the run.
    #[tokio::test]
    async fn a_skipped_sync_is_not_published_or_reported_as_done() {
        use wiremock::matchers::{method, path};
        let cases =
            [(RepoType::File, false, json!({ "mirror": false })), (RepoType::Rpm, true, json!({ "sync_policy": "mirror_content_only" }))];
        for (t, mirror, mut want) in cases {
            let server = wiremock::MockServer::start().await;
            let c = client(&server, "dst");
            let ok = |b: Value| wiremock::ResponseTemplate::new(200).set_body_json(b);
            let (remotes, repos) = (format!("/pulp/api/v3/{}", t.remotes()), format!("/pulp/api/v3/{}", t.repositories()));
            let (remote, repo) = (format!("{remotes}1/"), format!("{repos}1/"));
            let one = |o: Value| json!({ "count": 1, "next": null, "results": [o] });
            let labels = json!({ LABEL_REPLICA: "up" });
            let r = json!({ "pulp_href": remote, "name": "files", "url": "https://up/files/PULP_MANIFEST", "pulp_labels": labels.clone() });
            wiremock::Mock::given(method("GET")).and(path(remotes)).respond_with(ok(one(r))).mount(&server).await;
            // The repository has the version of an earlier run.
            let v1 = format!("{repo}versions/1/");
            let r = json!({ "pulp_href": repo, "name": "files", "latest_version_href": v1, "manifest": "PULP_MANIFEST", "pulp_labels": labels });
            wiremock::Mock::given(method("GET")).and(path(repos)).respond_with(ok(one(r.clone()))).mount(&server).await;
            wiremock::Mock::given(method("GET")).and(path(repo.as_str())).respond_with(ok(r)).mount(&server).await;
            serve_made_by_me(&server, &[&remote, &repo]).await;
            wiremock::Mock::given(method("POST"))
                .and(path(format!("{repo}sync/")))
                .respond_with(wiremock::ResponseTemplate::new(202).set_body_json(json!({ "task": "/pulp/api/v3/tasks/1/" })))
                .mount(&server)
                .await;
            let skipped = json!({ "pulp_href": "/pulp/api/v3/tasks/1/", "state": "skipped", "name": "sync" });
            wiremock::Mock::given(method("GET")).and(path("/pulp/api/v3/tasks/1/")).respond_with(ok(skipped)).mount(&server).await;
            let it = PlanItem {
                t,
                name: "files".into(),
                base_path: "files".into(),
                remote_url: "https://up/files/PULP_MANIFEST".into(),
                manifest: (t == RepoType::File).then(|| "PULP_MANIFEST".into()),
                distributions: None,
                mode: json!({}),
                action: PlanAction::Update,
                note: String::new(),
            };
            let e = replicate_one(&c, "up", &it, mirror, &ops::no_progress).await.unwrap_err();
            assert_eq!(e.to_string(), "task skipped: ");
            let sent = server.received_requests().await.unwrap();
            let changes: Vec<_> = sent.iter().filter(|r| r.method.as_str() != "GET").collect();
            want["remote"] = remote.into();
            assert_eq!(changes.len(), 1, "{t:?}: no publication, no distribution");
            assert_eq!((changes[0].url.path(), changes[0].body_json::<Value>().unwrap()), (format!("{repo}sync/").as_str(), want));
        }
    }

    #[tokio::test]
    async fn a_file_replica_syncs_from_the_manifest_the_upstream_serves() {
        // F99: without CONTENT_ORIGIN the upstream `base_url` is a path on the server of the source profile.
        for relative in [false, true] {
            a_file_replica_syncs_from_the_served_manifest(relative).await;
        }
    }

    async fn a_file_replica_syncs_from_the_served_manifest(relative: bool) {
        use wiremock::matchers::{method, path, query_param};
        let (up, down) = (wiremock::MockServer::start().await, wiremock::MockServer::start().await);
        serve_status(&up, &["file"]).await;
        serve_status(&down, &["file"]).await;
        let page = |r: Value| json!({ "count": r.as_array().map_or(0, Vec::len), "next": null, "results": r });
        let ok = |b: Value| wiremock::ResponseTemplate::new(200).set_body_json(b);
        let repo = "/pulp/api/v3/repositories/file/file/1/";
        let (base, served) = match relative {
            false => ("https://up/pulp/content/files/".to_string(), "https://up/pulp/content/files/CUSTOM".to_string()),
            true => ("/pulp/content/files/".into(), format!("{}/pulp/content/files/CUSTOM", up.uri())),
        };
        wiremock::Mock::given(method("GET"))
            .and(path("/pulp/api/v3/distributions/file/file/"))
            .respond_with(ok(page(json!([{ "pulp_href": "/pulp/api/v3/distributions/file/file/1/", "name": "files",
                "base_path": "files", "base_url": base, "repository": repo }]))))
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
        assert_eq!((plan[0].remote_url.as_str(), &plan[0].action), (served.as_str(), &PlanAction::Create));
        assert_eq!(plan[0].manifest.as_deref(), Some("CUSTOM"), "the replica publishes under that name too");
    }

    /// A checkpoint distribution serves its newest checkpoint publication at `<base>/<timestamp>/` only (its bare
    /// base path lists the timestamps): the replica remote syncs from there, for every type, and the note says so.
    #[tokio::test]
    async fn a_checkpoint_distribution_is_replicated_from_its_newest_checkpoint() {
        use wiremock::matchers::{method, path, query_param};
        let (up, down) = (wiremock::MockServer::start().await, wiremock::MockServer::start().await);
        serve_status(&up, &["rpm", "file"]).await;
        serve_status(&down, &["rpm", "file"]).await;
        let page = |r: Value| json!({ "count": r.as_array().map_or(0, Vec::len), "next": null, "results": r });
        let ok = |b: Value| wiremock::ResponseTemplate::new(200).set_body_json(b);
        let dist = |t: &str, name: &str, repo: &str| {
            json!({ "pulp_href": format!("/pulp/api/v3/distributions/{t}/{t}/{name}/"), "name": name, "base_path": name,
                "base_url": format!("https://up/pulp/content/{name}/"), "repository": repo, "checkpoint": true })
        };
        let (os, empty, files) =
            ("/pulp/api/v3/repositories/rpm/rpm/1/", "/pulp/api/v3/repositories/rpm/rpm/2/", "/pulp/api/v3/repositories/file/file/1/");
        let rpm_dists = json!([dist("rpm", "empty", empty), dist("rpm", "os", os)]);
        wiremock::Mock::given(method("GET"))
            .and(path("/pulp/api/v3/distributions/rpm/rpm/"))
            .respond_with(ok(page(rpm_dists)))
            .mount(&up)
            .await;
        let file_dists = json!([dist("file", "files", files)]);
        wiremock::Mock::given(method("GET"))
            .and(path("/pulp/api/v3/distributions/file/file/"))
            .respond_with(ok(page(file_dists)))
            .mount(&up)
            .await;
        let publication = |repo: &str, n: u32, created: &str, checkpoint: bool| {
            json!({ "pulp_href": format!("{repo}p/{n}/"), "repository_version": format!("{repo}versions/{n}/"), "pulp_created": created,
                "checkpoint": checkpoint, "manifest": "PULP_MANIFEST" })
        };
        // The newest checkpoint publication counts, not a newer one that is no checkpoint.
        let pubs = [
            (
                "rpm",
                os,
                json!([
                    publication(os, 1, "2026-01-01T00:00:00Z", true),
                    publication(os, 2, "2026-02-03T04:05:06.123456Z", true),
                    publication(os, 3, "2026-03-01T00:00:00Z", false)
                ]),
            ),
            ("rpm", empty, json!([publication(empty, 1, "2026-01-01T00:00:00Z", false)])),
            ("file", files, json!([publication(files, 1, "2026-03-01T10:20:30Z", true)])),
        ];
        for (t, repo, p) in pubs {
            wiremock::Mock::given(method("GET"))
                .and(path(format!("/pulp/api/v3/publications/{t}/{t}/")))
                .and(query_param("repository", repo))
                .respond_with(ok(page(p)))
                .mount(&up)
                .await;
        }
        wiremock::Mock::given(method("GET")).respond_with(ok(page(json!([])))).mount(&down).await;
        let plan = plan(&client(&up, "up"), &client(&down, "down"), &[RepoType::Rpm, RepoType::File], "", None).await.unwrap();
        let rows: Vec<_> = plan.iter().map(|i| (i.name.as_str(), &i.action, i.remote_url.as_str(), i.note.as_str())).collect();
        let follows = |ts: &str| {
            format!(
                "new remote + repository + distribution; upstream is a checkpoint distribution: the replica follows the newest \
                 checkpoint at the time of the run ({ts})"
            )
        };
        assert_eq!(
            rows,
            [
                ("empty", &PlanAction::Skip, "", "upstream distribution serves nothing (no publication)"),
                ("os", &PlanAction::Create, "https://up/pulp/content/os/20260203T040506Z/", follows("20260203T040506Z").as_str()),
                (
                    "files",
                    &PlanAction::Create,
                    "https://up/pulp/content/files/20260301T102030Z/PULP_MANIFEST",
                    follows("20260301T102030Z").as_str()
                ),
            ]
        );
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

    /// The dry run says why the run stops for a replica, with the checks of the run (F57): a local repository or remote
    /// of the name without the replica label, a distribution of the name without it, one with it that another Pulp user
    /// made (it holds the owner role) or with a field that the replication does not set, and a new distribution whose
    /// base path overlaps another one. The replica of this source that this Pulp user (`op`) made is updated.
    #[tokio::test]
    async fn the_dry_run_names_what_stops_the_run_for_a_replica() {
        use wiremock::Mock;
        use wiremock::matchers::{method, path, query_param};
        let (up, down) = (wiremock::MockServer::start().await, wiremock::MockServer::start().await);
        serve_status(&up, &["rpm"]).await;
        serve_status(&down, &["rpm"]).await;
        let page = |r: Value| json!({ "count": r.as_array().map_or(0, Vec::len), "next": null, "results": r });
        let ok = |b: Value| wiremock::ResponseTemplate::new(200).set_body_json(b);
        let dist = |name: &str| {
            json!({ "pulp_href": format!("/pulp/api/v3/distributions/rpm/rpm/{name}/"), "name": name, "base_path": name,
                "base_url": format!("https://up/pulp/content/{name}/"), "publication": "/pulp/api/v3/publications/rpm/rpm/1/" })
        };
        let names = ["dsquat", "mine", "over", "planted", "remote", "repo", "squat", "taken"];
        let dists = Value::Array(names.map(dist).to_vec());
        Mock::given(method("GET")).and(path("/pulp/api/v3/distributions/rpm/rpm/")).respond_with(ok(page(dists))).mount(&up).await;
        let ours = json!({ LABEL_REPLICA: "up" });
        let (new, by_remote) = (json!({}), |name: &str| json!({ "remote": format!("/pulp/api/v3/remotes/rpm/rpm/{name}/") }));
        let (mine, planted) = (by_remote("mine"), by_remote("planted"));
        let no_tls = json!({ "tls_validation": false });
        // (collection, name, labels, the user that holds its owner role, its other fields): what the destination has of that
        // name. A repository made by the replication has its remote as the default remote.
        let local = [
            ("repositories", "mine", ours.clone(), "op", &mine),
            ("remotes", "mine", ours.clone(), "op", &new),
            ("distributions", "mine", ours.clone(), "op", &new),
            ("repositories", "planted", ours.clone(), "op", &planted),
            ("remotes", "planted", ours.clone(), "op", &no_tls),
            ("remotes", "remote", json!({}), "op", &new),
            ("repositories", "repo", json!({}), "op", &new),
            ("repositories", "squat", ours.clone(), "eve", &new),
            ("remotes", "squat", ours.clone(), "eve", &new),
            ("distributions", "squat", ours.clone(), "eve", &new),
            ("distributions", "dsquat", ours, "eve", &new),
            ("distributions", "taken", json!({ "team": "x" }), "op", &new),
        ];
        let get = |p: String| Mock::given(method("GET")).and(path(p));
        for (coll, name, labels, holder, fields) in local {
            let href = format!("/pulp/api/v3/{coll}/rpm/rpm/{name}/");
            let mut o = json!({ "pulp_href": href, "name": name, "base_path": name, "pulp_labels": labels });
            o.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
            let m = get(format!("/pulp/api/v3/{coll}/rpm/rpm/")).and(query_param("name", name));
            m.respond_with(ok(page(json!([o])))).with_priority(1).mount(&down).await;
            let roles = json!({ "roles": [{ "role": format!("{coll}_owner"), "users": [holder], "groups": [] }] });
            get(format!("{href}list_roles/")).respond_with(ok(roles)).with_priority(1).mount(&down).await;
        }
        get("/pulp/api/v3/login/".into()).respond_with(ok(json!({ "username": "op" }))).with_priority(1).mount(&down).await;
        let below = json!([{ "pulp_href": "/pulp/api/v3/distributions/file/file/9/", "name": "local", "base_path": "over/x" }]);
        let m = Mock::given(method("GET")).and(path("/pulp/api/v3/distributions/")).and(query_param("base_path__contains", "over/"));
        m.respond_with(ok(page(below))).with_priority(1).mount(&down).await;
        Mock::given(method("GET")).respond_with(ok(page(json!([])))).mount(&down).await;

        let plan = plan(&client(&up, "up"), &client(&down, "down"), &[RepoType::Rpm], "", None).await.unwrap();
        let rows: Vec<_> = plan.iter().map(|i| (i.name.as_str(), &i.action, i.note.as_str())).collect();
        let foreign =
            |kind: &str, name: &str| format!("conflict: a local {kind} named {name} exists and is not managed by this replication");
        let taken = r#"conflict: distribution taken exists but lacks ownership labels {"pulp_tui_replica":"up"}: restore them or delete the distribution, then try again"#;
        let squat = |what: &str, coll: &str| {
            format!(
                "conflict: {what} exists, but this database does not record it and user eve has role {coll}_owner on it: \
                 delete it, or remove that role, then try again"
            )
        };
        let no_tls_note = "conflict: remote planted exists, but this database does not record it and its field differs from a \
                           new one (tls_validation is false, not true): delete it, or set it back, then try again";
        assert_eq!(
            rows,
            [
                ("dsquat", &PlanAction::Skip, squat("distribution dsquat", "distributions").as_str()),
                ("mine", &PlanAction::Update, "re-sync existing replica"),
                ("over", &PlanAction::Skip, "conflict: base path over overlaps base path over/x of distribution local"),
                ("planted", &PlanAction::Skip, no_tls_note),
                ("remote", &PlanAction::Skip, foreign("remote", "remote").as_str()),
                ("repo", &PlanAction::Skip, foreign("repository", "repo").as_str()),
                ("squat", &PlanAction::Skip, squat("repository squat", "repositories").as_str()),
                ("taken", &PlanAction::Skip, taken),
            ]
        );
        let sent = down.received_requests().await.unwrap();
        assert!(sent.iter().all(|r| r.method.as_str() == "GET"), "a dry run only reads: {sent:?}");
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
        serve_made_by_me(&server, &[remote, repo]).await;
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
            distributions: None,
            mode: json!({}),
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
