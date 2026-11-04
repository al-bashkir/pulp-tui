//! Replication between two live Pulp instances: pulp2 (upstream, :8081) -> pulp1 (:8080).
//! Requires `tests/env/pulp-env.sh up pulp2` in addition to pulp1.

mod common;

use common::*;
use pulp_tui::pulp::models::{Distribution, TaskGroup};
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{Capabilities, Feature, Mutation, PulpClient, RepoType};
use pulp_tui::replication::{self, PlanAction};
use pulp_tui::repo_export;
use pulp_tui::store::Store;
use serde_json::{Value, json};

fn upstream_client() -> PulpClient {
    client_for(&std::env::var("PULP_TUI_TEST_UPSTREAM_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into()), "pulp2")
}

/// URL of pulp2 as seen from the pulp1 container.
fn upstream_from_downstream() -> String {
    std::env::var("PULP_TUI_TEST_UPSTREAM_INTERNAL").unwrap_or_else(|_| "http://host.containers.internal:8081".into())
}

/// Upstream RPM repo (pinned publication) + File repo (follows latest via autopublish).
async fn upstream_content(up: &PulpClient, prefix: &str) -> (String, String) {
    let (rpm_repo, _) = synced_repo(up, RepoType::Rpm, &format!("{prefix}-os"), "rpm/", "additive").await;
    let v = latest(up, &rpm_repo).await;
    let p = ops::ensure_publication(up, RepoType::Rpm, &v, &json!({}), &no_progress).await.unwrap();
    ops::ensure_distribution(up, RepoType::Rpm, &format!("{prefix}-os"), &format!("{prefix}/os"), Some(&p), &json!({}), None, &no_progress)
        .await
        .unwrap();
    let (file_repo, _) = synced_repo(up, RepoType::File, &format!("{prefix}-docs"), "file/PULP_MANIFEST", "mirror").await;
    let v = latest(up, &file_repo).await;
    let p = ops::ensure_publication(up, RepoType::File, &v, &json!({}), &no_progress).await.unwrap();
    ops::ensure_distribution(
        up,
        RepoType::File,
        &format!("{prefix}-docs"),
        &format!("{prefix}/docs"),
        Some(&p),
        &json!({}),
        None,
        &no_progress,
    )
    .await
    .unwrap();
    (rpm_repo, file_repo)
}

/// Move the upstream RPM repo to the newer fixture repo and re-publish its distribution.
async fn update_upstream_rpm(up: &PulpClient, prefix: &str, rpm_repo: &str) {
    let r: Value = up.get(rpm_repo, &[]).await.unwrap();
    let remote = r["remote"].as_str().unwrap().to_string();
    ops::settle(up, up.patch(&remote, &json!({ "url": format!("{}/rpm2/", upstream()) })).await.unwrap(), &no_progress).await.unwrap();
    ops::settle(up, ops::sync(up, RepoType::Rpm, rpm_repo, None, "additive").await.unwrap(), &no_progress).await.unwrap();
    let v = latest(up, rpm_repo).await;
    let p = ops::ensure_publication(up, RepoType::Rpm, &v, &json!({}), &no_progress).await.unwrap();
    ops::ensure_distribution(up, RepoType::Rpm, &format!("{prefix}-os"), &format!("{prefix}/os"), Some(&p), &json!({}), None, &no_progress)
        .await
        .unwrap();
}

async fn downstream_has(c: &PulpClient, dist_name: &str, nevra: &str) -> bool {
    let d: Distribution = ops::find_by_name(c, &c.api(RepoType::Rpm.distributions()), dist_name).await.unwrap().unwrap();
    let version = match (&d.repository_version, &d.publication) {
        (Some(v), _) => v.clone(),
        (None, Some(p)) => c.get::<Value>(p, &[]).await.unwrap()["repository_version"].as_str().unwrap().to_string(),
        // Older pulpcore (3.95) replicates with a distribution that follows the repository.
        _ if d.repository.is_some() => latest(c, d.repository.as_deref().unwrap()).await,
        _ => panic!("distribution {dist_name} serves nothing"),
    };
    ops::verify_served(c, RepoType::Rpm, d.base_url.as_deref().unwrap(), &version, &Value::Null).await.unwrap();
    ops::content_keys(c, RepoType::Rpm, Some(&version)).await.unwrap().contains(nevra)
}

/// An Upstream Pulp on pulp1 for the pulp2 distributions named `prefix*`, policy `nodelete` where the server has it.
async fn upstream_pulp(dst: &PulpClient, caps: &Capabilities, prefix: &str) -> String {
    let mut body = json!({
        "name": prefix, "base_url": upstream_from_downstream(), "api_root": "pulp/",
        "username": "admin", "password": std::env::var("PULP_TUI_TEST_PASSWORD").unwrap_or_else(|_| "password".into()),
        "q_select": format!("name__startswith=\"{prefix}\""),
    });
    if caps.supports(Feature::ReplicationPolicy) {
        body["policy"] = "nodelete".into();
    }
    ops::create(dst, &dst.api("upstream-pulps/"), &body, "/upstream-pulps/", &no_progress).await.unwrap()
}

/// A canceled replication ends (its Task Center row becomes final) and is never reported as completed.
#[tokio::test]
async fn canceled_native_replication_is_not_complete() {
    if !live_enabled() {
        return;
    }
    let dst = client();
    let caps = Capabilities::detect(&dst).await.unwrap();
    if !caps.supports(Feature::NativeReplication) {
        eprintln!("native replication unsupported by this server; skipping");
        return;
    }
    let upstream_href = upstream_pulp(&dst, &caps, &unique("natc")).await;
    let Mutation::TaskGroup(g) = dst.post(&format!("{upstream_href}replicate/"), &json!({})).await.unwrap() else {
        panic!("replicate did not return a task group");
    };
    let started: TaskGroup = dst.get(&g, &[]).await.unwrap();
    let canceled = dst.patch(started.tasks[0]["pulp_href"].as_str().unwrap(), &json!({ "state": "canceled" })).await;
    let row = ops::wait_task_group(&dst, &g, &|_| {}).await.unwrap().as_task();
    match canceled {
        Ok(_) => assert!(row.state != "completed" && row.error.is_some(), "{row:?}"),
        Err(e) => eprintln!("replicate_distributions ended before the cancel ({e}): {row:?}"),
    }
    cleanup(&dst, &[&upstream_href]).await;
}

#[tokio::test]
async fn native_replication() {
    if !live_enabled() {
        return;
    }
    let (dst, up) = (client(), upstream_client());
    let caps = Capabilities::detect(&dst).await.unwrap();
    if !caps.supports(Feature::NativeReplication) {
        eprintln!("native replication unsupported by this server; skipping");
        return;
    }
    let prefix = unique("nat");
    let (rpm_repo, _) = upstream_content(&up, &prefix).await;
    let upstream_href = upstream_pulp(&dst, &caps, &prefix).await;

    let last = std::sync::Mutex::new(None);
    let lines =
        replication::replicate_native(&dst, &upstream_href, &|g: &TaskGroup| *last.lock().unwrap() = Some(g.as_task())).await.unwrap();
    assert!(lines[0].contains("Replication of"), "{lines:?}");
    // The Task Center row ends completed, also when a restart re-attaches it (a poll with get_task).
    let row = last.into_inner().unwrap().unwrap();
    assert_eq!((row.state.as_str(), &row.error), ("completed", &None), "{lines:?}");
    assert_eq!(ops::get_task(&dst, &row.pulp_href).await.unwrap(), row);
    assert!(downstream_has(&dst, &format!("{prefix}-os"), "fixture-alpha-1.0-1.noarch").await);
    let docs: Distribution =
        ops::find_by_name(&dst, &dst.api(RepoType::File.distributions()), &format!("{prefix}-docs")).await.unwrap().unwrap();
    let (status, body) = served(&dst, &format!("{}docs/readme.txt", docs.base_url.unwrap())).await;
    assert_eq!((status, body.as_slice()), (200, &b"readme v1\n"[..]));

    // Idempotent rerun, then upstream update is picked up.
    replication::replicate_native(&dst, &upstream_href, &|_| {}).await.unwrap();
    update_upstream_rpm(&up, &prefix, &rpm_repo).await;
    replication::replicate_native(&dst, &upstream_href, &|_| {}).await.unwrap();
    assert!(downstream_has(&dst, &format!("{prefix}-os"), "fixture-alpha-1.1-1.noarch").await);

    // The fallback must not touch objects owned by native replication.
    let plan = replication::plan(&up, &dst, &RepoType::ALL, &prefix, None).await.unwrap();
    assert_eq!(plan.len(), 2);
    assert!(plan.iter().all(|p| p.action == PlanAction::Skip && p.note.contains("conflict")), "{plan:?}");
    cleanup(&dst, &[&upstream_href]).await;
}

#[tokio::test]
async fn fallback_replication() {
    if !live_enabled() {
        return;
    }
    let (dst, up) = (client(), upstream_client());
    assert!(replication::guard_self("http://localhost:8080", "http://localhost:8080").is_err());
    let prefix = unique("fb");
    let (rpm_repo, file_repo) = upstream_content(&up, &prefix).await;
    let store = Store::open_in_memory().await.unwrap();

    // Dry run, then execute.
    let plan = replication::plan(&up, &dst, &RepoType::ALL, &prefix, Some("http://127.0.0.1:8080/pulp/content/")).await.unwrap();
    assert_eq!(plan.iter().map(|p| p.action.clone()).collect::<Vec<_>>(), vec![PlanAction::Create, PlanAction::Create], "{plan:?}");
    assert!(plan.iter().find(|p| p.t == RepoType::File).unwrap().remote_url.ends_with("/PULP_MANIFEST"));
    let res = replication::run(&dst, &store, up.profile(), &plan, false, &|_| {}, &no_progress).await.unwrap();
    assert!(res.iter().all(|l| l.starts_with("completed")), "{res:?}");
    assert!(downstream_has(&dst, &format!("{prefix}-os"), "fixture-beta-2.0-1.noarch").await);

    // Rerun is idempotent (Update), and picks up upstream changes: new packages, and a new manifest name
    // that the replica publishes under too, although its File content is the same.
    update_upstream_rpm(&up, &prefix, &rpm_repo).await;
    let m = up.patch(&file_repo, &json!({ "manifest": "CUSTOM_MANIFEST" })).await.unwrap();
    ops::settle(&up, m, &no_progress).await.unwrap();
    let p = ops::create_publication(&up, RepoType::File, &latest(&up, &file_repo).await, &json!({}), &no_progress).await.unwrap();
    let (docs, docs_path) = (format!("{prefix}-docs"), format!("{prefix}/docs"));
    ops::ensure_distribution(&up, RepoType::File, &docs, &docs_path, Some(&p), &json!({}), None, &no_progress).await.unwrap();
    let plan = replication::plan(&up, &dst, &RepoType::ALL, &prefix, None).await.unwrap();
    assert!(plan.iter().all(|p| p.action == PlanAction::Update), "{plan:?}");
    let res = replication::run(&dst, &store, up.profile(), &plan, false, &|_| {}, &no_progress).await.unwrap();
    assert!(res.iter().all(|l| l.starts_with("completed")), "{res:?}");
    assert!(downstream_has(&dst, &format!("{prefix}-os"), "fixture-alpha-1.1-1.noarch").await);
    let replica = |f: &str| format!("http://127.0.0.1:8080/pulp/content/{docs_path}/{f}");
    assert_eq!(served(&dst, &replica("CUSTOM_MANIFEST")).await.0, 200);
    assert_eq!(served(&dst, &replica("PULP_MANIFEST")).await.0, 404, "the publication of the first run is not reused");
    assert_eq!(store.last_replications(dst.profile(), up.profile()).await.unwrap().len(), 2);

    // Loop guard: distributions served by the destination itself are skipped.
    let plan = replication::plan(&dst, &dst, &[RepoType::Rpm], &prefix, Some("http://127.0.0.1:8080/pulp/content/")).await.unwrap();
    assert!(plan.iter().all(|p| p.action == PlanAction::Skip && p.note.contains("loop")), "{plan:?}");
}

/// Deb fallback replication: a structured and a simple publication of one synced repository on pulp2. Each replica
/// on pulp1 syncs the dists that its upstream serves, publishes in the same mode and serves the packages; a rerun
/// re-syncs.
#[tokio::test]
async fn deb_fallback_replication() {
    if !live_enabled() {
        return;
    }
    let (dst, up) = (client(), upstream_client());
    let t = RepoType::Deb;
    let prefix = unique("fbdeb");
    let body = json!({ "name": prefix, "url": format!("{}/deb/", upstream()), "policy": "immediate", "distributions": "fixture" });
    let remote = create(&up, t.remotes(), body, "/remotes/").await;
    let repo = create(&up, t.repositories(), json!({ "name": prefix, "remote": remote }), "/repositories/").await;
    ops::settle(&up, ops::sync(&up, t, &repo, None, "mirror").await.unwrap(), &no_progress).await.unwrap();
    let v = latest(&up, &repo).await;
    let (apt, simple) = (format!("{prefix}-apt"), format!("{prefix}-simple"));
    for (k, mode) in [("apt", json!({})), ("simple", json!({ "simple": true, "structured": false }))] {
        let p = ops::create_publication(&up, t, &v, &mode, &no_progress).await.unwrap();
        let (name, base_path) = (format!("{prefix}-{k}"), format!("{prefix}/{k}"));
        ops::ensure_distribution(&up, t, &name, &base_path, Some(&p), &json!({}), None, &no_progress).await.unwrap();
    }
    let store = Store::open_in_memory().await.unwrap();

    let plan = replication::plan(&up, &dst, &[t], &prefix, Some("http://127.0.0.1:8080/pulp/content/")).await.unwrap();
    let rows: Vec<_> = plan.iter().map(|p| (p.name.clone(), p.action.clone(), p.distributions.clone(), p.mode["simple"].clone())).collect();
    assert_eq!(
        rows,
        [
            (apt.clone(), PlanAction::Create, Some("fixture".to_string()), json!(false)),
            (simple.clone(), PlanAction::Create, Some("default".to_string()), json!(true)),
        ],
        "{plan:?}"
    );
    let res = replication::run(&dst, &store, up.profile(), &plan, true, &|_| {}, &no_progress).await.unwrap();
    assert!(res.iter().all(|l| l.starts_with("completed") && l.contains("matching digest")), "{res:?}");
    // Each replica serves the paths of its mode only.
    for (name, served_path, absent) in [
        (&apt, "dists/fixture/Release", "dists/default/all/binary-amd64/Packages"),
        (&simple, "dists/default/Release", "dists/fixture/Release"),
    ] {
        let d: Distribution = ops::find_by_name(&dst, &dst.api(t.distributions()), name).await.unwrap().unwrap();
        let base = d.base_url.unwrap();
        assert_eq!(served(&dst, &format!("{base}{served_path}")).await.0, 200, "{name}: {served_path}");
        assert_eq!(served(&dst, &format!("{base}{absent}")).await.0, 404, "{name}: {absent}");
    }

    // A rerun re-syncs the replicas and reuses their publications.
    let plan = replication::plan(&up, &dst, &[t], &prefix, None).await.unwrap();
    assert!(plan.iter().all(|p| p.action == PlanAction::Update), "{plan:?}");
    let res = replication::run(&dst, &store, up.profile(), &plan, true, &|_| {}, &no_progress).await.unwrap();
    assert!(res.iter().all(|l| l.starts_with("completed")), "{res:?}");
}

/// `export-repos` on pulp2 and `import-repos` on pulp1: a repository gets the remote of the same name on pulp1,
/// no secret is in the file, and a rerun keeps everything.
#[tokio::test]
async fn repo_definitions_move_between_servers() {
    if !live_enabled() {
        return;
    }
    let (up, down) = (upstream_client(), client());
    let p = unique("rx");
    let remote = json!({ "name": format!("{p}-rpm"), "url": "https://mirror.example/os/", "policy": "on_demand",
        "username": "u", "password": "pw", "sles_auth_token": "tok", "headers": [{ "X-Token": "t" }] });
    let rpm_remote = create(&up, RepoType::Rpm.remotes(), remote, "/remotes/").await;
    let repo = json!({ "name": format!("{p}-rpm"), "description": "d", "retain_repo_versions": 3, "remote": rpm_remote });
    create(&up, RepoType::Rpm.repositories(), repo, "/repositories/").await;
    let remote = json!({ "name": format!("{p}-deb"), "url": "http://deb.example/debian/", "distributions": "bookworm" });
    let deb_remote = create(&up, RepoType::Deb.remotes(), remote, "/remotes/").await;
    create(&up, RepoType::Deb.repositories(), json!({ "name": format!("{p}-deb"), "remote": deb_remote }), "/repositories/").await;
    create(&up, RepoType::File.repositories(), json!({ "name": format!("{p}-file") }), "/repositories/").await;

    let (mut ex, _) = repo_export::export(&up, &Capabilities::detect(&up).await.unwrap()).await.unwrap();
    ex.remotes.retain(|e| e.name.starts_with(&p));
    ex.repositories.retain(|e| e.name.starts_with(&p));
    assert_eq!((ex.remotes.len(), ex.repositories.len()), (2, 3));
    let file = serde_json::to_string(&ex).unwrap();
    for secret in ["\"pw\"", "tok", "X-Token"] {
        assert!(!file.contains(secret), "{secret} in {file}");
    }
    let secrets = &ex.remotes.iter().find(|e| e.name == format!("{p}-rpm")).unwrap().secrets;
    assert!(secrets.contains(&"username".into()) && secrets.contains(&"password".into()), "{secrets:?}");

    let caps = Capabilities::detect(&down).await.unwrap();
    let (created, kept, notes) = repo_export::import(&down, &caps, &ex).await.unwrap();
    assert_eq!((created, kept), (5, 0), "{notes:?}");
    for t in [RepoType::Rpm, RepoType::Deb] {
        let name = format!("{p}-{}", t.component());
        let repo: Value = ops::find_by_name(&down, &down.api(t.repositories()), &name).await.unwrap().unwrap();
        let remote: Value = ops::find_by_name(&down, &down.api(t.remotes()), &name).await.unwrap().unwrap();
        assert_eq!(repo["remote"], remote["pulp_href"], "{name}: the repository has the remote of this server");
    }
    let repo: Value = ops::find_by_name(&down, &down.api(RepoType::Rpm.repositories()), &format!("{p}-rpm")).await.unwrap().unwrap();
    assert_eq!((repo["description"].as_str(), repo["retain_repo_versions"].as_u64()), (Some("d"), Some(3)));

    let (created, kept, notes) = repo_export::import(&down, &caps, &ex).await.unwrap();
    assert_eq!((created, kept), (0, 5));
    assert!(notes.is_empty(), "a rerun finds nothing different: {notes:?}");
}
