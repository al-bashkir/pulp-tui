//! Content View workflows against a live Pulp: multi-repository snapshots with filters,
//! numbered versions, promotion through environments, rollback, served-content
//! verification, retention protection, partial failure + recovery, and resuming a
//! publish whose Pulp task was submitted before the client stopped, objects owned by
//! another database, and a composite view of two component views.

mod common;

use common::*;
use pulp_tui::cv::{CvService, Mode, base_path, export, import, unix_now};
use pulp_tui::filters::parse_rules;
use pulp_tui::pulp::client::Page;
use pulp_tui::pulp::models::{RepoVersion, Task};
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{Capabilities, Mutation, PulpClient, RepoType};
use pulp_tui::store::{CvRepo, Export, Store};
use pulp_tui::tui::actions::run;
use pulp_tui::tui::app::{App, Modal, Msg};
use pulp_tui::tui::kinds::{Act, Kind, s};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedReceiver;

fn quiet(_: String) {}

#[tokio::test]
async fn content_view_lifecycle() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let id = unique("cvt");
    let (rpm_repo, rpm_remote) = synced_repo(&c, RepoType::Rpm, &format!("{id}-os"), "rpm/", "additive").await;
    let (file_repo, file_remote) = synced_repo(&c, RepoType::File, &format!("{id}-docs"), "file/PULP_MANIFEST", "mirror").await;

    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let profile = c.profile().to_string();
    let svc = CvService { client: c.clone(), store: store.clone(), scope: profile.clone(), actor: "test@live".into(), lock: None };
    let cv = store.create_cv(&profile, &id, "test view").await.unwrap();
    // Every workflow below runs under the view's lock (renewed every 30 s), as the TUI does.
    let svc = svc.locked(cv, "live test").await.unwrap();
    let mut os = CvRepo {
        repo_href: rpm_repo.clone(),
        repo_name: "os".into(),
        repo_type: RepoType::Rpm,
        filters: parse_rules("exclude name=*-debuginfo", RepoType::Rpm).unwrap(),
    };
    store.set_cv_repo(cv, &os).await.unwrap();
    let docs = CvRepo {
        repo_href: file_repo.clone(),
        repo_name: "docs".into(),
        repo_type: RepoType::File,
        filters: parse_rules("exclude path=isos/*", RepoType::File).unwrap(),
    };
    store.set_cv_repo(cv, &docs).await.unwrap();

    // Preview shows the exact selection.
    let preview = svc.preview(cv, None).await.unwrap().join("\n");
    assert!(preview.contains("+ fixture-alpha-1.0-1.noarch"), "{preview}");
    assert!(!preview.contains("+ fixture-beta-debuginfo"), "{preview}");
    assert!(preview.contains("+ docs/readme.txt"), "{preview}");
    assert!(!preview.contains("boot.iso"), "{preview}");

    // v1: publish auto-binds Library and is verified against served content.
    let (v1, _) = svc.publish(cv, "first", "", &quiet, &no_progress).await.unwrap();
    assert_eq!((v1.number, v1.state.as_str()), (1, "published"));
    let lib = store.ensure_library(&profile).await.unwrap();
    assert_eq!(svc.verify(cv, lib).await.unwrap(), Vec::<String>::new());
    let vrs = store.version_repos(v1.id).await.unwrap();
    let os_v1 = vrs.iter().find(|v| v.repo().repo_name == "os").unwrap();
    let rv: RepoVersion = c.get(os_v1.derived_version.as_deref().unwrap(), &[]).await.unwrap();
    assert_eq!(rv.content_summary.present["rpm.package"].count, 2);
    assert_eq!(rv.content_summary.present["rpm.advisory"].count, 1, "advisory kept: its package is selected");
    assert_eq!(vrs.iter().find(|v| v.repo().repo_name == "docs").unwrap().content_count, Some(2));

    // Environments: Library -> Dev -> Prod. Promotion must follow the path.
    let dev = store.create_env(&profile, "Dev", lib, false).await.unwrap();
    let prod = store.create_env(&profile, "Prod", dev, true).await.unwrap();
    let err = svc.promote(cv, v1.id, prod, Mode::Promote, &quiet, &no_progress).await.unwrap_err();
    assert!(err.to_string().contains("not active in Dev"), "{err}");
    svc.promote(cv, v1.id, dev, Mode::Promote, &quiet, &no_progress).await.unwrap();
    assert_eq!(svc.verify(cv, dev).await.unwrap(), Vec::<String>::new());
    let dev_docs = c
        .fetch_content(&format!("http://127.0.0.1:8080/pulp/content/{}/docs/readme.txt", base_path(&id, "dev", "docs")), 1 << 20)
        .await
        .unwrap();
    assert_eq!(dev_docs.0, 200);

    // Upstream update -> v2 (Library follows), promote to Dev, compare.
    let m = c.patch(&rpm_remote, &json!({ "url": format!("{}/rpm2/", upstream()) })).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let m = ops::sync(&c, RepoType::Rpm, &rpm_repo, None, "additive").await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let (v2, _) = svc.publish(cv, "upstream update", "", &quiet, &no_progress).await.unwrap();
    assert_eq!(v2.number, 2);
    assert_eq!(store.binding(lib, cv).await.unwrap().unwrap().active(), Some(v2.id));
    svc.promote(cv, v2.id, dev, Mode::Promote, &quiet, &no_progress).await.unwrap();
    assert!(svc.verify(cv, dev).await.unwrap().is_empty());
    let cmp = svc.compare_versions(v1.id, v2.id).await.unwrap().join("\n");
    assert!(cmp.contains("+ fixture-alpha-1.1-1.noarch") && cmp.contains("+ fixture-gamma-0.5-1.noarch"), "{cmp}");

    // Rollback Dev to v1: bindings change, history stays, served content is v1 again.
    svc.promote(cv, v1.id, dev, Mode::Rollback, &quiet, &no_progress).await.unwrap();
    assert_eq!(store.binding(dev, cv).await.unwrap().unwrap().active(), Some(v1.id));
    assert!(svc.verify(cv, dev).await.unwrap().is_empty());
    assert_eq!(store.version(v2.id).await.unwrap().state, "published");
    // Rollback goes back only: v2 again, or v1 into empty Prod, would skip the path (CV-5).
    let refused = async |v: i64, env| svc.promote(cv, v, env, Mode::Rollback, &quiet, &no_progress).await.unwrap_err().to_string();
    assert_eq!(refused(v2.id, dev).await, "v2 is newer than the active v1 in Dev; use Promote");
    assert_eq!(refused(v1.id, prod).await, "no version is active in Prod; use Promote");
    assert_eq!(store.binding(dev, cv).await.unwrap().unwrap().active(), Some(v1.id));

    // Retention protection: referenced snapshots are listed; bound versions cannot be deleted.
    let protected = store.protected_versions(&profile).await.unwrap();
    assert!(protected.contains(os_v1.derived_version.as_ref().unwrap()));
    assert!(svc.delete_version(v1.id, &quiet, &no_progress).await.unwrap_err().to_string().contains("bound to Dev"));

    // Advisory filtering: excluding the package referenced by the advisory drops it.
    os.filters = parse_rules("exclude name=*-debuginfo; exclude name=fixture-beta", RepoType::Rpm).unwrap();
    store.set_cv_repo(cv, &os).await.unwrap();
    let (v3, _) = svc.publish(cv, "no beta", "", &quiet, &no_progress).await.unwrap();
    let os_v3 = store.version_repos(v3.id).await.unwrap().into_iter().find(|v| v.repo().repo_name == "os").unwrap();
    let rv: RepoVersion = c.get(os_v3.derived_version.as_deref().unwrap(), &[]).await.unwrap();
    assert!(!rv.content_summary.present.contains_key("rpm.advisory"));
    assert_eq!(rv.content_summary.present["rpm.package"].count, 3);

    // Partial failure: a foreign distribution occupies the File base path of QA.
    let qa = store.create_env(&profile, "QA", lib, false).await.unwrap();
    let blocker = create(
        &c,
        RepoType::File.distributions(),
        json!({ "name": format!("{id}-blocker"), "base_path": base_path(&id, "qa", "docs") }),
        "/distributions/",
    )
    .await;
    let err = svc.promote(cv, v3.id, qa, Mode::Promote, &quiet, &no_progress).await.unwrap_err();
    assert!(err.to_string().contains("partial"), "{err}");
    assert_eq!(store.binding(qa, cv).await.unwrap().unwrap().attempt.unwrap().state, "partial");
    assert!(!svc.verify(cv, qa).await.unwrap().is_empty());
    cleanup(&c, &[&blocker]).await;
    svc.promote(cv, v3.id, qa, Mode::Promote, &quiet, &no_progress).await.unwrap(); // idempotent retry
    assert_eq!(store.binding(qa, cv).await.unwrap().unwrap().attempt, None);
    assert!(svc.verify(cv, qa).await.unwrap().is_empty());

    // Resume: the copy task was submitted, then the client "crashed" before waiting.
    os.filters = vec![];
    store.set_cv_repo(cv, &os).await.unwrap();
    let v4 = store.begin_version(cv, "resume", &store.cv_repos(cv).await.unwrap()).await.unwrap();
    let mut pending = String::new();
    for mut vr in store.version_repos(v4.id).await.unwrap() {
        let latest_src = latest(&c, &vr.repo().repo_href).await;
        vr.source_version = Some(latest_src.clone());
        if vr.repo().repo_type == RepoType::Rpm {
            vr.derived_repo = os_v3.derived_repo.clone();
            let cfg = json!({ "source_repo_version": latest_src, "dest_repo": vr.derived_repo, "dest_base_version": 0 });
            let Mutation::Task(t) = c.post(&c.api("rpm/copy/"), &json!({ "config": [cfg], "dependency_solving": false })).await.unwrap()
            else {
                panic!()
            };
            vr.pending_task = Some(t.clone());
            pending = t;
        }
        store.save_version_repo(&vr).await.unwrap();
    }
    let seen: Mutex<Vec<Task>> = Mutex::new(vec![]);
    let on = |t: &Task| seen.lock().unwrap().push(t.clone());
    let (v4, _) = svc.resume(v4.id, &quiet, &on).await.unwrap();
    assert_eq!(v4.state, "published");
    let copies: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|t| t.name.contains("copy"))
        .map(|t| t.pulp_href.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(copies, vec![pending], "resume re-attached instead of resubmitting");
    let os_v4 = store.version_repos(v4.id).await.unwrap().into_iter().find(|v| v.repo().repo_name == "os").unwrap();
    assert_eq!(os_v4.content_count, Some(6), "5 packages + advisory");

    // Unbound versions can be deleted; their Pulp objects go away unless shared.
    svc.delete_version(v2.id, &quiet, &no_progress).await.unwrap();
    assert_eq!(store.version(v2.id).await.unwrap().state, "deleted");

    // Full cleanup through the service removes distributions and derived repositories.
    svc.delete_view(cv, &quiet, &no_progress).await.unwrap();
    assert!(c.get::<serde_json::Value>(os_v1.derived_repo.as_deref().unwrap(), &[]).await.unwrap_err().is_not_found());
    cleanup(&c, &[&rpm_repo, &file_repo, &rpm_remote, &file_remote]).await;
}

