//! Content features against a live Pulp: alternate content sources (RPM, File, Deb), RPM comps and
//! advisory uploads, module content made through the TUI, version repair, reclaim space.

mod common;

use common::*;
use pulp_tui::pulp::models::{RepoVersion, Task};
use pulp_tui::pulp::ops;
use pulp_tui::pulp::{Mutation, RepoType, rpm};
use pulp_tui::tui::actions::run;
use pulp_tui::tui::kinds::{Act, Kind};
use serde_json::{Value, json};
use std::path::PathBuf;

/// A fixture of `tests/fixtures/` with `from` replaced by `to` (a unique id), as a file in a temporary directory.
fn fixture_as(dir: &tempfile::TempDir, file: &str, from: &str, to: &str) -> PathBuf {
    let text = std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(file)).unwrap();
    let path = dir.path().join(file);
    std::fs::write(&path, text.replace(from, to)).unwrap();
    path
}

/// The new version of a finished upload task (`None`: Pulp made none).
async fn settled_version(c: &pulp_tui::pulp::PulpClient, m: Mutation) -> Option<String> {
    let t: Option<Task> = ops::settle(c, m, &ops::no_progress).await.unwrap();
    t.and_then(|t| t.created_matching("/versions/"))
}

/// Comps and advisory uploads with `repository` make a new version that the task reports; `replace` removes
/// the comps units of the repository first.
#[tokio::test]
async fn rpm_comps_and_advisory_uploads_make_new_versions() {
    if !live_enabled() {
        return;
    }
    let (c, t, name, dir) = (client(), RepoType::Rpm, unique("it-upmeta"), tempfile::tempdir().unwrap());
    let repo = create(&c, t.repositories(), json!({ "name": name }), "/repositories/").await;
    let groups = async |v: &str| -> Vec<String> {
        let page: Value = c.get(&c.api("content/rpm/packagegroups/"), &[("repository_version", v.into())]).await.unwrap();
        page["results"].as_array().unwrap().iter().map(|g| g["id"].as_str().unwrap().to_string()).collect()
    };
    let comps = fixture_as(&dir, "comps.xml", "pulp-tui-fixture", &format!("{name}-a"));
    let v1 = settled_version(&c, rpm::upload_comps(&c, &comps, &repo, false).await.unwrap()).await.expect("a new version");
    assert_eq!(groups(&v1).await, [format!("{name}-a")]);
    let comps = fixture_as(&dir, "comps.xml", "pulp-tui-fixture", &format!("{name}-b"));
    let v2 = settled_version(&c, rpm::upload_comps(&c, &comps, &repo, true).await.unwrap()).await.expect("a new version");
    assert_eq!(groups(&v2).await, [format!("{name}-b")], "replace removed group a");

    let id = format!("{name}:0001");
    let advisory = fixture_as(&dir, "advisory.json", "PULP-TUI-2026:0002", &id);
    let v3 = settled_version(&c, rpm::upload_advisory(&c, &advisory, &repo).await.unwrap()).await.expect("a new version");
    assert!(v3.ends_with("/versions/3/"), "{v3}");
    let page: Value = c.get(&c.api(rpm::ADVISORIES), &[("repository_version", v3.clone()), ("fields", "id".into())]).await.unwrap();
    assert_eq!(page["results"], json!([{ "id": id }]));
    let rv: RepoVersion = c.get(&v3, &[]).await.unwrap();
    assert_eq!(rv.content_summary.present["rpm.packagegroup"].count, 1, "the advisory keeps the group");
    cleanup(&c, &[&repo]).await;
}

