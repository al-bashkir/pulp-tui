//! Replication between two live Pulp instances: pulp2 (upstream, :8081) -> pulp1 (:8080).
//! Requires `tests/env/pulp-env.sh up pulp2` in addition to pulp1.

mod common;

use common::*;
use pulp_tui::pulp::models::Distribution;
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{Capabilities, Feature, PulpClient, RepoType};
use pulp_tui::replication::{self, PlanAction};
use pulp_tui::repo_export;
use pulp_tui::store::Store;
use serde_json::{Value, json};

fn upstream_client() -> PulpClient {
    client_for(&std::env::var("PULP_TUI_TEST_UPSTREAM_URL").unwrap_or_else(|_| "http://localhost:8081".into()), "pulp2")
}

/// URL of pulp2 as seen from the pulp1 container.
fn upstream_from_downstream() -> String {
    std::env::var("PULP_TUI_TEST_UPSTREAM_INTERNAL").unwrap_or_else(|_| "http://host.containers.internal:8081".into())
}

/// Upstream RPM repo (pinned publication) + File repo (follows latest via autopublish).
async fn upstream_content(up: &PulpClient, prefix: &str) -> (String, String) {
    let (rpm_repo, _) = synced_repo(up, RepoType::Rpm, &format!("{prefix}-os"), "rpm/", "additive").await;
    let v = latest(up, &rpm_repo).await;
    let p = ops::ensure_publication(up, RepoType::Rpm, &v, None, &no_progress).await.unwrap();
    ops::ensure_distribution(up, RepoType::Rpm, &format!("{prefix}-os"), &format!("{prefix}/os"), Some(&p), &json!({}), None, &no_progress)
        .await
        .unwrap();
    let (file_repo, _) = synced_repo(up, RepoType::File, &format!("{prefix}-docs"), "file/PULP_MANIFEST", "mirror").await;
    let v = latest(up, &file_repo).await;
    let p = ops::ensure_publication(up, RepoType::File, &v, None, &no_progress).await.unwrap();
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
    let p = ops::ensure_publication(up, RepoType::Rpm, &v, None, &no_progress).await.unwrap();
    ops::ensure_distribution(up, RepoType::Rpm, &format!("{prefix}-os"), &format!("{prefix}/os"), Some(&p), &json!({}), None, &no_progress)
        .await
        .unwrap();
}

async fn downstream_has(c: &PulpClient, dist_name: &str, nevra: &str) -> bool {
    let d: Distribution = ops::find_by_name(c, &c.api(RepoType::Rpm.distributions()), dist_name).await.unwrap().unwrap();
    let version = match (&d.repository_version, &d.publication) {
        (Some(v), _) => v.clone(),
        (None, Some(p)) => c.get::<Value>(p, &[]).await.unwrap()["repository_version"].as_str().unwrap().to_string(),
        _ => panic!("distribution {dist_name} serves nothing"),
    };
    ops::verify_served(c, RepoType::Rpm, d.base_url.as_deref().unwrap(), &version, "").await.unwrap();
    ops::content_keys(c, RepoType::Rpm, Some(&version)).await.unwrap().contains(nevra)
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
    let mut body = json!({
        "name": prefix, "base_url": upstream_from_downstream(), "api_root": "pulp/",
        "username": "admin", "password": std::env::var("PULP_TUI_TEST_PASSWORD").unwrap_or_else(|_| "password".into()),
        "q_select": format!("name__startswith=\"{prefix}\""),
    });
    if caps.supports(Feature::ReplicationPolicy) {
        body["policy"] = "nodelete".into();
    }
    let upstream_href = ops::create(&dst, &dst.api("upstream-pulps/"), &body, "/upstream-pulps/", &no_progress).await.unwrap();

    let lines = replication::replicate_native(&dst, &upstream_href, &|_| {}).await.unwrap();
    assert!(lines[0].contains("Replication of"), "{lines:?}");
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
    let (rpm_repo, _) = upstream_content(&up, &prefix).await;
    let store = Store::open_in_memory().await.unwrap();

    // Dry run, then execute.
    let plan = replication::plan(&up, &dst, &RepoType::ALL, &prefix, Some("http://localhost:8080/pulp/content/")).await.unwrap();
    assert_eq!(plan.iter().map(|p| p.action.clone()).collect::<Vec<_>>(), vec![PlanAction::Create, PlanAction::Create], "{plan:?}");
    assert!(plan.iter().find(|p| p.t == RepoType::File).unwrap().remote_url.ends_with("/PULP_MANIFEST"));
    let res = replication::run(&dst, &store, up.profile(), &plan, false, &|_| {}, &no_progress).await.unwrap();
    assert!(res.iter().all(|l| l.starts_with("completed")), "{res:?}");
    assert!(downstream_has(&dst, &format!("{prefix}-os"), "fixture-beta-2.0-1.noarch").await);

    // Rerun is idempotent (Update), and picks up upstream changes.
    update_upstream_rpm(&up, &prefix, &rpm_repo).await;
    let plan = replication::plan(&up, &dst, &RepoType::ALL, &prefix, None).await.unwrap();
    assert!(plan.iter().all(|p| p.action == PlanAction::Update), "{plan:?}");
    let res = replication::run(&dst, &store, up.profile(), &plan, false, &|_| {}, &no_progress).await.unwrap();
    assert!(res.iter().all(|l| l.starts_with("completed")), "{res:?}");
    assert!(downstream_has(&dst, &format!("{prefix}-os"), "fixture-alpha-1.1-1.noarch").await);
    assert_eq!(store.last_replications(dst.profile(), up.profile()).await.unwrap().len(), 2);

    // Loop guard: distributions served by the destination itself are skipped.
    let plan = replication::plan(&dst, &dst, &[RepoType::Rpm], &prefix, Some("http://localhost:8080/pulp/content/")).await.unwrap();
    assert!(plan.iter().all(|p| p.action == PlanAction::Skip && p.note.contains("loop")), "{plan:?}");
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