/// Relabeling an environment moves its served paths in place; later promotions use the new label.
#[tokio::test]
async fn environment_relabel_moves_served_paths() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let id = unique("cvlb");
    let (repo, remote) = synced_repo(&c, RepoType::File, &format!("{id}-docs"), "file/PULP_MANIFEST", "mirror").await;
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let profile = c.profile().to_string();
    let svc = CvService { client: c.clone(), store: store.clone(), scope: profile.clone(), actor: "test@live".into(), lock: None };
    let cv = store.create_cv(&profile, &id, "").await.unwrap();
    store
        .set_cv_repo(cv, &CvRepo { repo_href: repo.clone(), repo_name: "docs".into(), repo_type: RepoType::File, filters: vec![] })
        .await
        .unwrap();
    let (v1, _) = svc.publish(cv, "", "", &quiet, &no_progress).await.unwrap();
    let lib = store.ensure_library(&profile).await.unwrap();
    let dev = store.create_env(&profile, "Dev", lib, false).await.unwrap();
    svc.promote(cv, v1.id, dev, Mode::Promote, &quiet, &no_progress).await.unwrap();
    let readme = |label: &str| format!("http://127.0.0.1:8080/pulp/content/{}/docs/readme.txt", base_path(&id, label, "docs"));
    assert_eq!(c.fetch_content(&readme("dev"), 1 << 20).await.unwrap().0, 200);

    // The impact names the move; the relabel moves the one served row and the view still verifies.
    let dist = store.binding_repos(dev, cv).await.unwrap()[0].distribution.clone();
    let impact = svc.relabel_impact(dev, "dev2").await.unwrap();
    assert_eq!((impact.len(), impact[0].1.as_str()), (1, base_path(&id, "dev2", "docs").as_str()));
    let (lines, failed) = svc.relabel_env(dev, "dev2", &quiet, &no_progress).await.unwrap();
    assert_eq!(failed, 0, "{lines:?}");
    assert!(svc.verify(cv, dev).await.unwrap().is_empty());
    assert_eq!(c.fetch_content(&readme("dev2"), 1 << 20).await.unwrap().0, 200);
    assert_eq!(c.fetch_content(&readme("dev"), 1 << 20).await.unwrap().0, 404, "the old path is gone");
    assert_eq!(store.binding_repos(dev, cv).await.unwrap()[0].base_path, base_path(&id, "dev2", "docs"));

    // The distribution moved in place (same object), and a second promotion serves at the new label.
    svc.promote(cv, v1.id, dev, Mode::Promote, &quiet, &no_progress).await.unwrap();
    let row = store.binding_repos(dev, cv).await.unwrap().remove(0);
    assert_eq!((row.distribution, row.base_path), (dist, base_path(&id, "dev2", "docs")));
    assert!(svc.verify(cv, dev).await.unwrap().is_empty());
    assert_eq!(c.fetch_content(&readme("dev"), 1 << 20).await.unwrap().0, 404);

    svc.delete_view(cv, &quiet, &no_progress).await.unwrap();
    cleanup(&c, &[&repo, &remote]).await;
}

/// A File view serves its files under the manifest name of the source repository.
#[tokio::test]
async fn a_file_view_serves_the_manifest_name_of_its_source() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let id = unique("cvmf");
    let (repo, remote) = synced_repo(&c, RepoType::File, &format!("{id}-docs"), "file/PULP_MANIFEST", "mirror").await;
    let m = c.patch(&repo, &json!({ "manifest": "CUSTOM_MANIFEST" })).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let profile = c.profile().to_string();
    let svc = CvService { client: c.clone(), store: store.clone(), scope: profile.clone(), actor: "test@live".into(), lock: None };
    let cv = store.create_cv(&profile, &id, "").await.unwrap();
    let docs = CvRepo { repo_href: repo.clone(), repo_name: "docs".into(), repo_type: RepoType::File, filters: vec![] };
    store.set_cv_repo(cv, &docs).await.unwrap();
    svc.publish(cv, "", "", &quiet, &no_progress).await.unwrap();
    let url = |f: &str| format!("http://127.0.0.1:8080/pulp/content/{}/{f}", base_path(&id, "library", "docs"));
    assert_eq!(served(&c, &url("CUSTOM_MANIFEST")).await.0, 200);
    assert_eq!(served(&c, &url("PULP_MANIFEST")).await.0, 404, "the default name is not served");
    let lib = store.ensure_library(&profile).await.unwrap();
    assert_eq!(svc.verify(cv, lib).await.unwrap(), Vec::<String>::new());
    svc.delete_view(cv, &quiet, &no_progress).await.unwrap();
    cleanup(&c, &[&repo, &remote]).await;
}

/// Resuming an older failed version publishes it; Library keeps serving the newer version.
#[tokio::test]
async fn resume_of_an_older_version_keeps_library_on_the_newer_one() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let id = unique("cvrs");
    let (repo, remote) = synced_repo(&c, RepoType::File, &format!("{id}-docs"), "file/PULP_MANIFEST", "mirror").await;
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let profile = c.profile().to_string();
    let svc = CvService { client: c.clone(), store: store.clone(), scope: profile.clone(), actor: "test@live".into(), lock: None };
    let cv = store.create_cv(&profile, &id, "").await.unwrap();
    let docs = CvRepo { repo_href: repo.clone(), repo_name: "docs".into(), repo_type: RepoType::File, filters: vec![] };
    store.set_cv_repo(cv, &docs).await.unwrap();
    // v1 (without docs/*) stopped before its first step; v2 (everything) is published and active in Library.
    let filters = parse_rules("exclude path=docs/*", RepoType::File).unwrap();
    let v1 = store.begin_version(cv, "", &[CvRepo { filters, ..docs }]).await.unwrap();
    let mut row = store.version_repos(v1.id).await.unwrap().remove(0);
    row.source_version = Some(latest(&c, &repo).await);
    store.save_version_repo(&row).await.unwrap();
    store.finish_version(v1.id, "failed", Some("stopped")).await.unwrap();
    let (v2, note) = svc.publish(cv, "", "", &quiet, &no_progress).await.unwrap();
    assert_eq!(note, None);

    let (v1, note) = svc.resume(v1.id, &quiet, &no_progress).await.unwrap();
    assert_eq!(v1.state, "published");
    assert_eq!(note.as_deref(), Some("Library not changed: v2 is newer (use Rollback to serve v1)"));
    let lib = store.ensure_library(&profile).await.unwrap();
    assert_eq!(store.binding(lib, cv).await.unwrap().unwrap().active(), Some(v2.id));
    let (r1, r2) = (store.version_repos(v1.id).await.unwrap(), store.version_repos(v2.id).await.unwrap());
    assert_ne!(r1[0].publication, r2[0].publication, "the versions differ");
    // The distribution in Pulp serves the publication of v2.
    assert!(svc.verify(cv, lib).await.unwrap().is_empty());
    let readme = format!("http://127.0.0.1:8080/pulp/content/{}/docs/readme.txt", base_path(&id, "library", "docs"));
    assert_eq!(served(&c, &readme).await.0, 200);

    svc.delete_view(cv, &quiet, &no_progress).await.unwrap();
    cleanup(&c, &[&repo, &remote]).await;
}

#[tokio::test]
async fn unpromote_goes_back_along_the_path() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let id = unique("cvun");
    let (repo, remote) = synced_repo(&c, RepoType::File, &format!("{id}-docs"), "file/PULP_MANIFEST", "mirror").await;
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let profile = c.profile().to_string();
    let svc = CvService { client: c.clone(), store: store.clone(), scope: profile.clone(), actor: "test@live".into(), lock: None };
    let cv = store.create_cv(&profile, &id, "").await.unwrap();
    store
        .set_cv_repo(cv, &CvRepo { repo_href: repo.clone(), repo_name: "docs".into(), repo_type: RepoType::File, filters: vec![] })
        .await
        .unwrap();
    let (v1, _) = svc.publish(cv, "", "", &quiet, &no_progress).await.unwrap();
    let lib = store.ensure_library(&profile).await.unwrap();
    let dev = store.create_env(&profile, "Dev", lib, false).await.unwrap();
    let test = store.create_env(&profile, "Test", dev, false).await.unwrap();
    svc.promote(cv, v1.id, dev, Mode::Promote, &quiet, &no_progress).await.unwrap();
    svc.promote(cv, v1.id, test, Mode::Promote, &quiet, &no_progress).await.unwrap();
    let readme = |label: &str| format!("http://127.0.0.1:8080/pulp/content/{}/docs/readme.txt", base_path(&id, label, "docs"));
    assert_eq!(c.fetch_content(&readme("test"), 1 << 20).await.unwrap().0, 200);

    let e = svc.unpromote(cv, dev, &quiet, &no_progress).await.unwrap_err().to_string();
    assert_eq!(e, format!("{id} is still in Test; unpromote it there first"));
    assert_eq!(c.fetch_content(&readme("dev"), 1 << 20).await.unwrap().0, 200, "the refusal changed nothing");

    // Test stops serving and its distribution is gone in Pulp; Dev still serves.
    let dist = store.binding_repos(test, cv).await.unwrap()[0].distribution.clone();
    svc.unpromote(cv, test, &quiet, &no_progress).await.unwrap();
    assert_eq!(c.fetch_content(&readme("test"), 1 << 20).await.unwrap().0, 404);
    assert!(c.get::<serde_json::Value>(&dist, &[]).await.unwrap_err().is_not_found());
    assert_eq!(c.fetch_content(&readme("dev"), 1 << 20).await.unwrap().0, 200);

    // A new promotion serves the same path again.
    svc.promote(cv, v1.id, test, Mode::Promote, &quiet, &no_progress).await.unwrap();
    assert_eq!(c.fetch_content(&readme("test"), 1 << 20).await.unwrap().0, 200);
    assert!(svc.verify(cv, test).await.unwrap().is_empty());

    // Back along the path; then the version is no longer bound and can be deleted.
    for env in [test, dev, lib] {
        svc.unpromote(cv, env, &quiet, &no_progress).await.unwrap();
    }
    assert_eq!(c.fetch_content(&readme("dev"), 1 << 20).await.unwrap().0, 404);
    svc.delete_version(v1.id, &quiet, &no_progress).await.unwrap();

    svc.delete_view(cv, &quiet, &no_progress).await.unwrap();
    cleanup(&c, &[&repo, &remote]).await;
}

/// A promotion that fails for every repository keeps the version that Pulp still serves bound: the list shows it with
/// the failed attempt, it cannot be deleted, and a rollback to it works.
#[tokio::test]
async fn a_failed_promotion_keeps_the_served_version() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let id = unique("cvff");
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let profile = c.profile().to_string();
    let svc = CvService { client: c.clone(), store: store.clone(), scope: profile.clone(), actor: "test@live".into(), lock: None };
    let cv = store.create_cv(&profile, &id, "").await.unwrap();
    let mut sources = vec![];
    for name in ["docs", "more"] {
        let (repo, remote) = synced_repo(&c, RepoType::File, &format!("{id}-{name}"), "file/PULP_MANIFEST", "mirror").await;
        let r = CvRepo { repo_href: repo.clone(), repo_name: name.into(), repo_type: RepoType::File, filters: vec![] };
        store.set_cv_repo(cv, &r).await.unwrap();
        sources.extend([repo, remote]);
    }
    let (v1, _) = svc.publish(cv, "", "", &quiet, &no_progress).await.unwrap();
    let lib = store.ensure_library(&profile).await.unwrap();
    let dev = store.create_env(&profile, "Dev", lib, false).await.unwrap();
    svc.promote(cv, v1.id, dev, Mode::Promote, &quiet, &no_progress).await.unwrap();
    let (v2, _) = svc.publish(cv, "", "", &quiet, &no_progress).await.unwrap();

    // Renamed outside pulp-tui: the promotion finds no distribution of its name, and their base paths are taken.
    let rows = store.binding_repos(dev, cv).await.unwrap();
    let rename = async |suffix: &str| {
        for b in &rows {
            let name = format!("{}{suffix}", pulp_tui::cv::distribution_name(&id, "Dev", &b.repo_name));
            let m = c.patch(&b.distribution, &json!({ "name": name })).await.unwrap();
            ops::settle(&c, m, &no_progress).await.unwrap();
        }
    };
    rename("-moved").await;
    // The preview reads the distribution from Pulp: it names the renamed one and says the promotion fails.
    let env = store.environments(&profile).await.unwrap().into_iter().find(|e| e.id == dev).unwrap();
    let docs = store.version_repos(v2.id).await.unwrap().remove(0);
    let line = svc.promote_target(&id, &env, docs.repo(), rows.iter().find(|b| b.repo_name == "docs")).await.unwrap();
    let moved = pulp_tui::cv::distribution_name(&id, "Dev", "docs");
    assert_eq!(
        line,
        format!(
            "  docs (File): distribution {moved}-moved at /{} was renamed outside pulp-tui; the promotion looks for {moved} and fails: \
             rename it back",
            base_path(&id, "dev", "docs")
        )
    );
    let e = svc.promote(cv, v2.id, dev, Mode::Promote, &quiet, &no_progress).await.unwrap_err().to_string();
    assert!(e.starts_with(&format!("{id} in Dev: failed: ")) && e.contains("is used by distribution"), "{e}");
    let b = store.binding(dev, cv).await.unwrap().unwrap();
    assert_eq!((b.served, b.attempt.map(|a| (a.version_id, a.state))), (Some(v1.id), Some((v2.id, "failed".to_string()))));
    let cfg = pulp_tui::config::Config::default();
    let list = pulp_tui::tui::kinds::load(&Kind::ContentViews, None, Ok(&*store), &profile, &profile, &cfg, 0, &id).await.unwrap().0;
    assert_eq!(list[0]["bound"], "Library:v2  Dev:v1 (v2 failed)");
    // Pulp still serves v1 in Dev: only the failed attempt is reported.
    assert_eq!(svc.verify(cv, dev).await.unwrap(), ["promotion of v2 is failed"]);
    let e = svc.delete_version(v1.id, &quiet, &no_progress).await.unwrap_err().to_string();
    assert_eq!(e, "version 1 is bound to Dev");
    assert_eq!(store.version(v1.id).await.unwrap().state, "published", "the rollback target stays");

    rename("").await;
    svc.promote(cv, v1.id, dev, Mode::Rollback, &quiet, &no_progress).await.unwrap();
    assert_eq!(store.binding(dev, cv).await.unwrap().unwrap().active(), Some(v1.id));
    assert!(svc.verify(cv, dev).await.unwrap().is_empty());

    svc.delete_view(cv, &quiet, &no_progress).await.unwrap();
    cleanup(&c, &sources.iter().map(String::as_str).collect::<Vec<_>>()).await;
}