/// The same advisory with a newer `updated` replaces the older one (pulp_rpm `resolve_advisory_conflict`): another
/// unit under the same id, which the Versions compare shows as changed.
#[tokio::test]
async fn compare_shows_an_updated_advisory_as_changed() {
    if !live_enabled() {
        return;
    }
    let (c, name, dir) = (client(), unique("it-advcmp"), tempfile::tempdir().unwrap());
    let repo = create(&c, RepoType::Rpm.repositories(), json!({ "name": name }), "/repositories/").await;
    let id = format!("{name}:0001");
    let first = fixture_as(&dir, "advisory.json", "PULP-TUI-2026:0002", &id);
    let v1 = settled_version(&c, rpm::upload_advisory(&c, &first, &repo).await.unwrap()).await.expect("a new version");
    let newer = dir.path().join("newer.json");
    let text = std::fs::read_to_string(&first).unwrap().replace("\"updated\": \"2026-01-02", "\"updated\": \"2026-01-03");
    std::fs::write(&newer, text).unwrap();
    let v2 = settled_version(&c, rpm::upload_advisory(&c, &newer, &repo).await.unwrap()).await.expect("a new version");
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Versions(Some((repo.clone(), name.clone())));
    app.list.marked = [v1].into();
    run(&mut app, Act::Compare, json!({ "pulp_href": v2, "number": 2 }));
    settle(&mut app, &mut rx).await;
    let Some(pulp_tui::tui::app::Modal::Text { lines, .. }) = &app.modal else { panic!("no comparison: {:?}", texts(&app)) };
    assert!(lines[0].ends_with(": +0 added, -0 removed, ~1 changed"), "{lines:?}");
    assert!(lines.contains(&format!("~ advisory {id}")), "{lines:?}");
    cleanup(&c, &[&repo]).await;
}

/// Pulp takes only an on_demand remote for an ACS. A refresh answers with a task group that never gets
/// `all_tasks_dispatched`; it ends with its tasks, also as the one Task Center task (`ops::get_task`).
#[tokio::test]
async fn acs_lifecycle() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let name = unique("it-acs");
    // pulpcore 3.63.36 (pulp_rpm 3.27.12) sets `all_tasks_dispatched` on a refresh; 3.95.3 and 3.123.1 do not.
    let status: Value = c.get(&c.api("status/"), &[]).await.unwrap();
    let core = status["versions"].as_array().unwrap().iter().find(|v| v["component"] == "core").unwrap()["version"].clone();
    let minor: u32 = core.as_str().unwrap().split('.').nth(1).unwrap().parse().unwrap();
    for (t, path, paths, tasks) in [
        (RepoType::Rpm, "", json!(["rpm/", "rpm2/"]), 2),
        (RepoType::File, "file/PULP_MANIFEST", json!([]), 1),
        (RepoType::Deb, "deb/", json!([]), 1),
    ] {
        // pulp_deb 3.5.4 (pulpcore 3.63.36) has no ACS: the screen says "not supported by this server".
        if c.get::<Value>(&c.api(t.acs()), &[("limit", "1".into())]).await.is_err_and(|e| e.status() == Some(404)) {
            continue;
        }
        let n = format!("{name}-{}", t.component());
        let mut body = json!({ "name": format!("{n}-immediate"), "url": format!("{}/{path}", upstream()), "policy": "immediate" });
        if t == RepoType::Deb {
            body["distributions"] = "fixture".into();
        }
        let immediate = create(&c, t.remotes(), body.clone(), "/remotes/").await;
        let e = c.post(&c.api(t.acs()), &json!({ "name": n, "remote": immediate })).await.unwrap_err();
        assert_eq!(e.status(), Some(400), "{e}");
        assert!(e.to_string().contains("on_demand"), "{e}");

        body["name"] = n.clone().into();
        body["policy"] = "on_demand".into();
        let remote = create(&c, t.remotes(), body, "/remotes/").await;
        let acs = create(&c, t.acs(), json!({ "name": n, "remote": remote, "paths": paths }), "/acs/").await;
        let Mutation::TaskGroup(g) = c.post(&format!("{acs}refresh/"), &json!({})).await.unwrap() else {
            panic!("{}: refresh did not return a task group", t.label())
        };
        let group = ops::wait_task_group(&c, &g, &|_| {}).await.unwrap();
        if minor >= 95 {
            assert!(!group.all_tasks_dispatched, "{}: Pulp sets the flag now; the description rule can go", t.label());
        }
        assert!(group.is_final(), "{}: {group:?}", t.label());
        assert_eq!((group.tasks.len(), group.completed), (tasks, tasks as u64), "{}: one sync per path", t.label());
        let row = ops::get_task(&c, &g).await.unwrap();
        assert_eq!(row.state, "completed", "{}: the Task Center row ends: {}", t.label(), group.description);
        let a: Value = c.get(&acs, &[]).await.unwrap();
        // pulp_file 3.123 does not set it.
        assert_eq!(a["last_refreshed"].is_string(), t != RepoType::File, "{}: {a}", t.label());

        let m = c.delete(&acs).await.unwrap();
        ops::settle(&c, m, &ops::no_progress).await.unwrap();
        assert_eq!(c.get::<Value>(&acs, &[]).await.unwrap_err().status(), Some(404));
        // pulp_deb 3.11 does not link its hidden repository to the ACS, so the delete keeps it.
        let hidden: Option<Value> = ops::find_by_name(&c, &c.api(t.repositories()), &format!("{n}--repository")).await.unwrap();
        assert_eq!(hidden.is_some(), t == RepoType::Deb, "{}: {hidden:?}", t.label());
        if let Some(h) = hidden {
            cleanup(&c, &[h["pulp_href"].as_str().unwrap()]).await;
        }
        cleanup(&c, &[&remote, &immediate]).await;
    }
}

