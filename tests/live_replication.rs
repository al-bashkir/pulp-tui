//! Replication between two live Pulp instances: pulp2 (upstream, :8081) -> pulp1 (:8080).
//! Requires `tests/env/pulp-env.sh up pulp2` in addition to pulp1.

mod common;

use common::*;
use pulp_tui::config::Profile;
use pulp_tui::pulp::models::{Distribution, TaskGroup};
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{Capabilities, Feature, PulpClient, RepoType};
use pulp_tui::replication::{self, PlanAction};
use pulp_tui::repo_export;
use pulp_tui::store::Store;
use pulp_tui::tui::actions::run;
use pulp_tui::tui::app::{App, Modal, Msg};
use pulp_tui::tui::kinds::{Act, Kind};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

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

/// A canceled replication, started from the TUI, ends (its Task Center row becomes final) and is never reported
/// as completed: the result view says CANCELED.
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
    let name = unique("natc");
    let upstream_href = upstream_pulp(&dst, &caps, &name).await;
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::UpstreamPulps;
    run(&mut app, Act::Replicate, dst.get(&upstream_href, &[]).await.unwrap());
    let Some(Modal::Confirm { on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm: {:?}", texts(&app)) };
    yes(&mut app);
    // The job reports the group to the Task Center at its first poll.
    while app.tasks.items.is_empty() {
        app.handle_msg(rx.recv().await.unwrap());
    }
    let started: TaskGroup = dst.get(&app.tasks.items[0].href, &[]).await.unwrap();
    let canceled = dst.patch(started.tasks[0]["pulp_href"].as_str().unwrap(), &json!({ "state": "canceled" })).await;
    settle(&mut app, &mut rx).await;
    let Some(Modal::Text { title, lines, .. }) = app.modal.take() else { panic!("no result: {:?}", texts(&app)) };
    let row = &app.tasks.items[0];
    match canceled {
        Ok(_) => {
            assert_eq!(title, format!("Replication of {name} CANCELED"), "{lines:?}");
            assert!(row.state == "canceled" && row.is_final(), "{row:?}");
        }
        Err(e) => eprintln!("replicate_distributions ended before the cancel ({e}): {title} {lines:?}"),
    }
    // History (H) finds the run by the task that reserves this Upstream Pulp.
    let group = row.href.clone();
    run(&mut app, Act::ReplicationHistory, json!({ "pulp_href": upstream_href, "name": name }));
    settle(&mut app, &mut rx).await;
    let Some(Modal::Text { lines, .. }) = &app.modal else { panic!("no history: {:?}", texts(&app)) };
    assert!(lines[0].starts_with(&format!("{group}  ")), "{lines:?}");
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

/// F57: a local distribution outside the replication is above the replica base paths (Pulp refuses an overlapping base
/// path). The dry run names it as a conflict (SKIP), with the refusal of the run, and the run creates nothing. Once the
/// path is free, the dry run says CREATE.
#[tokio::test]
async fn the_fallback_plan_names_a_base_path_that_overlaps_a_local_distribution() {
    if !live_enabled() {
        return;
    }
    let (dst, up) = (client(), upstream_client());
    let prefix = unique("fbov");
    upstream_content(&up, &prefix).await;
    let store = Store::open_in_memory().await.unwrap();
    let foreign = format!("{prefix}-local");
    let local = create(&dst, RepoType::File.distributions(), json!({ "name": foreign, "base_path": prefix }), "/distributions/").await;

    let plan = replication::plan(&up, &dst, &RepoType::ALL, &prefix, None).await.unwrap();
    let notes: Vec<_> = plan.iter().map(|p| (p.action.clone(), p.note.clone())).collect();
    let conflict = |path: &str| format!("conflict: base path {path} overlaps base path {prefix} of distribution {foreign}");
    assert_eq!(
        notes,
        [(PlanAction::Skip, conflict(&format!("{prefix}/os"))), (PlanAction::Skip, conflict(&format!("{prefix}/docs")))],
        "nothing is created before the overlap is named"
    );
    let res = replication::run(&dst, &store, up.profile(), &plan, false, &|_| {}, &no_progress).await.unwrap();
    assert!(res.is_empty(), "{res:?}");
    for t in [RepoType::Rpm, RepoType::File] {
        assert!(ops::find_by_name::<Value>(&dst, &dst.api(t.remotes()), &format!("{prefix}-os")).await.unwrap().is_none());
        assert!(ops::find_by_name::<Value>(&dst, &dst.api(t.remotes()), &format!("{prefix}-docs")).await.unwrap().is_none());
    }

    cleanup(&dst, &[&local]).await;
    let plan = replication::plan(&up, &dst, &RepoType::ALL, &prefix, None).await.unwrap();
    assert!(plan.iter().all(|p| p.action == PlanAction::Create), "{plan:?}");
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

/// A checkpoint File distribution on pulp2 serves its newest checkpoint publication at `<base>/<timestamp>/` only. The
/// replica on pulp1 syncs from there, says so in the dry run, and serves the files.
#[tokio::test]
async fn checkpoint_fallback_replication() {
    if !live_enabled() {
        return;
    }
    let (dst, up) = (client(), upstream_client());
    let t = RepoType::File;
    if !Capabilities::detect(&up).await.unwrap().has_field(t.distribution_schema(), "checkpoint") {
        eprintln!("skipping: the upstream distributions have no `checkpoint` field (older pulpcore)");
        return;
    }
    let prefix = unique("fbcp");
    let (repo, _) = synced_repo(&up, t, &prefix, "file/PULP_MANIFEST", "mirror").await;
    // Pulp makes a checkpoint publication only with `repository` (of its latest version).
    create(&up, t.publications(), json!({ "repository": repo, "checkpoint": true }), "/publications/").await;
    let body = json!({ "name": prefix, "base_path": prefix, "repository": repo, "checkpoint": true });
    create(&up, t.distributions(), body, "/distributions/").await;

    let plan = replication::plan(&up, &dst, &[t], &prefix, None).await.unwrap();
    assert_eq!(plan.len(), 1, "{plan:?}");
    let url = &plan[0].remote_url;
    let at = url.trim_end_matches("/PULP_MANIFEST").rsplit('/').next().unwrap();
    assert!(url.ends_with("Z/PULP_MANIFEST") && url.contains(&format!("/{prefix}/{at}/")), "{url}");
    let note = format!("the replica follows the newest checkpoint at the time of the run ({at})");
    assert!(plan[0].note.ends_with(&note), "{}", plan[0].note);
    let store = Store::open_in_memory().await.unwrap();
    let res = replication::run(&dst, &store, up.profile(), &plan, false, &|_| {}, &no_progress).await.unwrap();
    assert!(res.iter().all(|l| l.starts_with("completed")), "{res:?}");
    let d: Distribution = ops::find_by_name(&dst, &dst.api(t.distributions()), &prefix).await.unwrap().unwrap();
    assert_eq!(served(&dst, &format!("{}docs/readme.txt", d.base_url.unwrap())).await.0, 200);
}

/// A flat Deb repository (Release and Packages at its root; a File distribution on pulp2 serves it), synced on pulp2
/// with distributions "/" and served by a verbatim publication. The replica on pulp1 syncs "/" from that publication,
/// publishes it verbatim and serves the flat Release and the packages.
#[tokio::test]
async fn deb_flat_verbatim_fallback_replication() {
    use sha2::Digest;
    if !live_enabled() {
        return;
    }
    let (dst, up) = (client(), upstream_client());
    let prefix = unique("fbflat");
    let (deb, dir) = (fixtures().join("upstream/deb"), tempfile::tempdir().unwrap());
    let packages = std::fs::read(deb.join("dists/fixture/main/binary-amd64/Packages")).unwrap();
    // Unique per run, so the File repository gets a Release file of its own.
    let release = format!(
        "Origin: pulp-tui\nLabel: pulp-tui\nSuite: flat\nCodename: flat\nArchitectures: amd64\nDescription: {prefix}\nSHA256:\n {} {} Packages\n",
        hex::encode(sha2::Sha256::digest(&packages)),
        packages.len()
    );
    std::fs::write(dir.path().join("Release"), release.as_bytes()).unwrap();
    let files = RepoType::File;
    let repo = create(&up, files.repositories(), json!({ "name": format!("{prefix}-files") }), "/repositories/").await;
    let mut uploads = vec![(dir.path().join("Release"), "Release".to_string())];
    uploads.push((deb.join("dists/fixture/main/binary-amd64/Packages"), "Packages".into()));
    for p in String::from_utf8(packages).unwrap().lines().filter_map(|l| l.strip_prefix("Filename: ")) {
        uploads.push((deb.join(p), p.to_string()));
    }
    for (file, path) in &uploads {
        ops::upload(&up, files, file, Some(path), Some(&repo), true, &|_, _| {}, &no_progress).await.unwrap();
    }
    let p = ops::ensure_publication(&up, files, &latest(&up, &repo).await, &json!({}), &no_progress).await.unwrap();
    let (name, base_path) = (format!("{prefix}-files"), format!("{prefix}/files"));
    let d = ops::ensure_distribution(&up, files, &name, &base_path, Some(&p), &json!({}), None, &no_progress).await.unwrap();
    let flat_url = up.get::<Distribution>(&d, &[]).await.unwrap().base_url.unwrap();

    let t = RepoType::Deb;
    let body = json!({ "name": prefix, "url": flat_url, "policy": "immediate", "distributions": "/" });
    let remote = create(&up, t.remotes(), body, "/remotes/").await;
    let deb_repo = create(&up, t.repositories(), json!({ "name": prefix, "remote": remote }), "/repositories/").await;
    ops::settle(&up, ops::sync(&up, t, &deb_repo, None, "mirror").await.unwrap(), &no_progress).await.unwrap();
    let verbatim = json!({ "verbatim": true });
    let p = ops::create_publication(&up, t, &latest(&up, &deb_repo).await, &verbatim, &no_progress).await.unwrap();
    let flat = format!("{prefix}-flat");
    ops::ensure_distribution(&up, t, &flat, &format!("{prefix}/flat"), Some(&p), &json!({}), None, &no_progress).await.unwrap();

    let plan = replication::plan(&up, &dst, &[t], &prefix, None).await.unwrap();
    let rows: Vec<_> = plan.iter().map(|p| (p.name.clone(), p.action.clone(), p.distributions.clone(), p.mode.clone())).collect();
    assert_eq!(rows, [(flat.clone(), PlanAction::Create, Some("/".to_string()), verbatim)], "{plan:?}");
    let store = Store::open_in_memory().await.unwrap();
    let res = replication::run(&dst, &store, up.profile(), &plan, true, &|_| {}, &no_progress).await.unwrap();
    assert!(res.iter().all(|l| l.starts_with("completed") && l.contains("matching digest (4 packages)")), "{res:?}");
    let d: Distribution = ops::find_by_name(&dst, &dst.api(t.distributions()), &flat).await.unwrap().unwrap();
    assert_eq!(served(&dst, &format!("{}Release", d.base_url.unwrap())).await, (200, release.into_bytes()), "the flat Release as it is");
}

/// `export-repos` on pulp2 and `import-repos` on pulp1: a repository gets the remote of the same name on pulp1,
/// no secret is in the file, and a rerun keeps everything. ULN and Git remotes go too; a ULN remote is created only
/// after the operator adds its username and password to the file.
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
    let uln = json!({ "name": format!("{p}-uln"), "url": "uln://ol8_x86_64_baseos_latest", "username": "u", "password": "uln-pw" });
    create(&up, "remotes/rpm/uln/", uln, "/remotes/").await;
    let up_caps = Capabilities::detect(&up).await.unwrap();
    // pulp_file has Git remotes only on newer servers (not on 3.63 or 3.95).
    let git = usize::from(up_caps.schemas.contains_key("file.FileGitRemote"));
    if git == 1 {
        let body = json!({ "name": format!("{p}-git"), "url": "https://git.example/docs.git", "git_ref": "main" });
        create(&up, "remotes/file/git/", body, "/remotes/").await;
    }

    let (mut ex, _) = repo_export::export(&up, &up_caps).await.unwrap();
    ex.remotes.retain(|e| e.name.starts_with(&p));
    ex.alt_remotes.retain(|e| e.name.starts_with(&p));
    ex.repositories.retain(|e| e.name.starts_with(&p));
    assert_eq!((ex.remotes.len(), ex.alt_remotes.len(), ex.repositories.len()), (2, 1 + git, 3));
    let file = serde_json::to_string(&ex).unwrap();
    for secret in ["\"pw\"", "tok", "X-Token", "uln-pw"] {
        assert!(!file.contains(secret), "{secret} in {file}");
    }
    let secrets = &ex.remotes.iter().find(|e| e.name == format!("{p}-rpm")).unwrap().secrets;
    assert!(secrets.contains(&"username".into()) && secrets.contains(&"password".into()), "{secrets:?}");

    let caps = Capabilities::detect(&down).await.unwrap();
    let (created, kept, notes) = repo_export::import(&down, &caps, &ex).await.unwrap();
    assert_eq!((created, kept), (5 + git, 0), "{notes:?}");
    let not_created = format!(
        "RPM ULN remote {p}-uln: not created: it needs password, username (not in the export); add them to its fields in the file, \
         or create it by hand"
    );
    assert!(notes.contains(&not_created), "{notes:?}");
    if git == 1 {
        let r: Value = ops::find_by_name(&down, &down.api("remotes/file/git/"), &format!("{p}-git")).await.unwrap().unwrap();
        assert_eq!(r["git_ref"], "main");
    }
    // The operator adds the ULN credentials to the file: the next import creates that remote too.
    let uln = ex.alt_remotes.iter_mut().find(|e| e.repo_type == RepoType::Rpm).unwrap();
    uln.fields.extend([("username".to_string(), json!("u")), ("password".to_string(), json!("uln-pw"))]);
    let (created, kept, notes) = repo_export::import(&down, &caps, &ex).await.unwrap();
    assert_eq!((created, kept, notes), (1, 5 + git, vec![]));
    for t in [RepoType::Rpm, RepoType::Deb] {
        let name = format!("{p}-{}", t.component());
        let repo: Value = ops::find_by_name(&down, &down.api(t.repositories()), &name).await.unwrap().unwrap();
        let remote: Value = ops::find_by_name(&down, &down.api(t.remotes()), &name).await.unwrap().unwrap();
        assert_eq!(repo["remote"], remote["pulp_href"], "{name}: the repository has the remote of this server");
    }
    let repo: Value = ops::find_by_name(&down, &down.api(RepoType::Rpm.repositories()), &format!("{p}-rpm")).await.unwrap().unwrap();
    assert_eq!((repo["description"].as_str(), repo["retain_repo_versions"].as_u64()), (Some("d"), Some(3)));

    let (created, kept, notes) = repo_export::import(&down, &caps, &ex).await.unwrap();
    assert_eq!((created, kept), (0, 6 + git));
    assert!(notes.is_empty(), "a rerun finds nothing different: {notes:?}");
}

/// Profile `pulp2` (the upstream) next to `pulp1` in the TUI `app`, as the Servers screen adds it.
fn add_upstream_profile(app: &mut App) {
    let dir = tempfile::tempdir().unwrap().keep();
    std::fs::write(dir.join("pw"), std::env::var("PULP_TUI_TEST_PASSWORD").unwrap_or_else(|_| "password".into())).unwrap();
    let url = std::env::var("PULP_TUI_TEST_UPSTREAM_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into());
    let secret = Some(format!("file:{}", dir.join("pw").display()));
    app.cfg.profiles.insert("pulp2".into(), Profile { url, username: Some("admin".into()), secret, ..Default::default() });
}

/// Best-effort removal of the distribution, repository and remote of type `t` named `name` on `c`: the objects that a
/// replication makes for an upstream distribution, or the upstream objects that the tests make with one name.
async fn cleanup_named(c: &PulpClient, t: RepoType, name: &str) {
    for collection in [t.distributions(), t.repositories(), t.remotes()] {
        if let Ok(Some(v)) = ops::find_by_name::<Value>(c, &c.api(collection), name).await {
            cleanup(c, &[v["pulp_href"].as_str().unwrap_or_default()]).await;
        }
    }
}

/// Fallback replication (F on Upstream Pulps) through the TUI from the profile pulp2, of `types` (the form's choice), of
/// the upstream distributions whose name contains `prefix`: the dry run has one CREATE line for each of `want`; Yes
/// replicates them; the result view has one completed line for each.
async fn tui_fallback(app: &mut App, rx: &mut UnboundedReceiver<Msg>, types: &str, prefix: &str, want: &[&str]) {
    app.list.kind = Kind::UpstreamPulps;
    run(app, Act::FallbackReplicate, Value::Null);
    fill(app, &[("source", "pulp2"), ("types", types), ("filter", prefix)]);
    settle(app, rx).await; // the dry run
    let (lines, typed) = yes(app);
    assert_eq!((typed, lines[0].as_str()), (None, "Dry run: pulp2 -> pulp1 (additive)"));
    let creates: Vec<&String> = lines.iter().filter(|l| l.starts_with("CREATE ")).collect();
    let planned = |n: &&str| creates.iter().any(|l| l.contains(&format!(" {n} -> /")));
    assert!(creates.len() == want.len() && want.iter().all(planned), "{lines:?}");
    settle(app, rx).await;
    shown(app, "fallback replication completed");
    let Some(Modal::Text { title, lines, .. }) = app.modal.take() else { panic!("no result: {:?}", texts(app)) };
    assert_eq!(title, "Fallback replication result");
    assert!(lines.len() == want.len() && lines.iter().all(|l| l.starts_with("completed")), "{lines:?}");
}

/// Fallback replication through the TUI of an RPM and a File distribution of pulp2: each replica on pulp1 serves the
/// upstream content.
#[tokio::test]
async fn fallback_replication_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (dst, up) = (client(), upstream_client());
    let prefix = unique("fbtui");
    upstream_content(&up, &prefix).await;
    let (os, docs) = (format!("{prefix}-os"), format!("{prefix}-docs"));
    let (mut app, mut rx) = app().await;
    add_upstream_profile(&mut app);
    tui_fallback(&mut app, &mut rx, "rpm+file", &prefix, &[&os, &docs]).await;
    assert!(downstream_has(&dst, &os, "fixture-beta-2.0-1.noarch").await);
    let d: Distribution = ops::find_by_name(&dst, &dst.api(RepoType::File.distributions()), &docs).await.unwrap().unwrap();
    assert_eq!(served(&dst, &format!("{}docs/readme.txt", d.base_url.unwrap())).await, (200, b"readme v1\n".to_vec()));
    for (t, name) in [(RepoType::Rpm, &os), (RepoType::File, &docs)] {
        cleanup_named(&dst, t, name).await;
        cleanup_named(&up, t, name).await;
    }
}

/// Fallback replication through the TUI of a Deb distribution of pulp2 (types: deb): the replica on pulp1 serves the
/// release of the upstream publication.
#[tokio::test]
async fn deb_fallback_replication_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (dst, up, t) = (client(), upstream_client(), RepoType::Deb);
    let prefix = unique("fbdebtui");
    let (repo, _) = synced_repo(&up, t, &prefix, "deb/", "mirror").await;
    let p = ops::create_publication(&up, t, &latest(&up, &repo).await, &json!({}), &no_progress).await.unwrap();
    let apt = format!("{prefix}-apt");
    ops::ensure_distribution(&up, t, &apt, &format!("{prefix}/apt"), Some(&p), &json!({}), None, &no_progress).await.unwrap();
    let (mut app, mut rx) = app().await;
    add_upstream_profile(&mut app);
    tui_fallback(&mut app, &mut rx, "deb", &prefix, &[&apt]).await;
    let d: Distribution = ops::find_by_name(&dst, &dst.api(t.distributions()), &apt).await.unwrap().unwrap();
    assert_eq!(served(&dst, &format!("{}dists/fixture/Release", d.base_url.unwrap())).await.0, 200);
    cleanup_named(&dst, t, &apt).await;
    cleanup_named(&up, t, &apt).await;
    cleanup_named(&up, t, &prefix).await;
}

/// Replicate (R on Upstream Pulps) through the TUI: the confirmation names the filter (policy nodelete: no typed name);
/// the Task Center row of the task group ends completed; the result view; pulp1 serves the upstream RPM and File
/// content; History (H) lists the run.
#[tokio::test]
async fn native_replication_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (dst, up) = (client(), upstream_client());
    let caps = Capabilities::detect(&dst).await.unwrap();
    if !caps.supports(Feature::NativeReplication) {
        eprintln!("native replication unsupported by this server; skipping");
        return;
    }
    let prefix = unique("nattui");
    upstream_content(&up, &prefix).await;
    let upstream_href = upstream_pulp(&dst, &caps, &prefix).await;
    let (os, docs) = (format!("{prefix}-os"), format!("{prefix}-docs"));
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::UpstreamPulps;
    let item: Value = dst.get(&upstream_href, &[]).await.unwrap();
    run(&mut app, Act::Replicate, item.clone());
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed.is_none(), caps.supports(Feature::ReplicationPolicy), "nodelete asks no typed name: {lines:?}");
    assert!(lines.contains(&format!("Distribution filter: name__startswith=\"{prefix}\"")), "{lines:?}");
    settle(&mut app, &mut rx).await;
    let Some(Modal::Text { title, lines, .. }) = app.modal.take() else { panic!("no result: {:?}", texts(&app)) };
    assert_eq!(title, format!("Replication of {prefix}"), "{lines:?}");
    let group = app.tasks.items[0].clone();
    assert!(group.state == "completed" && group.is_final(), "{group:?}");
    assert!(downstream_has(&dst, &os, "fixture-alpha-1.0-1.noarch").await);
    let d: Distribution = ops::find_by_name(&dst, &dst.api(RepoType::File.distributions()), &docs).await.unwrap().unwrap();
    assert_eq!(served(&dst, &format!("{}docs/readme.txt", d.base_url.unwrap())).await.0, 200);

    run(&mut app, Act::ReplicationHistory, item);
    settle(&mut app, &mut rx).await;
    let Some(Modal::Text { lines, .. }) = app.modal.take() else { panic!("no history: {:?}", texts(&app)) };
    assert!(lines[0].starts_with(&format!("{}  completed ", group.href)), "{lines:?}");
    cleanup(&dst, &[&upstream_href]).await;
    for (t, name) in [(RepoType::Rpm, &os), (RepoType::File, &docs)] {
        cleanup_named(&dst, t, name).await;
        cleanup_named(&up, t, name).await;
    }
}