/// Pulp refuses a base path that is inside another one or contains one. A distribution outside pulp-tui above the new
/// Dev path, then one below it: the preview says that the promotion fails, and pulp-tui refuses before Pulp (no 400).
/// After a clean preview, the promotion completes.
#[tokio::test]
async fn a_promotion_to_a_base_path_that_overlaps_another_fails_as_the_preview_says() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let id = unique("cvov");
    let (repo, remote) = synced_repo(&c, RepoType::File, &format!("{id}-docs"), "file/PULP_MANIFEST", "mirror").await;
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let profile = c.profile().to_string();
    let svc = CvService { client: c.clone(), store: store.clone(), scope: profile.clone(), actor: "test@live".into(), lock: None };
    let cv = store.create_cv(&profile, &id, "").await.unwrap();
    let docs = CvRepo { repo_href: repo.clone(), repo_name: "docs".into(), repo_type: RepoType::File, filters: vec![] };
    store.set_cv_repo(cv, &docs).await.unwrap();
    let (v1, _) = svc.publish(cv, "", "", &quiet, &no_progress).await.unwrap();
    let lib = store.ensure_library(&profile).await.unwrap();
    let dev = store.create_env(&profile, "Dev", lib, false).await.unwrap();
    let env = store.environments(&profile).await.unwrap().into_iter().find(|e| e.id == dev).unwrap();
    let (path, name) = (base_path(&id, "dev", "docs"), pulp_tui::cv::distribution_name(&id, "Dev", "docs"));
    let new = format!("  docs (File): new distribution {name} at /{path}");
    let foreign = format!("{id}-foreign");
    for other in [format!("dev/{id}"), format!("{path}/extra")] {
        let body = json!({ "name": foreign, "base_path": other });
        let href = create(&c, RepoType::File.distributions(), body, "/distributions/").await;
        let line = svc.promote_target(&id, &env, &docs, None).await.unwrap();
        assert_eq!(line, format!("{new}; distribution {foreign} at /{other} overlaps that base path, so the promotion fails for it"));
        let e = svc.promote(cv, v1.id, dev, Mode::Promote, &quiet, &no_progress).await.unwrap_err().to_string();
        let want =
            format!("{id} in Dev: failed: docs: conflict [409]: base path {path} overlaps base path {other} of distribution {foreign}");
        assert!(e.starts_with(&want), "{e}");
        cleanup(&c, &[&href]).await;
    }
    assert_eq!(svc.promote_target(&id, &env, &docs, None).await.unwrap(), new);
    svc.promote(cv, v1.id, dev, Mode::Promote, &quiet, &no_progress).await.unwrap();
    assert!(svc.verify(cv, dev).await.unwrap().is_empty());

    svc.delete_view(cv, &quiet, &no_progress).await.unwrap();
    cleanup(&c, &[&repo, &remote]).await;
}

/// Two databases (stores with different ids) share one Pulp and one Content View name.
#[tokio::test]
async fn content_view_owned_by_another_database() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let id = unique("cvdb");
    let (repo, remote) = synced_repo(&c, RepoType::File, &format!("{id}-docs"), "file/PULP_MANIFEST", "mirror").await;
    let profile = c.profile().to_string();
    let docs = CvRepo { repo_href: repo.clone(), repo_name: "docs".into(), repo_type: RepoType::File, filters: vec![] };
    let mut services = vec![];
    for actor in ["a@live", "b@live"] {
        let store = Arc::new(Store::open_in_memory().await.unwrap());
        let cv = store.create_cv(&profile, &id, "").await.unwrap();
        store.set_cv_repo(cv, &docs).await.unwrap();
        let svc = CvService { client: c.clone(), store, scope: profile.clone(), actor: actor.into(), lock: None };
        services.push((svc, cv));
    }
    let ((a, a_cv), (b, b_cv)) = (services.remove(0), services.remove(0));
    let label = |h: String| {
        let c = c.clone();
        async move { c.get::<serde_json::Value>(&h, &[]).await.unwrap()["pulp_labels"]["pulp_tui_db"].as_str().map(String::from) }
    };

    // A publishes and promotes to Library: its derived repository and distribution carry A's id.
    let (v, _) = a.publish(a_cv, "", "", &quiet, &no_progress).await.unwrap();
    let derived = a.store.version_repos(v.id).await.unwrap().remove(0).derived_repo.unwrap();
    let lib = a.store.ensure_library(&profile).await.unwrap();
    let dist = a.store.binding_repos(lib, a_cv).await.unwrap().remove(0).distribution;
    assert_eq!(label(derived.clone()).await.as_deref(), Some(a.store.db_id()));
    assert_eq!(label(dist.clone()).await.as_deref(), Some(a.store.db_id()));

    // B refuses the foreign derived repository, and writes nothing.
    let err = format!("{:#}", b.publish(b_cv, "", "", &quiet, &no_progress).await.unwrap_err());
    let foreign = format!("is owned by another pulp-tui database ({})", a.store.db_id());
    assert!(err.contains(&foreign), "{err}");
    assert_eq!(label(derived.clone()).await.as_deref(), Some(a.store.db_id()));

    // Once B knows A's id as an alias, B adopts both objects.
    b.store.add_alias(a.store.db_id()).await.unwrap();
    b.publish(b_cv, "", "", &quiet, &no_progress).await.unwrap();
    assert_eq!(label(derived.clone()).await.as_deref(), Some(b.store.db_id()));
    assert_eq!(label(dist.clone()).await.as_deref(), Some(b.store.db_id()));

    // Now A is the other database: deleting its view keeps what B owns.
    let logs = Mutex::new(vec![]);
    a.delete_view(a_cv, &|m| logs.lock().unwrap().push(m), &no_progress).await.unwrap();
    let logs = logs.into_inner().unwrap().join("\n");
    let kept = format!("owned by another pulp-tui database ({})", b.store.db_id());
    assert!(logs.contains(&format!("keeping {derived}: {kept}")) && logs.contains(&format!("keeping {dist}: {kept}")), "{logs}");
    assert!(c.get::<serde_json::Value>(&derived, &[]).await.is_ok());

    b.delete_view(b_cv, &quiet, &no_progress).await.unwrap();
    assert!(c.get::<serde_json::Value>(&derived, &[]).await.unwrap_err().is_not_found());
    cleanup(&c, &[&repo, &remote]).await;
}