/// pulpcore 3.123 deletes a path that an edit removes from every ACS with that path (`_update_paths`
/// filters by path only); the Edit form warns about it.
#[tokio::test]
async fn acs_path_edit_removes_the_path_from_other_sources() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let (t, name) = (RepoType::Rpm, unique("it-acs-paths"));
    let body = json!({ "name": name, "url": format!("{}/", upstream()), "policy": "on_demand" });
    let remote = create(&c, t.remotes(), body, "/remotes/").await;
    let a = create(&c, t.acs(), json!({ "name": format!("{name}-a"), "remote": remote, "paths": ["rpm/", "rpm2/"] }), "/acs/").await;
    let b = create(&c, t.acs(), json!({ "name": format!("{name}-b"), "remote": remote, "paths": ["rpm/"] }), "/acs/").await;
    let m = c.patch(&a, &json!({ "paths": ["rpm2/"] })).await.unwrap();
    ops::settle(&c, m, &ops::no_progress).await.unwrap();
    let paths = |v: Value| v["paths"].clone();
    assert_eq!(paths(c.get(&a, &[]).await.unwrap()), json!(["rpm2/"]));
    assert_eq!(paths(c.get(&b, &[]).await.unwrap()), json!([]), "pulpcore fixed it: drop the warning of the Edit form");
    cleanup(&c, &[&a, &b, &remote]).await;
}

/// The repair of a version with intact files is a task whose progress reports count nothing to repair.
#[tokio::test]
async fn version_repair_reports_its_counts() {
    if !live_enabled() {
        return;
    }
    let (c, t, name) = (client(), RepoType::Rpm, unique("it-repair"));
    let (repo, remote) = synced_repo(&c, t, &name, "rpm/", "additive").await;
    let v1 = latest(&c, &repo).await;
    assert!(
        c.get::<RepoVersion>(&v1, &[]).await.unwrap().content_summary.present_count() > 0,
        "a version without content repairs the server"
    );
    for verify in [false, true] {
        let task = ops::settle(&c, ops::repair_version(&c, &v1, verify).await.unwrap(), &ops::no_progress).await.unwrap().unwrap();
        let done = |code: &str| task.progress_reports.iter().find(|r| r.code == code).map(|r| r.done);
        assert_eq!([done("repair.missing"), done("repair.corrupted"), done("repair.repaired")], [Some(Some(0)); 3], "{task:?}");
    }
    cleanup(&c, &[&repo, &remote]).await;
}

