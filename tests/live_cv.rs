//! Content View workflows against a live Pulp: multi-repository snapshots with filters,
//! numbered versions, promotion through environments, rollback, served-content
//! verification, retention protection, partial failure + recovery, and resuming a
//! publish whose Pulp task was submitted before the client stopped, objects owned by
//! another database, and a composite view of two component views.

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
    let v1 = svc.publish(cv, "first", &quiet, &no_progress).await.unwrap();
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
        .fetch_content(&format!("http://localhost:8080/pulp/content/{}/docs/readme.txt", base_path(&id, "dev", "docs")), 1 << 20)
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
    assert_eq!(store.binding(lib, cv).await.unwrap().unwrap().version_id, v2.id);
    svc.promote(cv, v2.id, dev, Mode::Promote, &quiet, &no_progress).await.unwrap();
    assert!(svc.verify(cv, dev).await.unwrap().is_empty());
    let cmp = svc.compare_versions(v1.id, v2.id).await.unwrap().join("\n");
    assert!(cmp.contains("+ fixture-alpha-1.1-1.noarch") && cmp.contains("+ fixture-gamma-0.5-1.noarch"), "{cmp}");

    // Rollback Dev to v1: bindings change, history stays, served content is v1 again.
    svc.promote(cv, v1.id, dev, Mode::Rollback, &quiet, &no_progress).await.unwrap();
    assert_eq!(store.binding(dev, cv).await.unwrap().unwrap().version_id, v1.id);
    assert!(svc.verify(cv, dev).await.unwrap().is_empty());
    assert_eq!(store.version(v2.id).await.unwrap().state, "published");

    // Retention protection: referenced snapshots are listed; bound versions cannot be deleted.
    let protected = store.protected_versions(&profile).await.unwrap();
    assert!(protected.contains(os_v1.derived_version.as_ref().unwrap()));
    assert!(svc.delete_version(v1.id, &quiet, &no_progress).await.unwrap_err().to_string().contains("bound to Dev"));

    // Advisory filtering: excluding the package referenced by the advisory drops it.
    os.filters = parse_rules("exclude name=*-debuginfo; exclude name=fixture-beta", RepoType::Rpm).unwrap();
    store.set_cv_repo(cv, &os).await.unwrap();
    let v3 = svc.publish(cv, "no beta", &quiet, &no_progress).await.unwrap();
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
    assert_eq!(store.binding(qa, cv).await.unwrap().unwrap().state, "partial");
    assert!(!svc.verify(cv, qa).await.unwrap().is_empty());
    cleanup(&c, &[&blocker]).await;
    svc.promote(cv, v3.id, qa, Mode::Promote, &quiet, &no_progress).await.unwrap(); // idempotent retry
    assert_eq!(store.binding(qa, cv).await.unwrap().unwrap().state, "active");
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
    let v1 = svc.publish(cv, "", &quiet, &no_progress).await.unwrap();
    let lib = store.ensure_library(&profile).await.unwrap();
    let dev = store.create_env(&profile, "Dev", lib, false).await.unwrap();
    svc.promote(cv, v1.id, dev, Mode::Promote, &quiet, &no_progress).await.unwrap();
    let readme = |label: &str| format!("http://localhost:8080/pulp/content/{}/docs/readme.txt", base_path(&id, label, "docs"));
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
    let v = a.publish(a_cv, "", &quiet, &no_progress).await.unwrap();
    let derived = a.store.version_repos(v.id).await.unwrap().remove(0).derived_repo.unwrap();
    let lib = a.store.ensure_library(&profile).await.unwrap();
    let dist = a.store.binding_repos(lib, a_cv).await.unwrap().remove(0).distribution;
    assert_eq!(label(derived.clone()).await.as_deref(), Some(a.store.db_id()));
    assert_eq!(label(dist.clone()).await.as_deref(), Some(a.store.db_id()));

    // B refuses the foreign derived repository, and writes nothing.
    let err = format!("{:#}", b.publish(b_cv, "", &quiet, &no_progress).await.unwrap_err());
    let foreign = format!("is owned by another pulp-tui database ({})", a.store.db_id());
    assert!(err.contains(&foreign), "{err}");
    assert_eq!(label(derived.clone()).await.as_deref(), Some(a.store.db_id()));

    // Once B knows A's id as an alias, B adopts both objects.
    b.store.add_alias(a.store.db_id()).await.unwrap();
    b.publish(b_cv, "", &quiet, &no_progress).await.unwrap();
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
    let os_v1 = svc.publish(os, "", &quiet, &no_progress).await.unwrap();
    let extras_v1 = svc.publish(extras, "", &quiet, &no_progress).await.unwrap();

    // v1: os pinned, extras latest; nothing is copied, and Library serves the composite's own paths.
    let comp = store.create_composite(&profile, &id, "").await.unwrap();
    store.set_component(comp, os, Some(os_v1.id)).await.unwrap();
    store.set_component(comp, extras, None).await.unwrap();
    store.set_auto_publish(comp, true).await.unwrap();
    let c1 = svc.publish(comp, "first", &quiet, &no_progress).await.unwrap();
    assert_eq!((c1.number, c1.state.as_str()), (1, "published"));
    let lib = store.ensure_library(&profile).await.unwrap();
    assert_eq!(svc.verify(comp, lib).await.unwrap(), Vec::<String>::new());
    assert!(svc.verify(extras, lib).await.unwrap().is_empty(), "one publication, served by two distributions");
    let url = |label: &str, repo: &str, file: &str| format!("http://localhost:8080/pulp/content/{}/{file}", base_path(&id, label, repo));
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
    let extras_v2 = svc.publish(extras, "update", &quiet, &no_progress).await.unwrap();
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
    assert_eq!(store.binding(dev, comp).await.unwrap().unwrap().version_id, c1.id);
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