/// Composite `rocky9`: an OS view pinned at v1 and an extras view at latest. The composite borrows the
/// components' Pulp objects, follows the extras with auto publish, and its deletes never remove them.
#[tokio::test]
async fn composite_content_view_rocky9() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let id = unique("cvc");
    let (os_name, extras_name) = (format!("{id}-os"), format!("{id}-extras"));
    let (file_repo, file_remote) = synced_repo(&c, RepoType::File, &format!("{id}-src-docs"), "file/PULP_MANIFEST", "mirror").await;
    let (rpm_repo, rpm_remote) = synced_repo(&c, RepoType::Rpm, &format!("{id}-src-os"), "rpm/", "additive").await;
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let profile = c.profile().to_string();
    let svc = CvService { client: c.clone(), store: store.clone(), scope: profile.clone(), actor: "test@live".into(), lock: None };
    let os = store.create_cv(&profile, &os_name, "").await.unwrap();
    let extras = store.create_cv(&profile, &extras_name, "").await.unwrap();
    let repo = |href: &str, name: &str, t| CvRepo { repo_href: href.into(), repo_name: name.into(), repo_type: t, filters: vec![] };
    store.set_cv_repo(os, &repo(&file_repo, "docs", RepoType::File)).await.unwrap();
    store.set_cv_repo(extras, &repo(&rpm_repo, "os", RepoType::Rpm)).await.unwrap();
    let (os_v1, _) = svc.publish(os, "", "", &quiet, &no_progress).await.unwrap();
    let (extras_v1, _) = svc.publish(extras, "", "", &quiet, &no_progress).await.unwrap();

    // v1: os pinned, extras latest; nothing is copied, and Library serves the composite's own paths.
    let comp = store.create_composite(&profile, &id, "").await.unwrap();
    store.set_component(comp, os, Some(os_v1.id)).await.unwrap();
    store.set_component(comp, extras, None).await.unwrap();
    store.set_auto_publish(comp, true).await.unwrap();
    let (c1, _) = svc.publish(comp, "first", "", &quiet, &no_progress).await.unwrap();
    assert_eq!((c1.number, c1.state.as_str()), (1, "published"));
    let lib = store.ensure_library(&profile).await.unwrap();
    assert_eq!(svc.verify(comp, lib).await.unwrap(), Vec::<String>::new());
    assert!(svc.verify(extras, lib).await.unwrap().is_empty(), "one publication, served by two distributions");
    let url = |label: &str, repo: &str, file: &str| format!("http://127.0.0.1:8080/pulp/content/{}/{file}", base_path(&id, label, repo));
    assert_eq!(c.fetch_content(&url("library", "docs", "docs/readme.txt"), 1 << 20).await.unwrap().0, 200);
    assert_eq!(c.fetch_content(&url("library", "os", "repodata/repomd.xml"), 1 << 20).await.unwrap().0, 200);

    // Promoting to another environment: the composite's own distributions under the label of Dev.
    let dev = store.create_env(&profile, "Dev", lib, false).await.unwrap();
    svc.promote(comp, c1.id, dev, Mode::Promote, &quiet, &no_progress).await.unwrap();
    assert_eq!(svc.verify(comp, dev).await.unwrap(), Vec::<String>::new());
    assert_eq!(c.fetch_content(&url("dev", "docs", "docs/readme.txt"), 1 << 20).await.unwrap().0, 200);
    assert_eq!(c.fetch_content(&url("dev", "os", "repodata/repomd.xml"), 1 << 20).await.unwrap().0, 200);

    // Upstream update: publishing the extras publishes the composite (auto publish), with os still at v1.
    let m = c.patch(&rpm_remote, &json!({ "url": format!("{}/rpm2/", upstream()) })).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let m = ops::sync(&c, RepoType::Rpm, &rpm_repo, None, "additive").await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let (extras_v2, _) = svc.publish(extras, "update", "", &quiet, &no_progress).await.unwrap();
    assert_eq!(extras_v2.number, 2);
    let auto = svc.auto_publish(extras, &quiet, &no_progress).await;
    let want: Vec<Result<String, String>> = vec![Ok(format!("{id} v2 published"))];
    assert_eq!(auto, want);
    let c2 = store.versions(comp).await.unwrap().remove(0);
    assert_eq!((c2.number, c2.state.as_str()), (2, "published"));
    let used: Vec<_> = store.version_components(c2.id).await.unwrap().into_iter().map(|u| (u.name, u.number)).collect();
    assert_eq!(used, [(extras_name.clone(), 2), (os_name.clone(), 1)]);
    assert_eq!(svc.verify(comp, lib).await.unwrap(), Vec::<String>::new());
    let cmp = svc.compare_versions(c1.id, c2.id).await.unwrap().join("\n");
    assert!(cmp.contains(&format!("{extras_name}: v1 -> v2")) && cmp.contains("+ fixture-gamma-0.5-1.noarch"), "{cmp}");

    // Auto publish never promotes: Dev still serves v1 until v2 is promoted there, then it serves the extras of v2.
    assert_eq!(store.binding(dev, comp).await.unwrap().unwrap().active(), Some(c1.id));
    svc.promote(comp, c2.id, dev, Mode::Promote, &quiet, &no_progress).await.unwrap();
    assert_eq!(svc.verify(comp, dev).await.unwrap(), Vec::<String>::new());
    let extras_v2_pub = store.version_repos(extras_v2.id).await.unwrap().remove(0).publication;
    let served = store.binding_repos(dev, comp).await.unwrap().into_iter().find(|b| b.repo_name == "os").unwrap().publication;
    assert_eq!(served, extras_v2_pub);
    assert_eq!(c.fetch_content(&url("dev", "os", "repodata/repomd.xml"), 1 << 20).await.unwrap().0, 200);
    assert_eq!(c.fetch_content(&url("dev", "docs", "docs/readme.txt"), 1 << 20).await.unwrap().0, 200);

    // Refusals name what uses the component; deleting the old composite version keeps the borrowed objects.
    let e = svc.delete_version(extras_v1.id, &quiet, &no_progress).await.unwrap_err().to_string();
    assert!(e.contains(&format!("used by {id} v1")), "{e}");
    let e = svc.delete_view(os, &quiet, &no_progress).await.unwrap_err().to_string();
    assert!(e.contains("is used by"), "{e}");
    let extras_pub = store.version_repos(extras_v1.id).await.unwrap().remove(0).publication.unwrap();
    svc.delete_version(c1.id, &quiet, &no_progress).await.unwrap();
    assert_eq!(store.version(c1.id).await.unwrap().state, "deleted");
    assert!(c.get::<serde_json::Value>(&extras_pub, &[]).await.is_ok(), "the borrowed publication stays");

    // Deleting the composite removes its distributions only; both components still serve.
    let mut derived = vec![];
    for v in [os_v1.id, extras_v1.id] {
        derived.push(store.version_repos(v).await.unwrap().remove(0).derived_repo.unwrap());
    }
    let dists: Vec<String> = store.all_binding_repos(comp).await.unwrap().into_iter().map(|b| b.distribution).collect();
    assert_eq!(dists.len(), 4, "two repositories in Library and in Dev");
    svc.delete_view(comp, &quiet, &no_progress).await.unwrap();
    for d in &derived {
        assert!(c.get::<serde_json::Value>(d, &[]).await.is_ok(), "{d}");
    }
    for d in &dists {
        assert!(c.get::<serde_json::Value>(d, &[]).await.unwrap_err().is_not_found(), "{d}");
    }
    assert!(svc.verify(os, lib).await.unwrap().is_empty() && svc.verify(extras, lib).await.unwrap().is_empty());

    svc.delete_view(os, &quiet, &no_progress).await.unwrap();
    svc.delete_view(extras, &quiet, &no_progress).await.unwrap();
    cleanup(&c, &[&file_repo, &rpm_repo, &file_remote, &rpm_remote]).await;
}

/// The snapshot cleanup of a publish keeps a version that no view version records but a distribution serves.
/// Pulp refuses that delete too (`protected_versions`, 3.63.36 and later); the cleanup does not send it.
#[tokio::test]
async fn publish_cleanup_keeps_a_served_version_that_no_view_version_records() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let id = unique("cvkeep");
    let (repo, remote) = synced_repo(&c, RepoType::File, &format!("{id}-docs"), "file/PULP_MANIFEST", "mirror").await;
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let profile = c.profile().to_string();
    let svc = CvService { client: c.clone(), store: store.clone(), scope: profile.clone(), actor: "test@live".into(), lock: None };
    let cv = store.create_cv(&profile, &id, "").await.unwrap();
    let docs = CvRepo { repo_href: repo.clone(), repo_name: "docs".into(), repo_type: RepoType::File, filters: vec![] };
    store.set_cv_repo(cv, &docs).await.unwrap();
    let (v1, _) = svc.publish(cv, "", "", &quiet, &no_progress).await.unwrap();
    let row = store.version_repos(v1.id).await.unwrap().remove(0);
    let derived = row.derived_repo.clone().unwrap();

    // Outside the view: a new snapshot version without one file, published and served by a plain distribution.
    let one = ops::content_in(&c, row.derived_version.as_deref().unwrap()).await.unwrap().remove(0);
    ops::settle(&c, ops::remove_content(&c, &derived, vec![one]).await.unwrap(), &no_progress).await.unwrap();
    let unrecorded = latest(&c, &derived).await;
    let p = ops::create_publication(&c, RepoType::File, &unrecorded, &json!({}), &no_progress).await.unwrap();
    let name = format!("{id}-partial");
    let dist = ops::ensure_distribution(&c, RepoType::File, &name, &name, Some(&p), &json!({}), None, &no_progress).await.unwrap();

    // The next publish copies onto that version, so it is no longer the latest one and no view version records it.
    let lines = Mutex::new(vec![]);
    svc.publish(cv, "", "", &|s| lines.lock().unwrap().push(s), &no_progress).await.unwrap();
    assert_ne!(latest(&c, &derived).await, unrecorded, "the publish made a newer version");
    assert!(c.get::<RepoVersion>(&unrecorded, &[]).await.is_ok(), "the served version was deleted");
    let d: serde_json::Value = c.get(&dist, &[]).await.unwrap();
    assert_eq!(d["publication"], json!(p), "the distribution lost its publication");
    let keeping = format!("keeping {unrecorded}: served by {name} (pinned to a publication of this version)");
    assert!(lines.lock().unwrap().contains(&keeping), "{:?}", lines.lock().unwrap());

    cleanup(&c, &[&dist]).await;
    svc.delete_view(cv, &quiet, &no_progress).await.unwrap();
    cleanup(&c, &[&repo, &remote]).await;
}

/// Pulp protects neither a served publication nor a served repository: a version delete keeps what a plain
/// distribution serves, and the view delete is refused until that distribution is gone.
#[tokio::test]
async fn deletes_keep_what_a_plain_distribution_serves() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let id = unique("cvplain");
    let (repo, remote) = synced_repo(&c, RepoType::File, &format!("{id}-docs"), "file/PULP_MANIFEST", "mirror").await;
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let profile = c.profile().to_string();
    let svc = CvService { client: c.clone(), store: store.clone(), scope: profile.clone(), actor: "test@live".into(), lock: None };
    let cv = store.create_cv(&profile, &id, "").await.unwrap();
    let docs = CvRepo { repo_href: repo.clone(), repo_name: "docs".into(), repo_type: RepoType::File, filters: vec![] };
    store.set_cv_repo(cv, &docs).await.unwrap();
    let (v1, _) = svc.publish(cv, "", "", &quiet, &no_progress).await.unwrap();
    let row = store.version_repos(v1.id).await.unwrap().remove(0);
    let (p, dv, derived) = (row.publication.unwrap(), row.derived_version.unwrap(), row.derived_repo.unwrap());

    // Outside the view: a plain distribution pinned to the snapshot publication of v1. Then v1 is no longer bound.
    let name = format!("{id}-plain");
    let dist = ops::ensure_distribution(&c, RepoType::File, &name, &name, Some(&p), &json!({}), None, &no_progress).await.unwrap();
    let readme = format!("http://127.0.0.1:8080/pulp/content/{name}/docs/readme.txt");
    assert_eq!(served(&c, &readme).await.0, 200);
    let lib = store.ensure_library(&profile).await.unwrap();
    svc.unpromote(cv, lib, &quiet, &no_progress).await.unwrap();

    let why = format!("served by {name} (pinned to a publication of this version)");
    let want = vec![("docs".to_string(), p.clone(), Some(why.clone())), ("docs".to_string(), dv.clone(), Some(why.clone()))];
    assert_eq!(svc.delete_version_impact(v1.id).await.unwrap(), want);
    let lines = Mutex::new(vec![]);
    svc.delete_version(v1.id, &|s| lines.lock().unwrap().push(s), &no_progress).await.unwrap();
    assert_eq!(*lines.lock().unwrap(), [format!("keeping {p}: {why}"), format!("keeping {dv}: {why}")]);
    assert_eq!(store.version(v1.id).await.unwrap().state, "deleted");
    assert!(c.get::<serde_json::Value>(&p, &[]).await.is_ok(), "the served publication was deleted");
    assert_eq!(served(&c, &readme).await.0, 200, "the distribution still serves");

    let refusal = format!(
        "content view {id}: distribution {name} serves the snapshot repository of docs ({derived}); \
         delete {name} or point it to other content first"
    );
    assert_eq!(svc.delete_view_impact(cv).await.unwrap_err().to_string(), refusal);
    assert_eq!(svc.delete_view(cv, &quiet, &no_progress).await.unwrap_err().to_string(), refusal);
    assert!(c.get::<serde_json::Value>(&derived, &[]).await.is_ok(), "refused before any delete");
    assert_eq!(served(&c, &readme).await.0, 200);

    cleanup(&c, &[&dist]).await;
    svc.delete_view(cv, &quiet, &no_progress).await.unwrap();
    assert!(c.get::<serde_json::Value>(&derived, &[]).await.unwrap_err().is_not_found());
    cleanup(&c, &[&repo, &remote]).await;
}