/// Reclaim space of a repository synced `immediate`: the units stay in the version and lose their stored file,
/// and the distribution still serves a package with the right digest (Pulp downloads it on demand).
#[tokio::test]
async fn reclaim_space_keeps_the_content_served_on_demand() {
    if !live_enabled() {
        return;
    }
    let (c, t, name) = (client(), RepoType::Rpm, unique("it-reclaim"));
    let (repo, remote) = synced_repo(&c, t, &name, "rpm2/", "additive").await;
    let v1 = latest(&c, &repo).await;
    let publication = ops::ensure_publication(&c, t, &v1, &json!({}), &ops::no_progress).await.unwrap();
    let dist = ops::ensure_distribution(&c, t, &name, &name, Some(&publication), &json!({}), None, &ops::no_progress).await.unwrap();
    let base = c.get::<Value>(&dist, &[]).await.unwrap()["base_url"].as_str().unwrap().to_string();
    let stored = async || -> (usize, usize) {
        let q = [("repository_version", v1.clone()), ("fields", "artifact".into())];
        let p: Vec<Value> = c.list_all(&c.api(t.content()), &q, 1000).await.unwrap();
        (p.iter().filter(|x| x["artifact"].is_string()).count(), p.len())
    };
    let (before, units) = stored().await;
    assert_eq!(before, units, "immediate: every package has its file");

    let task = ops::settle(&c, ops::reclaim_space(&c, &repo, &[]).await.unwrap(), &ops::no_progress).await.unwrap().unwrap();
    let deleted = task.progress_reports.iter().find(|r| r.code == "reclaim-space.artifact").and_then(|r| r.done);
    let (after, still) = stored().await;
    assert_eq!(still, units, "the content stays in the version");
    // Packages that another repository of the server also has keep their file (other tests sync the fixtures too).
    eprintln!("reclaim: {units} packages, {before} with a file before, {after} after; the task deleted {deleted:?} file(s)");
    assert_eq!(Some((before - after) as u64), deleted, "a reclaimed unit has no file");

    let msg = ops::verify_served(&c, t, &base, &v1, &json!({ "layout": "nested_alphabetically" })).await.unwrap();
    assert!(msg.contains("matching digest"), "{msg}");
    eprintln!("after reclaim: {msg}; {} package(s) with a file", stored().await.0);
    cleanup(&c, &[&dist, &repo, &remote]).await;
}

/// A modulemd made through the TUI goes into a new version of the repository and shows on its Content screen. Its
/// empty JSON fields go as the empty values of their NOT NULL columns.
#[tokio::test]
async fn a_modulemd_made_through_the_tui_is_in_the_new_version() {
    if !live_enabled() {
        return;
    }
    let (c, name) = (client(), unique("it-module"));
    let repo = create(&c, RepoType::Rpm.repositories(), json!({ "name": name }), "/repositories/").await;
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Repos(RepoType::Rpm, false);
    run(&mut app, Act::CreateModule, json!({ "pulp_href": repo, "name": name }));
    fill(&mut app, &[("kind", "modulemd")]);
    let snippet = format!(
        "--- {{document: modulemd, version: 2, data: {{name: {name}, stream: '1', version: 1, context: c0ffee42, arch: noarch, \
         summary: s, description: d, license: {{module: [MIT]}}}}}}"
    );
    let fields = [("name", name.as_str()), ("stream", "1"), ("version", "1"), ("context", "c0ffee42"), ("arch", "noarch")];
    fill(&mut app, &[&fields[..], &[("description", "d"), ("snippet", &snippet)]].concat());
    settle(&mut app, &mut rx).await;
    assert!(texts(&app).contains(&format!("create modulemd {name}:1 in {name}: new version 1")), "{:?}", texts(&app));
    let v1 = latest(&c, &repo).await;
    let q = [("repository_version", v1.clone()), ("fields", "artifacts,dependencies,profiles".into())];
    let page: Value = c.get(&c.api("content/rpm/modulemds/"), &q).await.unwrap();
    assert_eq!(page["results"], json!([{ "artifacts": [], "dependencies": [], "profiles": {} }]));
    app.push(Kind::Content { version: v1, ctype: 2 });
    settle(&mut app, &mut rx).await;
    let shown: Vec<&str> = app.list.items.iter().map(|v| v["name"].as_str().unwrap_or_default()).collect();
    assert_eq!(shown, [name.as_str()], "the Content screen (modulemds) lists it");
    cleanup(&c, &[&repo]).await;
}
