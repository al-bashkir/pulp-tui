//! Content View workflows against a live Pulp: multi-repository snapshots with filters,
//! numbered versions, promotion through environments, rollback, served-content
//! verification, retention protection, partial failure + recovery, and resuming a
//! publish whose Pulp task was submitted before the client stopped.

mod common;

use common::*;
use pulp_tui::cv::{CvService, Mode, base_path};
use pulp_tui::filters::parse_rules;
use pulp_tui::pulp::models::{RepoVersion, Task};
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{Mutation, RepoType};
use pulp_tui::store::{CvRepo, Store};
use serde_json::json;
use std::sync::{Arc, Mutex};

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

    let store = Arc::new(Store::open_in_memory().unwrap());
    let svc = CvService { client: c.clone(), store: store.clone(), prefix: id.clone() };
    let profile = c.profile().to_string();
    let cv = store.create_cv(&profile, &id, "test view").unwrap();
    let mut os = CvRepo {
        repo_href: rpm_repo.clone(),
        repo_name: "os".into(),
        repo_type: RepoType::Rpm,
        filters: parse_rules("exclude name=*-debuginfo", RepoType::Rpm).unwrap(),
    };
    store.set_cv_repo(cv, &os).unwrap();
    let docs = CvRepo {
        repo_href: file_repo.clone(),
        repo_name: "docs".into(),
        repo_type: RepoType::File,
        filters: parse_rules("exclude path=isos/*", RepoType::File).unwrap(),
    };
    store.set_cv_repo(cv, &docs).unwrap();

    // Preview shows the exact selection.
    let preview = svc.preview(cv, None).await.unwrap().join("\n");
    assert!(preview.contains("+ fixture-alpha-1.0-1.noarch"), "{preview}");
    assert!(!preview.contains("+ fixture-beta-debuginfo"), "{preview}");
    assert!(preview.contains("+ docs/readme.txt"), "{preview}");
    assert!(!preview.contains("boot.iso"), "{preview}");

    // v1: publish auto-binds Library and is verified against served content.
    let v1 = svc.publish(cv, "first", &quiet, &no_progress).await.unwrap();
    assert_eq!((v1.number, v1.state.as_str()), (1, "published"));
    let lib = store.ensure_library(&profile).unwrap();
    assert_eq!(svc.verify(cv, lib).await.unwrap(), Vec::<String>::new());
    let vrs = store.version_repos(v1.id).unwrap();
    let os_v1 = vrs.iter().find(|v| v.repo().repo_name == "os").unwrap();
    let rv: RepoVersion = c.get(os_v1.derived_version.as_deref().unwrap(), &[]).await.unwrap();
    assert_eq!(rv.content_summary.present["rpm.package"].count, 2);
    assert_eq!(rv.content_summary.present["rpm.advisory"].count, 1, "advisory kept: its package is selected");
    assert_eq!(vrs.iter().find(|v| v.repo().repo_name == "docs").unwrap().content_count, Some(2));

    // Environments: Library -> Dev -> Prod. Promotion must follow the path.
    let dev = store.create_env(&profile, "Dev", lib, false).unwrap();
    let prod = store.create_env(&profile, "Prod", dev, true).unwrap();
    let err = svc.promote(cv, v1.id, prod, Mode::Promote, &quiet, &no_progress).await.unwrap_err();
    assert!(err.to_string().contains("not active in Dev"), "{err}");
    svc.promote(cv, v1.id, dev, Mode::Promote, &quiet, &no_progress).await.unwrap();
    assert_eq!(svc.verify(cv, dev).await.unwrap(), Vec::<String>::new());
    let dev_docs = c
        .fetch_content(&format!("http://localhost:8080/pulp/content/{}/docs/readme.txt", base_path(&id, &id, "Dev", "docs")), 1 << 20)
        .await
        .unwrap();
    assert_eq!(dev_docs.0, 200);

    // Upstream update -> v2 (Library follows), promote to Dev, compare.
    let m = c.patch(&rpm_remote, &json!({ "url": format!("{}/rpm2/", upstream()) })).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let m = ops::sync(&c, RepoType::Rpm, &rpm_repo, None, "additive").await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let v2 = svc.publish(cv, "upstream update", &quiet, &no_progress).await.unwrap();
    assert_eq!(v2.number, 2);
    assert_eq!(store.binding(lib, cv).unwrap().unwrap().version_id, v2.id);
    svc.promote(cv, v2.id, dev, Mode::Promote, &quiet, &no_progress).await.unwrap();
    assert!(svc.verify(cv, dev).await.unwrap().is_empty());
    let cmp = svc.compare_versions(v1.id, v2.id).await.unwrap().join("\n");
    assert!(cmp.contains("+ fixture-alpha-1.1-1.noarch") && cmp.contains("+ fixture-gamma-0.5-1.noarch"), "{cmp}");

    // Rollback Dev to v1: bindings change, history stays, served content is v1 again.
    svc.promote(cv, v1.id, dev, Mode::Rollback, &quiet, &no_progress).await.unwrap();
    assert_eq!(store.binding(dev, cv).unwrap().unwrap().version_id, v1.id);
    assert!(svc.verify(cv, dev).await.unwrap().is_empty());
    assert_eq!(store.version(v2.id).unwrap().state, "published");

    // Retention protection: referenced snapshots are listed; bound versions cannot be deleted.
    let protected = store.protected_versions(&profile).unwrap();
    assert!(protected.contains(os_v1.derived_version.as_ref().unwrap()));
    assert!(svc.delete_version(v1.id, &quiet, &no_progress).await.unwrap_err().to_string().contains("bound to Dev"));

    // Advisory filtering: excluding the package referenced by the advisory drops it.
    os.filters = parse_rules("exclude name=*-debuginfo; exclude name=fixture-beta", RepoType::Rpm).unwrap();
    store.set_cv_repo(cv, &os).unwrap();
    let v3 = svc.publish(cv, "no beta", &quiet, &no_progress).await.unwrap();
    let os_v3 = store.version_repos(v3.id).unwrap().into_iter().find(|v| v.repo().repo_name == "os").unwrap();
    let rv: RepoVersion = c.get(os_v3.derived_version.as_deref().unwrap(), &[]).await.unwrap();
    assert!(!rv.content_summary.present.contains_key("rpm.advisory"));
    assert_eq!(rv.content_summary.present["rpm.package"].count, 3);

    // Partial failure: a foreign distribution occupies the File base path of QA.
    let qa = store.create_env(&profile, "QA", lib, false).unwrap();
    let blocker = create(
        &c,
        RepoType::File.distributions(),
        json!({ "name": format!("{id}-blocker"), "base_path": base_path(&id, &id, "QA", "docs") }),
        "/distributions/",
    )
    .await;
    let err = svc.promote(cv, v3.id, qa, Mode::Promote, &quiet, &no_progress).await.unwrap_err();
    assert!(err.to_string().contains("partial"), "{err}");
    assert_eq!(store.binding(qa, cv).unwrap().unwrap().state, "partial");
    assert!(!svc.verify(cv, qa).await.unwrap().is_empty());
    cleanup(&c, &[&blocker]).await;
    svc.promote(cv, v3.id, qa, Mode::Promote, &quiet, &no_progress).await.unwrap(); // idempotent retry
    assert_eq!(store.binding(qa, cv).unwrap().unwrap().state, "active");
    assert!(svc.verify(cv, qa).await.unwrap().is_empty());

    // Resume: the copy task was submitted, then the client "crashed" before waiting.
    os.filters = vec![];
    store.set_cv_repo(cv, &os).unwrap();
    let v4 = store.begin_version(cv, "resume", &store.cv_repos(cv).unwrap()).unwrap();
    let mut pending = String::new();
    for mut vr in store.version_repos(v4.id).unwrap() {
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
        store.save_version_repo(&vr).unwrap();
    }
    let seen: Mutex<Vec<Task>> = Mutex::new(vec![]);
    let on = |t: &Task| seen.lock().unwrap().push(t.clone());
    let v4 = svc.resume(v4.id, &quiet, &on).await.unwrap();
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
    let os_v4 = store.version_repos(v4.id).unwrap().into_iter().find(|v| v.repo().repo_name == "os").unwrap();
    assert_eq!(os_v4.content_count, Some(6), "5 packages + advisory");

    // Unbound versions can be deleted; their Pulp objects go away unless shared.
    svc.delete_version(v2.id, &quiet, &no_progress).await.unwrap();
    assert_eq!(store.version(v2.id).unwrap().state, "deleted");

    // Full cleanup through the service removes distributions and derived repositories.
    svc.delete_view(cv, &quiet, &no_progress).await.unwrap();
    assert!(c.get::<serde_json::Value>(os_v1.derived_repo.as_deref().unwrap(), &[]).await.unwrap_err().is_not_found());
    cleanup(&c, &[&rpm_repo, &file_repo, &rpm_remote, &file_remote]).await;
}