/// `export-cv` on pulp2, `import-cv` on pulp1: each repository gets the href of the repository with its name and type
/// on pulp1, so the view publishes there. One missing repository refuses the whole import before a write. The export
/// carries the current name of a repository that was renamed on pulp2 after the view recorded it.
#[tokio::test]
async fn import_cv_maps_repositories_by_name_to_the_target_server() {
    if !live_enabled() {
        return;
    }
    let c1 = client();
    let c2 = client_for(&std::env::var("PULP_TUI_TEST_UPSTREAM_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into()), "pulp2");
    let id = unique("cvimp");
    let (docs, old, only2) = (format!("{id}-docs"), format!("{id}-old"), format!("{id}-only2"));
    let (repo1, remote1) = synced_repo(&c1, RepoType::File, &docs, "file/PULP_MANIFEST", "mirror").await;
    let repo2 = create(&c2, RepoType::File.repositories(), json!({ "name": old }), "/repositories/").await;
    let gone = create(&c2, RepoType::File.repositories(), json!({ "name": only2 }), "/repositories/").await;
    assert_ne!(repo1, repo2);
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let (p1, p2) = (c1.profile().to_string(), c2.profile().to_string());
    let file =
        |href: &str, name: &str| CvRepo { repo_href: href.into(), repo_name: name.into(), repo_type: RepoType::File, filters: vec![] };
    let cv2 = store.create_cv(&p2, &id, "").await.unwrap();
    store.set_cv_repo(cv2, &file(&repo2, &old)).await.unwrap();
    let b = store.create_cv(&p2, &format!("{id}-b"), "").await.unwrap();
    store.set_cv_repo(b, &file(&gone, &only2)).await.unwrap();
    let m = c2.patch(&repo2, &json!({ "name": docs })).await.unwrap();
    ops::settle(&c2, m, &no_progress).await.unwrap();
    let mut ex: Export = serde_json::from_slice(&serde_json::to_vec(&export(&c2, &store, &p2).await.unwrap()).unwrap()).unwrap();
    assert_eq!(ex.content_views.iter().find(|v| v.name == id).unwrap().repos, [file(&repo2, &docs)], "the name of pulp2 now");
    assert_eq!(store.cv_repos(cv2).await.unwrap(), [file(&repo2, &old)], "the store keeps the recorded name");
    let caps = Capabilities::detect(&c1).await.unwrap();

    let e = import(&c1, &caps, &store, &p1, &ex).await.unwrap_err();
    let want = format!("repositories not found on this server (by name and type): {only2} (File, view {id}-b); nothing was imported");
    assert_eq!(e.to_string(), want);
    assert!(store.content_views(&p1).await.unwrap().is_empty() && store.environments(&p1).await.unwrap().is_empty());

    ex.content_views.retain(|v| v.name == id);
    let (cvs, _, warnings) = import(&c1, &caps, &store, &p1, &ex).await.unwrap();
    assert_eq!((cvs, warnings), (1, vec![]));
    let cv1 = store.content_views(&p1).await.unwrap()[0].id;
    assert_eq!(store.cv_repos(cv1).await.unwrap(), [file(&repo1, &docs)]);
    let svc = CvService { client: c1.clone(), store: store.clone(), scope: p1.clone(), actor: "test@live".into(), lock: None };
    svc.publish(cv1, "", "", &quiet, &no_progress).await.unwrap();
    let url = |f: &str| format!("http://127.0.0.1:8080/pulp/content/{}/{f}", base_path(&id, "library", &docs));
    let (status, body) = served(&c1, &url("docs/readme.txt")).await;
    assert_eq!((status, body.len()), (200, 10), "served from pulp1's {docs}");
    let lib = store.ensure_library(&p1).await.unwrap();
    assert_eq!(svc.verify(cv1, lib).await.unwrap(), Vec::<String>::new());
    svc.delete_view(cv1, &quiet, &no_progress).await.unwrap();
    cleanup(&c1, &[&repo1, &remote1]).await;
    cleanup(&c2, &[&repo2, &gone]).await;
}

/// The local Content View screens through the TUI on a connected profile: an environment made with Create, protected
/// (L: at once), unprotected (L: after the typed name) and deleted (after the typed name); a view's Edit (description)
/// and Remove repository (x). Each change is in the store and in the reloaded list.
#[tokio::test]
async fn environments_and_view_definition_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let id = unique("cvdef");
    let (mut app, mut rx) = app().await;
    let (st, scope) = (app.cv_store.clone().unwrap(), app.conn.as_ref().unwrap().scope.clone());
    let env = |app: &App| row(app, "name", "Test").unwrap_or_else(|| panic!("the list reloads with it: {:?}", app.list.items));

    open_list(&mut app, &mut rx, Kind::Environments, "").await;
    run(&mut app, Act::Create, Value::Null);
    settle(&mut app, &mut rx).await; // the environments are read before the form
    fill(&mut app, &[("name", "Test"), ("prior", "Library")]);
    settle(&mut app, &mut rx).await;
    shown(&app, "environment Test created after Library");
    let e = env(&app);
    assert_eq!((e["prior"].as_str(), &e["protected"]), (Some("Library"), &json!(false)));

    run(&mut app, Act::ToggleProtected, e);
    assert!(app.modal.is_none(), "protecting asks nothing");
    settle(&mut app, &mut rx).await;
    shown(&app, "Test protected: true");
    let e = env(&app);
    assert_eq!(e["protected"], true);
    run(&mut app, Act::ToggleProtected, e);
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed.as_deref(), Some("Test"));
    assert!(lines[0].starts_with("Promote and roll back to Test"), "{lines:?}");
    settle(&mut app, &mut rx).await;
    shown(&app, "Test protected: false");
    let e = env(&app);
    assert_eq!(e["protected"], false);

    run(&mut app, Act::Delete, e.clone());
    settle(&mut app, &mut rx).await; // what it removes is read before the confirmation
    let (lines, typed) = yes(&mut app);
    let first = format!("Delete lifecycle environment Test (label {}). No view is bound to it.", s(&e, "label"));
    assert_eq!((typed.as_deref(), lines[0].clone()), (Some("Test"), first));
    settle(&mut app, &mut rx).await;
    shown(&app, "environment Test deleted");
    assert!(row(&app, "name", "Test").is_none(), "the list reloads without it");
    assert!(st.environments(&scope).await.unwrap().iter().all(|e| e.name != "Test"));

    // A view of two repositories (definitions only): Edit its description, Remove one repository.
    let cv = st.create_cv(&scope, &id, "old").await.unwrap();
    for name in ["docs", "more"] {
        let repo_href = format!("/pulp/api/v3/repositories/file/file/{id}-{name}/");
        st.set_cv_repo(cv, &CvRepo { repo_href, repo_name: name.into(), repo_type: RepoType::File, filters: vec![] }).await.unwrap();
    }
    let r = cv_row(&mut app, &mut rx, &id).await;
    run(&mut app, Act::Edit, r);
    fill(&mut app, &[("description", "new")]);
    settle(&mut app, &mut rx).await;
    shown(&app, "saved");
    assert_eq!(row(&app, "name", &id).unwrap()["description"], "new", "the list reloads");
    let r = row(&app, "name", &id).unwrap();
    run(&mut app, Act::CvRemoveRepo, r);
    settle(&mut app, &mut rx).await; // the definitions are read before the form
    fill(&mut app, &[("repo", "more")]);
    settle(&mut app, &mut rx).await;
    shown(&app, "more removed from the definition; published versions are unchanged");
    let names: Vec<String> = st.cv_repos(cv).await.unwrap().into_iter().map(|r| r.repo_name).collect();
    assert_eq!(names, ["docs"]);
    assert_eq!(row(&app, "name", &id).unwrap()["repos"].as_array().unwrap().len(), 1, "the list reloads");
}

/// The row of Content View `name` on the Content Views screen, loaded again.
async fn cv_row(app: &mut App, rx: &mut UnboundedReceiver<Msg>, name: &str) -> Value {
    open_list(app, rx, Kind::ContentViews, "").await;
    row(app, "name", name).unwrap_or_else(|| panic!("no view {name}: {:?}", app.list.items))
}

/// Publish (P) of view `name`: the form, the preview, the confirmation, the job.
async fn tui_publish(app: &mut App, rx: &mut UnboundedReceiver<Msg>, name: &str, notes: &str) {
    let r = cv_row(app, rx, name).await;
    run(app, Act::CvPublish, r);
    settle(app, rx).await; // the repositories are read before the form
    fill(app, &[("notes", notes)]);
    settle(app, rx).await;
    let (lines, typed) = yes(app);
    assert_eq!((typed, lines[0].as_str()), (None, "Publishing creates a new immutable version and makes it active in Library."));
    settle(app, rx).await;
}

/// Promote (M) or Rollback (b) of view `name` to version `number` in `env`: the form, the preview (its lines), the job.
async fn tui_promote(app: &mut App, rx: &mut UnboundedReceiver<Msg>, name: &str, act: Act, number: &str, env: &str) -> Vec<String> {
    let r = cv_row(app, rx, name).await;
    run(app, act, r);
    settle(app, rx).await; // the environments are read before the form
    fill(app, &[("version", number), ("env", env)]);
    settle(app, rx).await;
    let (lines, typed) = yes(app);
    assert_eq!(typed, None, "a plain confirmation: {env} is not protected");
    settle(app, rx).await;
    lines
}

/// The PULP_MANIFEST that the File repository `docs` of view `cv` serves in `env`, and the hrefs of the view's
/// distributions there.
async fn served_docs(c: &PulpClient, st: &Store, env: i64, cv: i64) -> (String, Vec<String>) {
    let rows = st.binding_repos(env, cv).await.unwrap();
    let docs = rows.iter().find(|b| b.repo_name == "docs").expect("docs is served");
    let d: Value = c.get(&docs.distribution, &[]).await.unwrap();
    let (status, body) = served(c, &format!("{}PULP_MANIFEST", d["base_url"].as_str().unwrap())).await;
    assert_eq!(status, 200);
    (String::from_utf8(body).unwrap(), rows.into_iter().map(|b| b.distribution).collect())
}