/// Audit F47 (T04): Pulp accepts user info in an Upstream Pulp `base_url`. Through the TUI the list, Details, the
/// Replicate confirmation, the FAILED result (a closed port), the Task Center row, the notices and the Logs show
/// `http://•••@host/`, never the password.
#[tokio::test]
async fn upstream_base_url_user_info_never_shows_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let dst = client();
    let caps = Capabilities::detect(&dst).await.unwrap();
    if !caps.supports(Feature::NativeReplication) {
        eprintln!("native replication unsupported by this server; skipping");
        return;
    }
    // The earlier tests of this binary fill the log; at its cap our task poll rows would be evicted first.
    pulp_tui::session_log::clear();
    let (name, masked) = (unique("t04-userinfo"), "http://•••@127.0.0.1:9/");
    let mut body = json!({ "name": name, "base_url": "http://t04user:t04secret@127.0.0.1:9/", "api_root": "pulp/",
        "q_select": format!("name__startswith=\"{name}\"") });
    // Without a policy (pulpcore 3.63) a run may delete local objects: no replicate run there.
    let policy = caps.supports(Feature::ReplicationPolicy);
    if policy {
        body["policy"] = "nodelete".into();
    }
    let href = ops::create(&dst, &dst.api("upstream-pulps/"), &body, "/upstream-pulps/", &no_progress).await.unwrap();
    let (mut app, mut rx) = app().await;
    let screen = |app: &App| {
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(200, 50)).unwrap();
        t.draw(|f| pulp_tui::tui::view::draw(f, app)).unwrap();
        t.backend().to_string()
    };
    open_list(&mut app, &mut rx, Kind::UpstreamPulps, &name).await;
    // Pulp's error of the failed task names the URL (pulpcore 3.123: "[PLP0014] External service '<url>' failed").
    let mut error_names_url = false;
    let mut seen = vec![screen(&app)];
    assert!(seen[0].matches(masked).count() >= 2, "the base_url column and Details: {}", seen[0]);
    if policy {
        let item = row(&app, "name", &name).unwrap();
        run(&mut app, Act::Replicate, item);
        seen.push(screen(&app));
        let (lines, _) = yes(&mut app);
        assert!(lines.iter().any(|l| l.contains(masked)), "{lines:?}");
        settle(&mut app, &mut rx).await;
        seen.push(screen(&app));
        let Some(Modal::Text { title, lines, .. }) = &app.modal else { panic!("no result: {:?}", texts(&app)) };
        assert_eq!(title, &format!("Replication of {name} FAILED"), "{lines:?}");
        error_names_url = lines.iter().any(|l| l.contains(masked));
        seen.push(lines.join("\n"));
        assert!(app.tasks.items[0].state == "failed", "{:?}", app.tasks.items[0]);
        seen.push(format!("{:?}", app.tasks.items[0]));
    } else {
        eprintln!("no deletion policy on this server: no replicate run");
    }
    seen.extend(texts(&app));
    let logs: Vec<String> = pulp_tui::session_log::rows(true)
        .iter()
        .map(|r| format!("{r} {:?}", pulp_tui::session_log::detail(r["id"].as_u64().unwrap())))
        .collect();
    assert!(logs.iter().any(|l| l.contains("upstream-pulps/") && l.contains(masked)), "the Logs body of the list");
    assert!(!error_names_url || logs.iter().any(|l| l.contains("/tasks/") && l.contains(masked)), "the Logs body of a task poll");
    seen.extend(logs);
    let leaks: Vec<&str> = seen.iter().flat_map(|s| s.lines()).filter(|l| l.contains("t04secret") || l.contains("t04user")).collect();
    assert!(leaks.is_empty(), "{leaks:#?}");
    cleanup(&dst, &[&href]).await;
}