/// The Content View workflows through the TUI, on a File and an RPM repository with default remotes (not synced):
/// Sync repositories (f), Publish (P), Verify (D), Promote (M), Filters (F) then a second Publish, Rollback (b),
/// Compare on the Versions screen, Unpromote (u), Delete of a version, Delete of the view. Each step answers the
/// real forms and confirmations, then checks what Pulp serves and what the screen shows.
#[tokio::test]
async fn content_view_workflows_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, id) = (client(), unique("cvtui"));
    let mut sources = vec![];
    for (t, name, path) in [(RepoType::File, "docs", "file/PULP_MANIFEST"), (RepoType::Rpm, "os", "rpm/")] {
        let full = format!("{id}-{name}");
        let body = json!({ "name": full, "url": format!("{}/{path}", upstream()), "policy": "immediate" });
        let remote = create(&c, t.remotes(), body, "/remotes/").await;
        let repo = create(&c, t.repositories(), json!({ "name": full, "remote": remote }), "/repositories/").await;
        sources.push((t, name, repo, remote));
    }
    let (mut app, mut rx) = app().await;
    let (st, scope) = (app.cv_store.clone().unwrap(), app.conn.as_ref().unwrap().scope.clone());
    let cv = st.create_cv(&scope, &id, "").await.unwrap();
    for (t, name, repo, _) in &sources {
        st.set_cv_repo(cv, &CvRepo { repo_href: repo.clone(), repo_name: name.to_string(), repo_type: *t, filters: vec![] }).await.unwrap();
    }
    let lib = st.ensure_library(&scope).await.unwrap();
    let dev = st.create_env(&scope, "Dev", lib, false).await.unwrap();

    // Sync repositories (f): both sync from their default remotes; the result names each new version.
    let r = cv_row(&mut app, &mut rx, &id).await;
    run(&mut app, Act::CvSyncRepos, r);
    fill(&mut app, &[("policy", "additive")]);
    settle_tasks(&mut app, &mut rx).await;
    let Some(Modal::Text { title, lines, .. }) = app.modal.take() else { panic!("no sync result: {:?}", texts(&app)) };
    assert_eq!((title, lines[0].as_str()), (format!("Sync of {id}"), "2 synced, 0 failed, 0 skipped"));
    for (_, name, repo, _) in &sources {
        assert!(lines.contains(&format!("  {name}: synced, new version 1")), "{lines:?}");
        assert!(latest(&c, repo).await.ends_with("/versions/1/"));
        assert!(texts(&app).contains(&format!("task completed: sync {name} (additive)")), "{:?}", texts(&app));
    }

    // Publish (P) v1: active in Library; Verify (D) checks what Library serves.
    tui_publish(&mut app, &mut rx, &id, "first").await;
    assert!(texts(&app).contains(&format!("{id} version 1 published and active in Library")), "{:?}", texts(&app));
    let r = row(&app, "name", &id).unwrap();
    assert_eq!(r["latest"], "v1", "the list reloads");
    let v1 = st.versions(cv).await.unwrap().into_iter().find(|v| v.number == 1).unwrap();
    assert_eq!(st.binding(lib, cv).await.unwrap().unwrap().active(), Some(v1.id));
    assert!(served_docs(&c, &st, lib, cv).await.0.contains("isos/boot.iso"));
    run(&mut app, Act::CvDrift, r);
    settle(&mut app, &mut rx).await;
    let Some(Modal::Text { lines, .. }) = app.modal.take() else { panic!("no verify result: {:?}", texts(&app)) };
    assert_eq!(lines, ["Library: v1 OK - served metadata match; RPM/Deb: also the smallest package"]);

    // Promote (M) v1 to Dev.
    let lines = tui_promote(&mut app, &mut rx, &id, Act::Promote, "1", "Dev").await;
    assert_eq!(lines[0], format!("{id}: Dev now serves nothing; will serve v1"));
    assert!(texts(&app).contains(&format!("{id} v1 active in Dev; served metadata verified")), "{:?}", texts(&app));
    let (manifest, dev_dists) = served_docs(&c, &st, dev, cv).await;
    assert!(manifest.contains("isos/boot.iso") && dev_dists.len() == 2, "{manifest} {dev_dists:?}");

    // Filters (F) of docs, then v2 without the ISO; Promote v2 to Dev.
    let r = cv_row(&mut app, &mut rx, &id).await;
    run(&mut app, Act::CvFilters, r);
    settle(&mut app, &mut rx).await; // the definitions are read before the form
    fill(&mut app, &[("repo", "docs"), ("filters", "exclude path=isos/*")]);
    settle(&mut app, &mut rx).await;
    assert!(texts(&app).contains(&"filters of docs: exclude path=isos/*".to_string()), "{:?}", texts(&app));
    tui_publish(&mut app, &mut rx, &id, "no iso").await;
    assert!(texts(&app).contains(&format!("{id} version 2 published and active in Library")), "{:?}", texts(&app));
    assert!(!served_docs(&c, &st, lib, cv).await.0.contains("isos/boot.iso"), "Library serves v2");
    tui_promote(&mut app, &mut rx, &id, Act::Promote, "2", "Dev").await;
    assert!(texts(&app).contains(&format!("{id} v2 active in Dev; served metadata verified")), "{:?}", texts(&app));
    assert!(!served_docs(&c, &st, dev, cv).await.0.contains("isos/boot.iso"));

    // Rollback (b) Dev to v1: Dev serves the ISO again; v2 stays published.
    let lines = tui_promote(&mut app, &mut rx, &id, Act::Rollback, "1", "Dev").await;
    assert!(lines.contains(&"Rollback only changes bindings; newer versions stay available.".to_string()), "{lines:?}");
    assert!(texts(&app).contains(&format!("{id} v1 active in Dev; served metadata verified")), "{:?}", texts(&app));
    assert!(served_docs(&c, &st, dev, cv).await.0.contains("isos/boot.iso"));
    assert_eq!(st.binding(dev, cv).await.unwrap().unwrap().active(), Some(v1.id));
    assert_eq!(cv_row(&mut app, &mut rx, &id).await["bound"], "Library:v2  Dev:v1", "the list shows the bindings");

    // Compare (m) on the Versions screen: v1 marked, v2 selected.
    let r = cv_row(&mut app, &mut rx, &id).await;
    run(&mut app, Act::CvVersions, r);
    settle(&mut app, &mut rx).await;
    let v2_row = app.list.items.iter().find(|v| v["number"] == 2).cloned().unwrap();
    app.list.marked = [v1.id.to_string()].into();
    run(&mut app, Act::Compare, v2_row);
    settle(&mut app, &mut rx).await;
    let Some(Modal::Text { lines, .. }) = app.modal.take() else { panic!("no comparison: {:?}", texts(&app)) };
    assert!(lines.iter().any(|l| l.starts_with("- ") && l.contains("isos/boot.iso")), "{lines:?}");
    app.list.marked.clear();
    app.back();
    settle(&mut app, &mut rx).await;

    // Unpromote (u) from Dev: its distributions go, after the typed name.
    let r = cv_row(&mut app, &mut rx, &id).await;
    run(&mut app, Act::Unpromote, r);
    settle(&mut app, &mut rx).await;
    fill(&mut app, &[("env", "Dev")]);
    settle(&mut app, &mut rx).await;
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed.as_deref(), Some(id.as_str()));
    assert!(lines[0].starts_with(&format!("Unpromote {id} from Dev: delete its distributions")), "{lines:?}");
    settle(&mut app, &mut rx).await;
    assert!(texts(&app).contains(&format!("{id} is not in Dev any more")), "{:?}", texts(&app));
    assert_eq!(st.binding(dev, cv).await.unwrap(), None);
    for d in &dev_dists {
        assert!(c.get::<Value>(d, &[]).await.unwrap_err().is_not_found(), "{d} is deleted");
    }

    // Delete v1 (Versions, d): the objects it alone uses go, after the typed version.
    let r = cv_row(&mut app, &mut rx, &id).await;
    run(&mut app, Act::CvVersions, r);
    settle(&mut app, &mut rx).await;
    let v1_row = app.list.items.iter().find(|v| v["number"] == 1).cloned().unwrap();
    run(&mut app, Act::Delete, v1_row);
    settle(&mut app, &mut rx).await;
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed.as_deref(), Some("v1"));
    let gone: Vec<String> = lines.iter().filter_map(|l| l.split_once(": delete ").map(|(_, h)| h.to_string())).collect();
    assert!(!gone.is_empty(), "the docs snapshot of v1 is its own: {lines:?}");
    settle(&mut app, &mut rx).await;
    assert!(texts(&app).contains(&"version deleted".to_string()), "{:?}", texts(&app));
    assert_eq!(st.version(v1.id).await.unwrap().state, "deleted");
    assert!(app.list.items.iter().find(|v| v["number"] == 1).is_none_or(|v| v["state"] == "deleted"), "the list reloads");
    for h in &gone {
        assert!(c.get::<Value>(h, &[]).await.unwrap_err().is_not_found(), "{h} is deleted");
    }
    app.back();
    settle(&mut app, &mut rx).await;

    // Delete the view (d): its Library distributions and snapshot repositories go, after the typed name.
    let snapshots: Vec<String> = st.version_repos(v1.id).await.unwrap().into_iter().filter_map(|r| r.derived_repo).collect();
    let (_, lib_dists) = served_docs(&c, &st, lib, cv).await;
    let r = cv_row(&mut app, &mut rx, &id).await;
    run(&mut app, Act::Delete, r);
    settle(&mut app, &mut rx).await;
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed.as_deref(), Some(id.as_str()));
    assert!(lines.contains(&"Served now:".to_string()), "{lines:?}");
    settle(&mut app, &mut rx).await;
    assert!(texts(&app).contains(&format!("content view {id} deleted")), "{:?}", texts(&app));
    assert!(row(&app, "name", &id).is_none(), "the list reloads without it");
    assert_eq!(snapshots.len(), 2);
    for h in snapshots.iter().chain(&lib_dists) {
        assert!(c.get::<Value>(h, &[]).await.unwrap_err().is_not_found(), "{h} is deleted");
    }
    for (_, _, repo, remote) in &sources {
        cleanup(&c, &[repo, remote]).await;
    }
}

/// A Content View service on the store of the TUI `app`, for the setup and the cleanup of a test.
fn service(app: &App, c: &PulpClient) -> CvService {
    let (store, scope) = (app.cv_store.clone().unwrap(), app.conn.as_ref().unwrap().scope.clone());
    CvService { client: c.clone(), store, scope, actor: "test@live".into(), lock: None }
}

/// A composite view through the TUI: Create (composite), Add components (a) with their pins (os at v1, extras at
/// latest), Edit (auto publish on), Publish (P): Library serves the composite's own paths. A Publish of the extras then
/// auto-publishes the composite with extras v2 and os still v1. Remove component (x) of the extras.
#[tokio::test]
async fn composite_content_view_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, id) = (client(), unique("cvctui"));
    let (os_name, extras_name) = (format!("{id}-os"), format!("{id}-extras"));
    let (file_repo, file_remote) = synced_repo(&c, RepoType::File, &format!("{id}-src-docs"), "file/PULP_MANIFEST", "mirror").await;
    let (rpm_repo, rpm_remote) = synced_repo(&c, RepoType::Rpm, &format!("{id}-src-os"), "rpm/", "additive").await;
    let (mut app, mut rx) = app().await;
    let svc = service(&app, &c);
    let (st, scope) = (svc.store.clone(), svc.scope.clone());
    let os = st.create_cv(&scope, &os_name, "").await.unwrap();
    let extras = st.create_cv(&scope, &extras_name, "").await.unwrap();
    let repo = |href: &str, name: &str, t| CvRepo { repo_href: href.into(), repo_name: name.into(), repo_type: t, filters: vec![] };
    st.set_cv_repo(os, &repo(&file_repo, "docs", RepoType::File)).await.unwrap();
    st.set_cv_repo(extras, &repo(&rpm_repo, "os", RepoType::Rpm)).await.unwrap();
    tui_publish(&mut app, &mut rx, &os_name, "").await;
    tui_publish(&mut app, &mut rx, &extras_name, "").await;

    // Create (c) the composite.
    open_list(&mut app, &mut rx, Kind::ContentViews, "").await;
    run(&mut app, Act::Create, Value::Null);
    fill(&mut app, &[("name", &id), ("composite", "true")]);
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("composite content view {id} created; add components with a"));
    let r = row(&app, "name", &id).unwrap_or_else(|| panic!("the list reloads with it: {:?}", app.list.items));
    assert_eq!(r["composite"], true);
    let comp = r["id"].as_i64().unwrap();

    // Add components (a): both views; os pinned at v1, extras at latest.
    run(&mut app, Act::CvAddComponent, r);
    load_choices(&mut app, &mut rx, "components").await;
    fill(&mut app, &[("components", &format!("{os}\n{extras}"))]);
    settle(&mut app, &mut rx).await; // the versions of each are read before the pin form
    fill(&mut app, &[(os.to_string().as_str(), "v1"), (extras.to_string().as_str(), "latest")]);
    settle(&mut app, &mut rx).await;
    shown(&app, "added 2 component(s) (takes effect on next publish)");
    let pins =
        |r: Value| -> Vec<(String, String)> { r["components"].as_array().unwrap().iter().map(|c| (s(c, "name"), s(c, "pin"))).collect() };
    let r = row(&app, "name", &id).unwrap();
    assert_eq!(pins(r.clone()), [(extras_name.clone(), "latest (v1)".into()), (os_name.clone(), "pinned v1".into())], "the list reloads");

    // Edit (e): auto publish on.
    run(&mut app, Act::Edit, r);
    settle(&mut app, &mut rx).await; // the components are read before the form
    fill(&mut app, &[("auto", "true")]);
    settle(&mut app, &mut rx).await;
    shown(&app, "saved");
    assert!(st.content_view(comp).await.unwrap().auto_publish);

    // Publish (P) the composite: Library serves the paths of the composite.
    tui_publish(&mut app, &mut rx, &id, "first").await;
    shown(&app, &format!("{id} version 1 published and active in Library"));
    let lib = st.ensure_library(&scope).await.unwrap();
    assert_eq!(svc.verify(comp, lib).await.unwrap(), Vec::<String>::new());
    let url = |repo: &str, file: &str| format!("http://127.0.0.1:8080/pulp/content/{}/{file}", base_path(&id, "library", repo));
    assert_eq!(served(&c, &url("docs", "docs/readme.txt")).await.0, 200);
    assert_eq!(served(&c, &url("os", "repodata/repomd.xml")).await.0, 200);

    // Publish (P) of the extras: the composite follows with auto publish, os stays at v1.
    tui_publish(&mut app, &mut rx, &extras_name, "again").await;
    shown(&app, &format!("{extras_name} version 2 published and active in Library; auto publish: {id} v2 published"));
    let c2 = st.versions(comp).await.unwrap().remove(0);
    assert_eq!((c2.number, c2.state.as_str()), (2, "published"));
    let used: Vec<_> = st.version_components(c2.id).await.unwrap().into_iter().map(|u| (u.name, u.number)).collect();
    assert_eq!(used, [(extras_name.clone(), 2), (os_name.clone(), 1)]);
    assert_eq!(svc.verify(comp, lib).await.unwrap(), Vec::<String>::new());
    let r = cv_row(&mut app, &mut rx, &id).await;
    assert_eq!((&r["latest"], pins(r.clone())[0].1.as_str()), (&json!("v2"), "latest (v2)"), "the list reloads");

    // Remove component (x): the extras.
    run(&mut app, Act::CvRemoveComponent, r);
    fill(&mut app, &[("component", &extras_name)]);
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("{extras_name} removed from {id}; published versions are unchanged"));
    let left: Vec<String> = st.components(comp).await.unwrap().into_iter().map(|c| c.name).collect();
    assert_eq!(left, [os_name.as_str()]);
    assert_eq!(pins(row(&app, "name", &id).unwrap()), [(os_name, "pinned v1".into())], "the list reloads");

    for v in [comp, os, extras] {
        svc.delete_view(v, &quiet, &no_progress).await.unwrap();
    }
    cleanup(&c, &[&file_repo, &rpm_repo, &file_remote, &rpm_remote]).await;
}

/// A publish that stopped after its checkpoint, through the TUI on the Versions screen: Watch (W) shows the task of
/// its pending step; Resume (R) is refused while another operator holds the lock of the view; Break lock (U) on the
/// Content Views screen, after the typed name, removes that lock; then Resume publishes the version and Library serves it.
#[tokio::test]
async fn resume_watch_and_break_lock_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, id) = (client(), unique("cvresume"));
    let (repo, remote) = synced_repo(&c, RepoType::File, &format!("{id}-docs"), "file/PULP_MANIFEST", "mirror").await;
    let (mut app, mut rx) = app().await;
    let svc = service(&app, &c);
    let (st, scope) = (svc.store.clone(), svc.scope.clone());
    let cv = st.create_cv(&scope, &id, "").await.unwrap();
    let docs = CvRepo { repo_href: repo.clone(), repo_name: "docs".into(), repo_type: RepoType::File, filters: vec![] };
    st.set_cv_repo(cv, &docs).await.unwrap();
    // v1 stopped after its checkpoint, with the sync task of the source as its pending step.
    let sync: Page<Task> = c.list(&c.api("tasks/"), &[("reserved_resources", repo.clone()), ("limit", "1".into())]).await.unwrap();
    let pending = sync.results[0].pulp_href.clone();
    let v1 = st.begin_version(cv, "", &[docs]).await.unwrap();
    let mut step = st.version_repos(v1.id).await.unwrap().remove(0);
    step.source_version = Some(latest(&c, &repo).await);
    step.pending_task = Some(pending.clone());
    st.save_version_repo(&step).await.unwrap();
    st.finish_version(v1.id, "failed", Some("stopped")).await.unwrap();

    // Watch (W): the pending step's task, followed by the Task Center.
    let r = cv_row(&mut app, &mut rx, &id).await;
    run(&mut app, Act::CvVersions, r);
    settle(&mut app, &mut rx).await;
    let v1_row = app.list.items.iter().find(|v| v["number"] == 1).cloned().unwrap();
    run(&mut app, Act::WatchTask, v1_row.clone());
    settle_tasks(&mut app, &mut rx).await;
    let Some(Modal::Text { title, lines, watch, .. }) = app.modal.take() else { panic!("no watch: {:?}", texts(&app)) };
    assert_eq!((title, watch), (format!("Watch task {id} v1: copy docs"), Some(pending.clone())));
    assert!(lines.contains(&format!("href:  {pending}")) && lines.contains(&"state: completed".to_string()), "{lines:?}");
    step.pending_task = None; // the step itself is done: Resume starts it again
    st.save_version_repo(&step).await.unwrap();

    // Resume (R) while another operator holds the lock: refused, the version stays failed.
    st.try_lock(cv, "bob@ws2", "dead-token", "publish", unix_now()).await.unwrap().unwrap();
    run(&mut app, Act::Resume, v1_row.clone());
    settle(&mut app, &mut rx).await;
    let refused = format!("Content View {id} is locked by bob@ws2 (publish) since ");
    assert!(texts(&app).iter().any(|t| t.contains(&refused)), "{:?}", texts(&app));
    assert_eq!(st.version(v1.id).await.unwrap().state, "failed");
    app.back();
    settle(&mut app, &mut rx).await;

    // Break lock (U): the holder, the typed name.
    let r = cv_row(&mut app, &mut rx, &id).await;
    run(&mut app, Act::BreakLock, r);
    settle(&mut app, &mut rx).await; // the holder is read before the confirmation
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed.as_deref(), Some(id.as_str()));
    assert!(lines[0].starts_with("Locked by bob@ws2 (publish) since "), "{lines:?}");
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("lock on {id} broken"));
    assert_eq!(st.lock_holder(cv).await.unwrap(), None);

    // Resume (R): v1 is published and active in Library.
    let r = cv_row(&mut app, &mut rx, &id).await;
    run(&mut app, Act::CvVersions, r);
    settle(&mut app, &mut rx).await;
    run(&mut app, Act::Resume, v1_row.clone());
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("{id} v1 published"));
    assert_eq!(st.version(v1.id).await.unwrap().state, "published");
    let lib = st.ensure_library(&scope).await.unwrap();
    assert_eq!(st.binding(lib, cv).await.unwrap().unwrap().active(), Some(v1.id));
    assert!(svc.verify(cv, lib).await.unwrap().is_empty());
    let readme = format!("http://127.0.0.1:8080/pulp/content/{}/docs/readme.txt", base_path(&id, "library", "docs"));
    assert_eq!(served(&c, &readme).await.0, 200);
    assert_eq!(app.list.items.iter().find(|v| v["number"] == 1).unwrap()["state"], "published", "the list reloads");
    // Watch (W) of a published version: no step is pending.
    run(&mut app, Act::WatchTask, v1_row);
    settle(&mut app, &mut rx).await;
    assert!(texts(&app).last().unwrap().starts_with("v1 has no copy or publication task now."), "{:?}", texts(&app));

    svc.delete_view(cv, &quiet, &no_progress).await.unwrap();
    cleanup(&c, &[&repo, &remote]).await;
}

/// Answer the open path input with `path`.
fn type_path(app: &mut App, path: &str) {
    let Some(Modal::Input { on_done: Some(done), .. }) = app.modal.take() else { panic!("no path input: {:?}", texts(app)) };
    done(app, path.into());
}

/// Environment Edit (relabel) and the Content View definitions export and import, through the TUI: a view served in
/// Dev; Edit of Dev's label: the impact names the path that moves, the typed name; Pulp serves at the new path only.
/// Export definitions (E on Servers) writes the view to a file; Import definitions (I) in a session with an empty
/// database creates it there, with the repository of this server.
#[tokio::test]
async fn environment_relabel_and_definitions_export_import_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, id) = (client(), unique("cvlbtui"));
    let (repo, remote) = synced_repo(&c, RepoType::File, &format!("{id}-docs"), "file/PULP_MANIFEST", "mirror").await;
    let (mut app, mut rx) = app().await;
    let svc = service(&app, &c);
    let (st, scope) = (svc.store.clone(), svc.scope.clone());
    let cv = st.create_cv(&scope, &id, "").await.unwrap();
    let docs = CvRepo { repo_href: repo.clone(), repo_name: "docs".into(), repo_type: RepoType::File, filters: vec![] };
    st.set_cv_repo(cv, &docs).await.unwrap();
    let lib = st.ensure_library(&scope).await.unwrap();
    let dev = st.create_env(&scope, "Dev", lib, false).await.unwrap();
    tui_publish(&mut app, &mut rx, &id, "").await;
    tui_promote(&mut app, &mut rx, &id, Act::Promote, "1", "Dev").await;
    let readme = |label: &str| format!("http://127.0.0.1:8080/pulp/content/{}/docs/readme.txt", base_path(&id, label, "docs"));
    assert_eq!(served(&c, &readme("dev")).await.0, 200);

    // Edit (e) of Dev: label dev2.
    open_list(&mut app, &mut rx, Kind::Environments, "").await;
    let e = row(&app, "name", "Dev").unwrap();
    run(&mut app, Act::Edit, e);
    fill(&mut app, &[("label", "dev2")]);
    settle(&mut app, &mut rx).await; // the served paths are read before the confirmation
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed.as_deref(), Some("Dev"));
    let moved = format!("  /{} -> /{}", base_path(&id, "dev", "docs"), base_path(&id, "dev2", "docs"));
    assert_eq!(lines[..2], ["Label dev -> dev2. Served paths move:".to_string(), moved]);
    settle(&mut app, &mut rx).await;
    shown(&app, "label of Dev is dev2");
    assert!(matches!(app.modal.take(), Some(Modal::Text { title, .. }) if title == "Label of Dev"));
    assert_eq!(row(&app, "name", "Dev").unwrap()["label"], "dev2", "the list reloads");
    assert_eq!((served(&c, &readme("dev2")).await.0, served(&c, &readme("dev")).await.0), (200, 404));
    assert!(svc.verify(cv, dev).await.unwrap().is_empty());

    // Export definitions (E on Servers) to a file.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("cv.json").display().to_string();
    open_list(&mut app, &mut rx, Kind::Profiles, "").await;
    let p = row(&app, "name", "pulp1").unwrap();
    run(&mut app, Act::ExportCv, p.clone());
    type_path(&mut app, &file);
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("exported 1 content view(s) to {file} (no secrets, no Pulp state)"));
    assert!(std::fs::read_to_string(&file).unwrap().contains(&id));

    // Import definitions (I) in another session, with its own empty database.
    let (mut other, mut orx) = common::app().await;
    open_list(&mut other, &mut orx, Kind::Profiles, "").await;
    run(&mut other, Act::ImportCv, p);
    type_path(&mut other, &file);
    settle(&mut other, &mut orx).await;
    assert!(texts(&other).iter().any(|t| t.starts_with("imported 1 content view(s), ")), "{:?}", texts(&other));
    let ost = other.cv_store.clone().unwrap();
    let views = ost.content_views(&scope).await.unwrap();
    let view = views.iter().find(|v| v.name == id).unwrap_or_else(|| panic!("not imported: {views:?}"));
    let hrefs: Vec<String> = ost.cv_repos(view.id).await.unwrap().into_iter().map(|r| r.repo_href).collect();
    assert_eq!(hrefs, [repo.as_str()], "mapped to the repository of this server by its name");

    svc.delete_view(cv, &quiet, &no_progress).await.unwrap();
    cleanup(&c, &[&repo, &remote]).await;
}

/// Rename the distributions of view `id` in Dev that `rows` record (`suffix` "" gives back the pulp-tui name).
async fn rename_dev(c: &PulpClient, id: &str, rows: &[pulp_tui::store::BindingRepo], suffix: &str) {
    for b in rows {
        let name = format!("{}{suffix}", pulp_tui::cv::distribution_name(id, "Dev", &b.repo_name));
        let m = c.patch(&b.distribution, &json!({ "name": name })).await.unwrap();
        ops::settle(c, m, &no_progress).await.unwrap();
    }
}

/// Dev serves v1. The promotion of v2 ends partial (docs serves v2; the distribution of more was renamed outside
/// pulp-tui), then the one of v3 fails (docs renamed too): docs still serves v2. The Versions screen shows v2 in Dev,
/// and its Delete (d) is refused before the typed confirmation.
#[tokio::test]
async fn a_version_that_dev_serves_in_part_is_not_deleted_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, id) = (client(), unique("cvpart"));
    let (mut app, mut rx) = app().await;
    let svc = service(&app, &c);
    let (st, scope) = (svc.store.clone(), svc.scope.clone());
    let cv = st.create_cv(&scope, &id, "").await.unwrap();
    let mut sources = vec![];
    for name in ["docs", "more"] {
        let (repo, remote) = synced_repo(&c, RepoType::File, &format!("{id}-{name}"), "file/PULP_MANIFEST", "mirror").await;
        st.set_cv_repo(cv, &CvRepo { repo_href: repo.clone(), repo_name: name.into(), repo_type: RepoType::File, filters: vec![] })
            .await
            .unwrap();
        sources.extend([repo, remote]);
    }
    let lib = st.ensure_library(&scope).await.unwrap();
    let dev = st.create_env(&scope, "Dev", lib, false).await.unwrap();
    // Each version has another docs snapshot (its filter changes); more keeps one publication.
    let publish = async |filters: &str| {
        let docs = CvRepo {
            repo_href: sources[0].clone(),
            repo_name: "docs".into(),
            repo_type: RepoType::File,
            filters: parse_rules(filters, RepoType::File).unwrap(),
        };
        st.set_cv_repo(cv, &docs).await.unwrap();
        svc.publish(cv, "", "", &quiet, &no_progress).await.unwrap().0
    };
    let v1 = publish("").await;
    svc.promote(cv, v1.id, dev, Mode::Promote, &quiet, &no_progress).await.unwrap();
    let rows = st.binding_repos(dev, cv).await.unwrap();
    let more: Vec<_> = rows.iter().filter(|b| b.repo_name == "more").cloned().collect();
    let v2 = publish("exclude path=isos/*").await;
    rename_dev(&c, &id, &more, "-moved").await;
    let e = svc.promote(cv, v2.id, dev, Mode::Promote, &quiet, &no_progress).await.unwrap_err().to_string();
    assert!(e.starts_with(&format!("{id} in Dev: partial: more: ")), "{e}");
    let v3 = publish("exclude path=docs/*").await;
    rename_dev(&c, &id, &rows, "-moved").await;
    let e = svc.promote(cv, v3.id, dev, Mode::Promote, &quiet, &no_progress).await.unwrap_err().to_string();
    assert!(e.starts_with(&format!("{id} in Dev: failed: ")), "{e}");
    assert_eq!(cv_row(&mut app, &mut rx, &id).await["bound"], "Library:v3  Dev:v1 (v3 failed)");

    // Versions (v): v2 is served in part; its Delete (d) is refused.
    let r = cv_row(&mut app, &mut rx, &id).await;
    run(&mut app, Act::CvVersions, r);
    settle(&mut app, &mut rx).await;
    let v2_row = app.list.items.iter().find(|v| v["number"] == 2).cloned().unwrap();
    assert_eq!(v2_row["environments"], "Dev (served in part)");
    run(&mut app, Act::Delete, v2_row);
    settle(&mut app, &mut rx).await;
    assert!(app.modal.is_none(), "no delete confirmation");
    shown(&app, &format!("read what deleting {id} v2 removes: version 2 is bound to Dev (served in part)"));
    assert_eq!(st.version(v2.id).await.unwrap().state, "published");

    svc.delete_view(cv, &quiet, &no_progress).await.unwrap();
    cleanup(&c, &sources.iter().map(String::as_str).collect::<Vec<_>>()).await;
}

/// Resume (R) of the newest component version whose auto publish never ran (the client stopped first): the composite
/// that follows it publishes, and Library serves it. A second Resume has nothing left to do.
#[tokio::test]
async fn resume_publishes_a_lagging_composite_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, id) = (client(), unique("cvlag"));
    let extras_name = format!("{id}-extras");
    let (repo, remote) = synced_repo(&c, RepoType::File, &format!("{id}-docs"), "file/PULP_MANIFEST", "mirror").await;
    let (mut app, mut rx) = app().await;
    let svc = service(&app, &c);
    let (st, scope) = (svc.store.clone(), svc.scope.clone());
    let extras = st.create_cv(&scope, &extras_name, "").await.unwrap();
    let docs = CvRepo { repo_href: repo.clone(), repo_name: "docs".into(), repo_type: RepoType::File, filters: vec![] };
    st.set_cv_repo(extras, &docs).await.unwrap();
    svc.publish(extras, "", "", &quiet, &no_progress).await.unwrap();
    let comp = st.create_composite(&scope, &id, "").await.unwrap();
    st.set_component(comp, extras, None).await.unwrap();
    st.set_auto_publish(comp, true).await.unwrap();
    svc.publish(comp, "", "", &quiet, &no_progress).await.unwrap(); // composite v1 uses extras v1
    // extras v2 without the ISO; the client stopped before the auto publish.
    st.set_cv_repo(extras, &CvRepo { filters: parse_rules("exclude path=isos/*", RepoType::File).unwrap(), ..docs }).await.unwrap();
    let (e2, _) = svc.publish(extras, "", "", &quiet, &no_progress).await.unwrap();

    // Resume (R) of extras v2 on its Versions screen: the composite publishes with extras v2.
    let r = cv_row(&mut app, &mut rx, &extras_name).await;
    run(&mut app, Act::CvVersions, r);
    settle(&mut app, &mut rx).await;
    let v2_row = app.list.items.iter().find(|v| v["number"] == 2).cloned().unwrap();
    run(&mut app, Act::Resume, v2_row.clone());
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("{extras_name} v2 published; auto publish: {id} v2 published"));
    let c2 = st.versions(comp).await.unwrap().remove(0);
    assert_eq!((c2.number, c2.state.as_str()), (2, "published"));
    let used: Vec<i64> = st.version_components(c2.id).await.unwrap().into_iter().map(|u| u.version_id).collect();
    assert_eq!(used, [e2.id]);
    let lib = st.ensure_library(&scope).await.unwrap();
    assert_eq!(st.binding(lib, comp).await.unwrap().unwrap().active(), Some(c2.id));
    assert!(svc.verify(comp, lib).await.unwrap().is_empty());
    let url = |file: &str| format!("http://127.0.0.1:8080/pulp/content/{}/{file}", base_path(&id, "library", "docs"));
    // The manifest switches at once (old files can stay downloadable for DISTRIBUTED_PUBLICATION_RETENTION_PERIOD).
    let (status, manifest) = served(&c, &url("PULP_MANIFEST")).await;
    let manifest = String::from_utf8(manifest).unwrap();
    assert!(status == 200 && manifest.contains("docs/readme.txt") && !manifest.contains("isos/boot.iso"), "{status} {manifest}");

    // A second Resume: no third composite version.
    run(&mut app, Act::Resume, v2_row);
    settle(&mut app, &mut rx).await;
    shown(&app, "version is already published");
    assert_eq!(st.versions(comp).await.unwrap().len(), 2);

    for v in [comp, extras] {
        svc.delete_view(v, &quiet, &no_progress).await.unwrap();
    }
    cleanup(&c, &[&repo, &remote]).await;
}

/// Environment Edit (e) to a label that a foreign distribution has a path below: refused before the typed
/// confirmation, with the distribution named. The label stays.
#[tokio::test]
async fn relabel_to_an_overlapping_label_is_refused_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, id) = (client(), unique("cvov"));
    let body = json!({ "name": format!("{id}-foreign"), "base_path": format!("{id}/foo") });
    let foreign = create(&c, RepoType::File.distributions(), body, "/distributions/").await;
    let (mut app, mut rx) = app().await;
    let (st, scope) = (app.cv_store.clone().unwrap(), app.conn.as_ref().unwrap().scope.clone());
    let lib = st.ensure_library(&scope).await.unwrap();
    st.create_env(&scope, "Dev", lib, false).await.unwrap();

    open_list(&mut app, &mut rx, Kind::Environments, "").await;
    let e = row(&app, "name", "Dev").unwrap();
    run(&mut app, Act::Edit, e);
    fill(&mut app, &[("label", &id)]);
    settle(&mut app, &mut rx).await;
    assert!(app.modal.is_none(), "no confirmation: {:?}", texts(&app));
    shown(
        &app,
        &format!(
            "read served paths of Dev: label {id} is refused: distribution {id}-foreign has base path /{id}/foo, the label or \
             below it (the paths below a label are its environment's: a promotion there could overlap it)"
        ),
    );
    let dev = st.environments(&scope).await.unwrap().into_iter().find(|e| e.name == "Dev").unwrap();
    assert_eq!(dev.label, "dev");

    cleanup(&c, &[&foreign]).await;
}

/// A partial promotion through the TUI: a foreign distribution has the Dev path of more. The preview warns; the
/// promotion serves docs and fails for more; Content Views, Versions, Environments and Verify (D) show the attempt, and
/// Verify names a new promotion. Once the path is free, that promotion completes.
#[tokio::test]
async fn a_partial_promotion_and_what_the_screens_show_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, id) = (client(), unique("cvpp"));
    let (mut app, mut rx) = app().await;
    let svc = service(&app, &c);
    let (st, scope) = (svc.store.clone(), svc.scope.clone());
    let cv = st.create_cv(&scope, &id, "").await.unwrap();
    let mut sources = vec![];
    for name in ["docs", "more"] {
        let (repo, remote) = synced_repo(&c, RepoType::File, &format!("{id}-{name}"), "file/PULP_MANIFEST", "mirror").await;
        st.set_cv_repo(cv, &CvRepo { repo_href: repo.clone(), repo_name: name.into(), repo_type: RepoType::File, filters: vec![] })
            .await
            .unwrap();
        sources.extend([repo, remote]);
    }
    let lib = st.ensure_library(&scope).await.unwrap();
    let dev = st.create_env(&scope, "Dev", lib, false).await.unwrap();
    tui_publish(&mut app, &mut rx, &id, "").await;
    let (foreign_name, more_path) = (format!("{id}-foreign"), base_path(&id, "dev", "more"));
    let body = json!({ "name": foreign_name, "base_path": more_path });
    let foreign = create(&c, RepoType::File.distributions(), body, "/distributions/").await;

    // Promote (M) v1 to Dev: the preview warns for more; docs is served, more fails.
    let lines = tui_promote(&mut app, &mut rx, &id, Act::Promote, "1", "Dev").await;
    let more_dist = pulp_tui::cv::distribution_name(&id, "Dev", "more");
    let warn = format!(
        "  more (File): new distribution {more_dist} at /{more_path}; distribution {foreign_name} has that base path, so the \
         promotion fails for it"
    );
    assert!(lines.contains(&warn), "{lines:?}");
    assert!(texts(&app).iter().any(|t| t.starts_with(&format!("{id} in Dev: partial: more: "))), "{:?}", texts(&app));
    let b = st.binding(dev, cv).await.unwrap().unwrap();
    assert_eq!((b.served, b.attempt.map(|a| a.state)), (None, Some("partial".to_string())));
    let readme = format!("http://127.0.0.1:8080/pulp/content/{}/docs/readme.txt", base_path(&id, "dev", "docs"));
    assert_eq!(served(&c, &readme).await.0, 200, "docs is served");

    // The screens.
    let r = cv_row(&mut app, &mut rx, &id).await;
    assert_eq!(r["bound"], "Library:v1  Dev:- (v1 partial)");
    run(&mut app, Act::CvVersions, r);
    settle(&mut app, &mut rx).await;
    assert_eq!(app.list.items.iter().find(|v| v["number"] == 1).unwrap()["environments"], "Library, Dev (partial)");
    open_list(&mut app, &mut rx, Kind::Environments, "").await;
    assert_eq!(row(&app, "name", "Dev").unwrap()["bound"], format!("{id}:- (v1 partial)"));
    let r = cv_row(&mut app, &mut rx, &id).await;
    run(&mut app, Act::CvDrift, r);
    settle(&mut app, &mut rx).await;
    let Some(Modal::Text { lines, .. }) = app.modal.take() else { panic!("no verify result: {:?}", texts(&app)) };
    assert_eq!(
        lines,
        [
            "Library: v1 OK - served metadata match; RPM/Deb: also the smallest package",
            "Dev: nothing (v1 partial) DRIFT",
            "    promotion of v1 is partial",
            "",
            "Dev: promote v1 again (M) to reconcile (idempotent).",
        ]
    );

    // The path is free again: Promote (M) completes.
    cleanup(&c, &[&foreign]).await;
    tui_promote(&mut app, &mut rx, &id, Act::Promote, "1", "Dev").await;
    shown(&app, &format!("{id} v1 active in Dev; served metadata verified"));
    assert_eq!(cv_row(&mut app, &mut rx, &id).await["bound"], "Library:v1  Dev:v1");

    svc.delete_view(cv, &quiet, &no_progress).await.unwrap();
    cleanup(&c, &sources.iter().map(String::as_str).collect::<Vec<_>>()).await;
}
