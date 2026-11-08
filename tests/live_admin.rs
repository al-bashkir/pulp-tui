//! Administration screens against a live Pulp, driven through the TUI actions: the same
//! forms, confirmations and jobs that an operator uses. Nothing here is server-wide
//! destructive: a purge uses `finished_before` 2000-01-01, which matches no task.

mod common;

use common::*;
use pulp_tui::config::Config;
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{Capabilities, PulpClient, RepoType};
use pulp_tui::store::Store;
use pulp_tui::tui::actions::run;
use pulp_tui::tui::app::{App, Modal, Msg, NO_PROFILE, Screen};
use pulp_tui::tui::kinds::{Act, Kind, s};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;

#[tokio::test]
async fn group_rename() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let name = unique("admin-grp");
    let href = create(&c, "groups/", json!({ "name": name }), "/groups/").await;
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Groups;
    run(&mut app, Act::Edit, json!({ "pulp_href": href, "name": name }));
    let renamed = format!("{name}-new");
    fill(&mut app, &[("name", &renamed)]);
    settle(&mut app, &mut rx).await;
    let g: Value = c.get(&href, &[]).await.unwrap();
    assert_eq!(g["name"], renamed.as_str(), "{:?}", texts(&app));
    cleanup(&c, &[&href]).await;
}

/// Add a member to a group, then Remove member: the membership goes, the user account stays.
#[tokio::test]
async fn group_member_add_and_remove_keeps_the_user() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let name = unique("admin-member");
    let user = create(&c, "users/", json!({ "username": name }), "/users/").await;
    let group = create(&c, "groups/", json!({ "name": name }), "/groups/").await;
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::GroupUsers { group: group.clone(), name: name.clone() };
    let members = async || {
        let p: pulp_tui::pulp::client::Page<Value> = c.list(&format!("{group}users/"), &[]).await.unwrap();
        p.results.iter().filter_map(|u| u["username"].as_str().map(String::from)).collect::<Vec<_>>()
    };
    run(&mut app, Act::Create, Value::Null);
    fill(&mut app, &[("username", &name)]);
    settle(&mut app, &mut rx).await;
    assert_eq!(members().await, std::slice::from_ref(&name), "{:?}", texts(&app));
    run(&mut app, Act::Delete, json!({ "pulp_href": user, "username": name }));
    yes(&mut app);
    settle(&mut app, &mut rx).await;
    assert!(members().await.is_empty(), "{:?}", texts(&app));
    let kept: Value = c.get(&user, &[]).await.unwrap_or_else(|e| panic!("the user is gone: {e}"));
    assert_eq!(kept["username"], name.as_str());
    cleanup(&c, &[&group, &user]).await;
}

/// A new File repository and the completed task of an empty `modify/` on it (the task reserves the repository).
async fn repo_with_task(c: &PulpClient, prefix: &str) -> (String, pulp_tui::pulp::models::Task) {
    let repo = create(c, RepoType::File.repositories(), json!({ "name": unique(prefix) }), "/repositories/").await;
    let m = c.post(&format!("{repo}modify/"), &json!({})).await.unwrap();
    (repo, ops::settle(c, m, &no_progress).await.unwrap().expect("a task"))
}

#[tokio::test]
async fn server_tasks_filter() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let (repo, task) = repo_with_task(&c, "admin-filter").await;
    let (mut app, mut rx) = app().await;
    app.open(Screen::List(Kind::ServerTasks(vec![])));
    settle(&mut app, &mut rx).await;
    let day = task.started_at.clone().unwrap()[..10].to_string();
    run(&mut app, Act::Filter, Value::Null);
    fill(&mut app, &[("state", "completed"), ("name", "add_and_remove"), ("reserved_resources", &repo), ("started_at__gte", &day)]);
    settle(&mut app, &mut rx).await;
    let rows: Vec<&str> = app.list.items.iter().filter_map(|t| t["pulp_href"].as_str()).collect();
    assert_eq!(rows, [task.pulp_href.as_str()], "only the task that reserved the repository: {:?}", app.list.error);

    // Pulp checks the date.
    run(&mut app, Act::Filter, Value::Null);
    fill(&mut app, &[("started_at__gte", "yesterday")]);
    settle(&mut app, &mut rx).await;
    let e = app.list.error.clone().unwrap_or_default();
    assert!(e.contains("Enter a valid date/time"), "{e}");
    cleanup(&c, &[&repo]).await;
}

#[tokio::test]
async fn task_delete_and_purge() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let (repo, task) = repo_with_task(&c, "admin-delete").await;
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::ServerTasks(vec![]);
    run(&mut app, Act::Delete, json!({ "pulp_href": task.pulp_href, "name": task.name, "state": task.state }));
    yes(&mut app);
    settle(&mut app, &mut rx).await;
    let e = c.get::<Value>(&task.pulp_href, &[]).await.unwrap_err();
    assert_eq!(e.status(), Some(404), "the task is deleted: {:?}", texts(&app));

    // Finished before 2000: the count and the purge match no task.
    run(&mut app, Act::PurgeTasks, Value::Null);
    fill(&mut app, &[("finished_before", "2000-01-01"), ("states", "completed, failed")]);
    settle(&mut app, &mut rx).await;
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed.as_deref(), Some("pulp1"));
    assert_eq!(lines[0], "Purge on pulp1: 0 task(s) in state completed, failed that finished before 2000-01-01.");
    settle(&mut app, &mut rx).await;
    assert_eq!(texts(&app).last().map(String::as_str), Some("purge tasks on pulp1: deleted 0 task(s)"), "{:?}", texts(&app));
    cleanup(&c, &[&repo]).await;
}

#[tokio::test]
async fn workers_screen() {
    if !live_enabled() {
        return;
    }
    let (mut app, mut rx) = app().await;
    app.open(Screen::List(Kind::Workers));
    settle(&mut app, &mut rx).await;
    assert!(!app.list.items.is_empty(), "pulp1 has a worker: {:?}", app.list.error);
    let cols = Kind::Workers.columns();
    for w in &app.list.items {
        let (name, beat) = ((cols[0].get)(w), (cols[1].get)(w));
        assert!(!name.is_empty() && beat.len() == 19, "{w}");
    }
    let w = app.list.items[0].clone();
    run(&mut app, Act::Inspect, w.clone());
    settle(&mut app, &mut rx).await;
    let Some(Modal::Text { lines, .. }) = &app.modal else { panic!("no details: {:?}", texts(&app)) };
    assert!(lines.iter().any(|l| l.contains(w["name"].as_str().unwrap())), "{lines:?}");
}

/// Answer the open path input with `path`.
fn type_path(app: &mut App, path: &str) {
    let Some(Modal::Input { on_done: Some(done), .. }) = app.modal.take() else { panic!("no path input: {:?}", texts(app)) };
    done(app, path.into());
}

#[tokio::test]
async fn repository_definitions_export_and_import() {
    if !live_enabled() {
        return;
    }
    use pulp_tui::repo_export::ServerExport;
    let c = client();
    let name = unique("admin-def");
    let secret = json!({ "password": "pw-secret-7f3", "username": "u", "headers": [{ "X-Token": "hdr-secret-7f3" }] });
    let mut body = json!({ "name": name, "url": format!("{}/file/PULP_MANIFEST", upstream()) });
    body.as_object_mut().unwrap().extend(secret.as_object().unwrap().clone());
    let remote = create(&c, RepoType::File.remotes(), body, "/remotes/").await;
    let repo =
        create(&c, RepoType::File.repositories(), json!({ "name": name, "remote": remote, "description": "d" }), "/repositories/").await;
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Profiles;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("repos.json").display().to_string();
    run(&mut app, Act::ExportRepos, json!({ "name": "pulp1" }));
    type_path(&mut app, &file);
    settle(&mut app, &mut rx).await;
    let written = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{e}: {:?}", texts(&app)));
    assert!(!written.contains("secret-7f3"), "a secret in the export");
    let ex: ServerExport = serde_json::from_str(&written).unwrap();
    let mine = |v: &[pulp_tui::repo_export::Entry]| v.iter().find(|e| e.name == name).cloned().expect("exported");
    let (mut r, p) = (mine(&ex.remotes), mine(&ex.repositories));
    r.secrets.sort();
    assert_eq!((r.secrets, p.fields["remote"].clone()), (vec!["password".to_string(), "username".into()], json!(name)));

    // The two objects under new names, as from another server: the plan, then the import.
    let new = format!("{name}-b");
    let entry = |e: pulp_tui::repo_export::Entry| pulp_tui::repo_export::Entry { name: new.clone(), secrets: vec![], ..e };
    let mut p2 = entry(p);
    p2.fields.insert("remote".into(), json!(new));
    let ex2 = ServerExport { format: 1, remotes: vec![entry(mine(&ex.remotes))], alt_remotes: vec![], repositories: vec![p2] };
    std::fs::write(&file, serde_json::to_vec(&ex2).unwrap()).unwrap();
    run(&mut app, Act::ImportRepos, Value::Null);
    type_path(&mut app, &file);
    settle(&mut app, &mut rx).await;
    let (lines, _) = yes(&mut app);
    assert_eq!(lines[0], format!("Import {file} into pulp1: create 2, keep 0 existing (remotes and repositories)."));
    settle(&mut app, &mut rx).await;
    assert!(texts(&app).contains(&"profile pulp1: created 2, kept 0 existing (remotes and repositories)".to_string()), "{:?}", texts(&app));
    let found = |coll: &'static str| {
        let c = c.clone();
        let new = new.clone();
        async move { c.list::<Value>(&c.api(coll), &[("name", new)]).await.unwrap().results.into_iter().next().expect("imported") }
    };
    let (r2, p2) = (found(RepoType::File.remotes()).await, found(RepoType::File.repositories()).await);
    assert_eq!(p2["remote"], r2["pulp_href"], "the repository has the imported remote");
    let hrefs = [&p2["pulp_href"], &r2["pulp_href"]].map(|h| h.as_str().unwrap().to_string());
    cleanup(&c, &[&hrefs[0], &hrefs[1], &repo, &remote]).await;
}

/// Read-only: the groups belong to other tests (replication). `task_group_cancel` cancels its own group.
#[tokio::test]
async fn task_groups_screen() {
    if !live_enabled() {
        return;
    }
    let (mut app, mut rx) = app().await;
    app.open(Screen::List(Kind::TaskGroups));
    settle(&mut app, &mut rx).await;
    assert_eq!(app.list.error, None);
    let Some(g) = app.list.items.first().cloned() else { return eprintln!("no task group on the server: nothing to inspect") };
    assert!(g.get("tasks").is_none(), "the list leaves the tasks out: {g}");
    let counts: Vec<String> = Kind::TaskGroups.columns()[1..].iter().map(|c| (c.get)(&g)).collect();
    assert!(counts.iter().all(|n| n.parse::<u64>().is_ok()), "{counts:?}");
    run(&mut app, Act::Inspect, g);
    settle(&mut app, &mut rx).await;
    let Some(Modal::Text { lines, .. }) = &app.modal else { panic!("no details: {:?}", texts(&app)) };
    assert!(lines.iter().any(|l| l.contains("\"tasks\"")), "Inspect reads the whole group: {lines:?}");
}

/// A FIFO in the directory of the fixture server (`PULP_TUI_TEST_UPSTREAM_DIR`, default
/// target/fixtures/upstream). The server blocks in its open until a writer comes, so a download of
/// its URL gets no answer. The drop opens it for write (O_RDWR does not block) and removes it: the
/// server then sends an empty body.
struct Stall(std::path::PathBuf);

impl Drop for Stall {
    fn drop(&mut self) {
        let _ = std::fs::OpenOptions::new().read(true).write(true).open(&self.0);
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The operator cancels a task group that is still running, from the Task groups screen. The group is
/// a File ACS refresh of a stalled URL: its sync cannot finish before the cancel, so this is not a race.
#[tokio::test]
async fn task_group_cancel() {
    if !live_enabled() {
        return;
    }
    use pulp_tui::pulp::Mutation;
    use pulp_tui::pulp::models::TaskGroup;
    use pulp_tui::tui::app::Level;
    use std::time::{Duration, Instant};
    let c = client();
    let name = unique("admin-cancel");
    let dir = std::env::var("PULP_TUI_TEST_UPSTREAM_DIR").map(Into::into).unwrap_or_else(|_| fixtures().join("upstream"));
    let stall = Stall(dir.join(&name));
    let made = std::process::Command::new("mkfifo").arg(&stall.0).status().unwrap();
    assert!(made.success(), "mkfifo {}", stall.0.display());
    let body = json!({ "name": name, "url": format!("{}/{name}", upstream()), "policy": "on_demand" });
    let remote = create(&c, RepoType::File.remotes(), body, "/remotes/").await;
    let acs = create(&c, RepoType::File.acs(), json!({ "name": name, "remote": remote }), "/acs/").await;
    let Mutation::TaskGroup(group) = c.post(&format!("{acs}refresh/"), &json!({})).await.unwrap() else { panic!("no task group") };
    let read = || async { c.get::<TaskGroup>(&group, &[]).await.unwrap() };
    let deadline = Instant::now() + Duration::from_secs(120);
    while read().await.running == 0 {
        assert!(Instant::now() < deadline, "no worker started the sync: {:?}", read().await);
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let (mut app, mut rx) = app().await;
    app.open(Screen::List(Kind::TaskGroups));
    settle(&mut app, &mut rx).await;
    let row = app.list.items.iter().find(|g| g["pulp_href"] == group.as_str()).cloned();
    let row = row.unwrap_or_else(|| panic!("the group is not on the first page: {:?}", app.list.error));
    run(&mut app, Act::CancelTask, row);
    settle(&mut app, &mut rx).await; // the group is read before its confirmation
    let (lines, _) = yes(&mut app);
    let description = read().await.description;
    assert_eq!(lines[0], format!("Ask Pulp to cancel the whole task group {description}: 1 of 1 task(s) are not finished."));
    settle(&mut app, &mut rx).await;
    let asked = texts(&app).into_iter().find(|t| t.starts_with("cancel requested; Pulp reports state "));
    assert!(asked.is_some(), "Pulp took the cancel: {:?}", texts(&app));

    // The Task Center polls the group until Pulp ends it.
    let deadline = Instant::now() + Duration::from_secs(90);
    while app.tasks.items.iter().find(|t| t.href == group).is_none_or(|t| !t.is_final()) {
        assert!(Instant::now() < deadline, "the group did not end: {:?}", read().await);
        app.tick();
        if let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            app.handle_msg(m);
        }
    }
    settle(&mut app, &mut rx).await;
    let g = read().await;
    let states: Vec<&str> = g.tasks.iter().filter_map(|t| t["state"].as_str()).collect();
    assert_eq!((states, g.canceled, g.active()), (vec!["canceled"], 1, 0), "{g:?}");
    assert_eq!(app.tasks.items.iter().find(|t| t.href == group).unwrap().state, "canceled");
    let errors: Vec<&String> = app.notices.iter().filter(|n| n.level == Level::Error).map(|n| &n.text).collect();
    assert!(errors.is_empty(), "{errors:?}");
    let ended = format!("task canceled: {description}: completed 0  failed 0  canceled 1  skipped 0");
    assert!(app.notices.iter().any(|n| n.level == Level::Warn && n.text == ended), "{:?}", texts(&app));
    let listed = app.list.items.iter().find(|r| r["pulp_href"] == group.as_str()).map(|r| r["canceled"].clone());
    assert_eq!(listed, Some(json!(1)), "the screen reloads with the count");

    drop(stall);
    cleanup(&c, &[&acs, &remote]).await;
}

/// A checkpoint distribution, driven through the Distributions screen: Verify reads the newest checkpoint
/// publication at `<base>/<timestamp>/`; Re-point (rollback) is refused before any write.
#[tokio::test]
async fn checkpoint_distribution_verifies_and_refuses_rollback() {
    if !live_enabled() {
        return;
    }
    let (mut app, mut rx) = app().await;
    let t = RepoType::File;
    if !app.conn.as_ref().unwrap().caps.has_field(t.distribution_schema(), "checkpoint") {
        eprintln!("skipping: this server's distributions have no `checkpoint` field (older pulpcore)");
        return;
    }
    let c = client();
    let name = unique("it-checkpoint");
    let repo = create(&c, t.repositories(), json!({ "name": name }), "/repositories/").await;
    let dir = tempfile::tempdir().unwrap();
    for i in 1..=2 {
        let path = dir.path().join(format!("{i}.txt"));
        // Unique bytes, so a rerun does not find the content unit of an earlier run.
        std::fs::write(&path, format!("{name} {i}\n")).unwrap();
        ops::upload(&c, t, &path, Some(&format!("{i}.txt")), Some(&repo), true, &|_, _| {}, &no_progress).await.unwrap();
        // Pulp makes a checkpoint publication only with `repository` (of its latest version).
        create(&c, t.publications(), json!({ "repository": repo, "checkpoint": true }), "/publications/").await;
    }
    let dist = create(
        &c,
        t.distributions(),
        json!({ "name": name, "base_path": name, "repository": repo, "checkpoint": true }),
        "/distributions/",
    )
    .await;
    let item: Value = c.get(&dist, &[]).await.unwrap();
    let pubs = || async { c.list_all::<Value>(&c.api(t.publications()), &[("repository", repo.clone())], 100).await.unwrap().len() };
    assert_eq!(pubs().await, 2);

    app.list.kind = Kind::Distributions(t);
    run(&mut app, Act::Verify, item.clone());
    settle(&mut app, &mut rx).await;
    let Some(Modal::Text { lines, .. }) = app.modal.take() else { panic!("no verify result: {:?}", texts(&app)) };
    assert!(lines[0].starts_with(&format!("{name}: checkpoint distribution of repository ")), "{lines:?}");
    assert_eq!(lines.last().map(String::as_str), Some("OK: PULP_MANIFEST matches 2 files"), "{lines:?}");

    run(&mut app, Act::Rollback, item.clone());
    fill(&mut app, &[("repo", &name), ("number", "1")]);
    settle(&mut app, &mut rx).await;
    assert!(app.modal.is_none(), "no preview to confirm");
    let e = texts(&app).pop().unwrap_or_default();
    assert!(e.contains(&format!("distribution {name} is a checkpoint distribution and Pulp refuses to pin it")), "{e}");
    // Nothing changed on the server: no new publication, the same binding.
    assert_eq!(pubs().await, 2);
    assert_eq!(c.get::<Value>(&dist, &[]).await.unwrap(), item);

    cleanup(&c, &[&dist, &repo]).await;
}

/// pulpcore `AlternateContentSource.remote` is PROTECT: the Remotes delete refuses a remote that an ACS uses
/// before any request, and the remote stays.
#[tokio::test]
async fn remote_delete_refused_while_an_acs_uses_it() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let name = unique("admin-acs-remote");
    let body = json!({ "name": name, "url": format!("{}/{name}/", upstream()), "policy": "on_demand" });
    let remote = create(&c, RepoType::File.remotes(), body, "/remotes/").await;
    let acs = create(&c, RepoType::File.acs(), json!({ "name": name, "remote": remote }), "/acs/").await;

    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Remotes(RepoType::File, false);
    run(&mut app, Act::Delete, json!({ "pulp_href": remote, "name": name }));
    settle(&mut app, &mut rx).await;
    assert!(app.modal.is_none(), "no confirm: {:?}", texts(&app));
    let refused = format!(
        "impact of deleting remote {name}: refused: Pulp does not delete a remote that an alternate content source uses: \
         {name} (File) uses {name}; delete those ACS or change their remote first"
    );
    assert!(texts(&app).contains(&refused), "{:?}", texts(&app));
    c.get::<Value>(&remote, &[]).await.expect("the remote is still there");
    cleanup(&c, &[&acs, &remote]).await;
}

/// The Content Guards screen: a header guard made with the forms, used by a File distribution. The list
/// names the distribution; an edit keeps the header value it never shows; the delete impact names the
/// distribution, and after the delete Pulp serves it without a guard (`on_delete=SET_NULL`).
#[tokio::test]
async fn content_guard_create_edit_and_delete_name_its_distribution() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let name = unique("admin-guard");
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::ContentGuards;
    run(&mut app, Act::Create, Value::Null);
    fill(&mut app, &[("type", "core/header")]);
    fill(&mut app, &[("name", &name), ("header_name", "X-Pulp-Tui"), ("header_value", "s3cret")]);
    settle(&mut app, &mut rx).await;
    let guard: Value = ops::find_by_name(&c, &c.api("contentguards/"), &name).await.unwrap().unwrap_or_else(|| panic!("{:?}", texts(&app)));
    let href = guard["pulp_href"].as_str().unwrap().to_string();
    let body = json!({ "name": name, "base_path": name, "content_guard": href });
    let dist = create(&c, RepoType::File.distributions(), body, "/distributions/").await;

    app.open(Screen::List(Kind::ContentGuards));
    app.list.filter = name.clone();
    app.reload();
    settle(&mut app, &mut rx).await;
    let row = app.list.items.iter().find(|r| r["name"] == name.as_str()).cloned().unwrap_or_else(|| panic!("{:?}", app.list.error));
    let cells: Vec<String> = Kind::ContentGuards.columns().iter().map(|col| (col.get)(&row)).collect();
    assert_eq!(cells, [name.clone(), "core/header".into(), name.clone()]);

    run(&mut app, Act::Edit, row.clone());
    settle(&mut app, &mut rx).await;
    fill(&mut app, &[("header_name", "X-Other")]);
    settle(&mut app, &mut rx).await;
    let g: Value = c.get(&href, &[]).await.unwrap();
    assert_eq!((g["header_name"].as_str(), g["header_value"].as_str()), (Some("X-Other"), Some("s3cret")), "{:?}", texts(&app));

    run(&mut app, Act::Delete, row);
    settle(&mut app, &mut rx).await;
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed.as_deref(), Some(name.as_str()));
    assert!(lines[1].contains("they become PUBLIC") && lines[2] == format!("  {name}"), "{lines:?}");
    settle(&mut app, &mut rx).await;
    assert_eq!(c.get::<Value>(&href, &[]).await.unwrap_err().status(), Some(404), "{:?}", texts(&app));
    assert_eq!(c.get::<Value>(&dist, &[]).await.unwrap()["content_guard"], Value::Null, "Pulp clears the guard");
    cleanup(&c, &[&dist]).await;
}

/// Download access of an RBAC guard: the form sends the downloader role; Pulp then lists the user.
#[tokio::test]
async fn rbac_guard_access_allows_and_removes_a_user() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let name = unique("admin-rbac");
    let user = create(&c, "users/", json!({ "username": name }), "/users/").await;
    let href = create(&c, "contentguards/core/rbac/", json!({ "name": name }), "/contentguards/").await;
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::ContentGuards;
    let users = async || {
        let g: Value = c.get(&href, &[]).await.unwrap();
        g["users"].as_array().unwrap().iter().filter_map(|u| u["username"].as_str().map(String::from)).collect::<Vec<_>>()
    };
    for change in ["allow", "remove"] {
        run(&mut app, Act::GuardAccess, json!({ "pulp_href": href, "name": name }));
        settle(&mut app, &mut rx).await;
        fill(&mut app, &[("change", change), ("users", &name)]);
        settle(&mut app, &mut rx).await;
        assert_eq!(users().await.contains(&name), change == "allow", "{change}: {:?}", texts(&app));
    }
    cleanup(&c, &[&href, &user]).await;
}

/// The Uploads screen: a chunked upload left unfinished (one chunk of two, no commit) is listed with
/// its size and no completion; Delete removes it after a plain confirmation (no typed name).
#[tokio::test]
async fn an_unfinished_upload_is_listed_and_deleted() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let href = create(&c, "uploads/", json!({ "size": 2048 }), "/uploads/").await;
    c.put_chunk(&href, (0, 1023, 2048), vec![b'x'; 1024]).await.unwrap();
    let (mut app, mut rx) = app().await;
    app.open(Screen::List(Kind::Uploads));
    settle(&mut app, &mut rx).await;
    let row = app.list.items.iter().find(|r| r["pulp_href"] == href.as_str()).cloned();
    let row = row.unwrap_or_else(|| panic!("{href} not on the first page: {:?}", app.list.error));
    let cells: Vec<String> = Kind::Uploads.columns().iter().map(|col| (col.get)(&row)).collect();
    assert_eq!((cells[2].as_str(), cells[3].as_str(), cells[1].len()), ("2048", "", 19), "{cells:?}");

    run(&mut app, Act::Delete, row);
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed, None, "an upload is only the chunks a client sent: {lines:?}");
    settle(&mut app, &mut rx).await;
    assert_eq!(c.get::<Value>(&href, &[]).await.unwrap_err().status(), Some(404), "{:?}", texts(&app));
}

/// The Task Schedules screen lists the schedules (pulp1 has two; a server may have none); W opens the
/// last task of a schedule in the Task Center.
#[tokio::test]
async fn task_schedules_screen() {
    if !live_enabled() {
        return;
    }
    let (mut app, mut rx) = app().await;
    app.open(Screen::List(Kind::TaskSchedules));
    settle(&mut app, &mut rx).await;
    assert_eq!(app.list.error, None);
    let cols = Kind::TaskSchedules.columns();
    for r in &app.list.items {
        let cells: Vec<String> = cols.iter().map(|col| (col.get)(r)).collect();
        assert!(!cells[0].is_empty() && !cells[1].is_empty() && !cells[2].is_empty(), "{r}");
    }
    let Some(r) = app.list.items.iter().find(|r| r["last_task"].is_string()).cloned() else { return };
    run(&mut app, Act::WatchTask, r.clone());
    let Some(Modal::Text { title, .. }) = &app.modal else { panic!("no live view: {:?}", texts(&app)) };
    assert_eq!(title, &format!("Watch task {}", r["name"].as_str().unwrap()));
    assert!(app.tasks.items.iter().any(|t| r["last_task"] == t.href.as_str()), "the last task is in the Task Center");
}

/// The Domains screen follows DOMAIN_ENABLED: pulp1 has it off (Pulp then lists only the default domain), so
/// the screen says that the server does not support it; a server with domains lists the default domain.
#[tokio::test]
async fn domains_screen_follows_domain_enabled() {
    if !live_enabled() {
        return;
    }
    let (mut app, mut rx) = app().await;
    let enabled = app.conn.as_ref().unwrap().caps.status.as_ref().and_then(|s| s.domain_enabled) == Some(true);
    app.open(Screen::List(Kind::Domains));
    settle(&mut app, &mut rx).await;
    if enabled {
        assert_eq!(app.list.error, None);
        assert!(app.list.items.iter().any(|r| r["name"] == "default"), "{:?}", app.list.items);
    } else {
        let off = "not supported by this server: domains (domains/, with DOMAIN_ENABLED on the server)";
        assert_eq!(app.list.error.as_deref(), Some(off));
    }
}

/// Object roles of a File repository: admin, its creator, holds the owner role; the form adds the viewer role
/// for a new user, Pulp then lists it, and the form removes it again.
#[tokio::test]
async fn object_roles_add_and_remove_a_viewer_on_a_repository() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let name = unique("admin-objroles");
    let user = create(&c, "users/", json!({ "username": name }), "/users/").await;
    let repo = create(&c, RepoType::File.repositories(), json!({ "name": name }), "/repositories/").await;
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Repos(RepoType::File, false);
    let item = json!({ "pulp_href": repo, "name": name });
    let viewers = async || {
        let v: Value = c.get(&format!("{repo}list_roles/"), &[]).await.unwrap();
        let roles = v["roles"].as_array().unwrap().iter().filter(|r| r["role"] == "file.filerepository_viewer");
        roles.flat_map(|r| r["users"].as_array().unwrap().iter().filter_map(|u| u.as_str().map(String::from))).collect::<Vec<_>>()
    };
    for change in ["add", "remove"] {
        run(&mut app, Act::ObjectRoles, item.clone());
        settle(&mut app, &mut rx).await;
        let Some(Modal::Form { form, .. }) = &app.modal else { panic!("no form: {:?}", texts(&app)) };
        let owner = form.notes.iter().any(|n| n.starts_with("file.filerepository_owner: users ") && n.contains("admin"));
        assert!(owner && form.notes.last().unwrap().starts_with("My permissions: "), "{:?}", form.notes);
        fill(&mut app, &[("change", change), ("role", "file.filerepository_viewer"), ("users", &name)]);
        settle(&mut app, &mut rx).await;
        assert_eq!(viewers().await.contains(&name), change == "add", "{change}: {:?}", texts(&app));
    }
    cleanup(&c, &[&repo, &user]).await;
}

/// The Artifacts screen lists the newest artifacts with size and sha256; `/` with a full sha256 finds that one.
#[tokio::test]
async fn artifacts_screen_lists_and_finds_a_sha256() {
    if !live_enabled() {
        return;
    }
    let (mut app, mut rx) = app().await;
    app.open(Screen::List(Kind::Artifacts));
    settle(&mut app, &mut rx).await;
    assert_eq!(app.list.error, None);
    let Some(first) = app.list.items.first().cloned() else { return };
    let cells: Vec<String> = Kind::Artifacts.columns().iter().map(|col| (col.get)(&first)).collect();
    assert!(cells[1].parse::<u64>().is_ok() && cells[2].len() == 64, "{cells:?}");
    app.list.filter = cells[2].clone();
    app.reload();
    settle(&mut app, &mut rx).await;
    let found: Vec<&str> = app.list.items.iter().filter_map(|r| r["sha256"].as_str()).collect();
    assert_eq!(found, [cells[2].as_str()], "{:?}", app.list.error);
}

/// Server repair in its smallest scope (`verify_checksums: false`: Pulp only checks that each file exists): the
/// content count, the typed profile name, then the task ends and pulp-tui reports what it counted.
#[tokio::test]
async fn server_repair_checks_that_the_files_exist() {
    if !live_enabled() {
        return;
    }
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Artifacts;
    run(&mut app, Act::RepairServer, Value::Null);
    fill(&mut app, &[("verify_checksums", "false")]);
    settle(&mut app, &mut rx).await;
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed.as_deref(), Some("pulp1"));
    assert!(
        lines[0].starts_with("Repair on pulp1: Pulp checks the files of all ") && lines[1] == "It checks only that each file exists.",
        "{lines:?}"
    );
    settle(&mut app, &mut rx).await;
    let t = texts(&app);
    assert!(t.iter().any(|x| x.starts_with("repair all files on pulp1: ") && x.contains(" missing, ")), "{t:?}");
}

/// Access policies: a policy that an operator changed (here a PATCH with its own statements, so no access
/// changes) is reset through the confirmation with the typed viewset name; Pulp then marks it as not customized.
#[tokio::test]
async fn access_policy_reset_restores_the_default() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let (mut app, mut rx) = app().await;
    app.open(Screen::List(Kind::AccessPolicies));
    app.list.filter = "task-schedules".into();
    app.reload();
    settle(&mut app, &mut rx).await;
    assert_eq!(app.list.error, None);
    let row =
        app.list.items.iter().find(|r| r["viewset_name"] == "task-schedules").cloned().unwrap_or_else(|| panic!("{:?}", app.list.items));
    let href = row["pulp_href"].as_str().unwrap().to_string();
    let before = row["statements"].clone();
    c.patch(&href, &json!({ "statements": before })).await.unwrap();
    let row: Value = c.get(&href, &[]).await.unwrap();
    assert_eq!(row["customized"], true, "an update marks the policy as customized");
    run(&mut app, Act::ResetPolicy, row);
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed.as_deref(), Some("task-schedules"));
    assert!(lines[1].starts_with("Pulp replaces its "), "{lines:?}");
    settle(&mut app, &mut rx).await;
    let after: Value = c.get(&href, &[]).await.unwrap();
    assert_eq!((after["customized"].clone(), after["statements"].clone()), (json!(false), before), "{:?}", texts(&app));
}

/// Users through the TUI: Create with a password, Edit, Password (w), Disable (A), Delete. Each change shows in the
/// reloaded list and in Pulp; the passwords are checked by signing in with them.
#[tokio::test]
async fn user_create_edit_password_disable_and_delete_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, name) = (client(), unique("admin-user"));
    let (pw1, pw2) = ("Xq7!mLp2-zR9v", "Wd4#nKs8-pT6c");
    let signs_in = async |pw: &str| {
        let r = user_client(&name, pw).get::<Value>(&c.api(RepoType::File.repositories()), &[("limit", "1".into())]).await;
        r.map(|_| ()).map_err(|e| e.status())
    };
    let (mut app, mut rx) = app().await;
    open_list(&mut app, &mut rx, Kind::Users, &name).await;
    run(&mut app, Act::Create, Value::Null);
    fill(&mut app, &[("username", &name), ("password", pw1), ("first_name", "Ann")]);
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("create user {name}: done"));
    let u = row(&app, "username", &name).unwrap_or_else(|| panic!("the list reloads with it: {:?}", app.list.items));
    assert_eq!((u["first_name"].as_str(), &u["is_active"], &u["is_staff"]), (Some("Ann"), &json!(true), &json!(false)));
    assert_eq!(signs_in(pw1).await, Ok(()));

    run(&mut app, Act::Edit, u);
    fill(&mut app, &[("last_name", "Bee")]);
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("update user {name}: done"));
    let u = row(&app, "username", &name).unwrap();
    assert_eq!((u["first_name"].as_str(), u["last_name"].as_str()), (Some("Ann"), Some("Bee")));

    run(&mut app, Act::Password, u.clone());
    fill(&mut app, &[("p1", pw2), ("p2", pw2)]);
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("set password of {name}: done"));
    assert_eq!((signs_in(pw2).await, signs_in(pw1).await), (Ok(()), Err(Some(401))));

    run(&mut app, Act::ToggleActive, u);
    let (lines, typed) = yes(&mut app);
    assert_eq!((lines, typed), (vec![format!("Disable user {name}.")], None));
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("set {name} active=false: done"));
    let u = row(&app, "username", &name).unwrap();
    assert_eq!(u["is_active"], false, "the list reloads");
    assert_eq!(signs_in(pw2).await, Err(Some(401)), "a disabled user cannot sign in");

    run(&mut app, Act::Delete, u.clone());
    settle(&mut app, &mut rx).await; // the impact is read before the confirmation
    let (lines, typed) = yes(&mut app);
    assert_eq!((typed.as_deref(), lines[0].clone()), (Some(name.as_str()), format!("Delete user {name}.")));
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("delete user {name}: done"));
    assert!(row(&app, "username", &name).is_none(), "the list reloads without it");
    assert!(c.get::<Value>(&s(&u, "pulp_href"), &[]).await.unwrap_err().is_not_found());
}

/// Servers, Test connection (T): the profile's URL answers, the user and where Pulp serves content.
#[tokio::test]
async fn server_profile_test_connection() {
    if !live_enabled() {
        return;
    }
    let (mut app, mut rx) = app().await;
    open_list(&mut app, &mut rx, Kind::Profiles, "").await;
    let p = row(&app, "name", "pulp1").unwrap_or_else(|| panic!("{:?}", app.list.items));
    run(&mut app, Act::TestProfile, p.clone());
    settle(&mut app, &mut rx).await;
    let Some(Modal::Text { title, lines, .. }) = app.modal.take() else { panic!("no result: {:?}", texts(&app)) };
    assert_eq!((title, lines[0].clone()), ("Test pulp1".to_string(), format!("Connection to {} OK", s(&p, "url"))));
    assert_eq!(lines[1], "user: admin (staff: Some(true))");
    let content = lines.iter().find_map(|l| l.strip_prefix("content served at: ")).unwrap_or_default();
    assert!(content.starts_with("http") && content.contains("/pulp/content/"), "{lines:?}");
}

/// Roles through the TUI: Create a custom role, Edit its permissions, Delete it: the impact names the user that has
/// it, the typed name; Pulp then drops the user's assignment.
#[tokio::test]
async fn custom_role_create_edit_and_delete_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, name) = (client(), unique("admin-role"));
    let user = create(&c, "users/", json!({ "username": name }), "/users/").await;
    let perms = |r: &Value| {
        let mut p: Vec<String> = r["permissions"].as_array().unwrap().iter().map(|p| p.as_str().unwrap().to_string()).collect();
        p.sort();
        p
    };
    let (mut app, mut rx) = app().await;
    open_list(&mut app, &mut rx, Kind::Roles, &name).await;
    run(&mut app, Act::Create, Value::Null);
    fill(&mut app, &[("name", &name), ("description", "test"), ("permissions", "file.view_filerepository")]);
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("create role {name}: done"));
    let r = row(&app, "name", &name).unwrap_or_else(|| panic!("the list reloads with it: {:?}", app.list.items));
    assert_eq!((perms(&r), &r["locked"]), (vec!["file.view_filerepository".to_string()], &json!(false)));

    run(&mut app, Act::Edit, r);
    fill(&mut app, &[("permissions", "file.view_filerepository, file.change_filerepository")]);
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("update role {name}: done"));
    let r = row(&app, "name", &name).unwrap();
    assert_eq!(perms(&r), ["file.change_filerepository", "file.view_filerepository"], "the list reloads");

    c.post(&format!("{user}roles/"), &json!({ "role": name, "content_object": null })).await.unwrap();
    run(&mut app, Act::Delete, r.clone());
    settle(&mut app, &mut rx).await; // the holders are read before the confirmation
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed.as_deref(), Some(name.as_str()));
    assert_eq!(lines[1..], [format!("1 user(s) have it: {name}"), "0 group(s) have it".into()]);
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("delete role {name}: done"));
    assert!(row(&app, "name", &name).is_none(), "the list reloads without it");
    assert!(c.get::<Value>(&s(&r, "pulp_href"), &[]).await.unwrap_err().is_not_found());
    assert_eq!(roles_of(&c, &user).await, [], "Pulp drops the assignment");
    cleanup(&c, &[&user]).await;
}

/// Upstream Pulps through the TUI (no replication): Create with an account, Edit to policy `labeled` (the typed
/// policy word) and clear the password, Delete after the impact and the typed name.
#[tokio::test]
async fn upstream_pulp_create_edit_and_delete_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, name) = (client(), unique("admin-upstream"));
    // pulpcore 3.63 has no policy: its replication deletes every local object of the domain that is not upstream.
    let policy = Capabilities::detect(&c).await.unwrap().has_field("UpstreamPulp", "policy");
    let set = |u: &Value, k: &str| u["hidden_fields"].as_array().unwrap().iter().any(|h| h["name"] == k && h["is_set"] == true);
    let (mut app, mut rx) = app().await;
    open_list(&mut app, &mut rx, Kind::UpstreamPulps, &name).await;
    run(&mut app, Act::Create, Value::Null);
    fill(&mut app, &[("name", &name), ("base_url", "https://upstream.example.com"), ("username", "u"), ("password", "p")]);
    if !policy {
        let (lines, typed) = yes(&mut app);
        assert_eq!(typed.as_deref(), Some(name.as_str()));
        assert!(lines[0].contains("may DELETE every local repository"), "{lines:?}");
    }
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("create upstream {name}: done"));
    let u = row(&app, "name", &name).unwrap_or_else(|| panic!("the list reloads with it: {:?}", app.list.items));
    assert_eq!((u["policy"].as_str(), set(&u, "username"), set(&u, "password")), (policy.then_some("nodelete"), true, true));
    let href = s(&u, "pulp_href");

    run(&mut app, Act::Edit, u);
    if policy {
        fill(&mut app, &[("policy", "labeled"), ("clear_password", "true")]);
        let (lines, typed) = yes(&mut app);
        assert_eq!(typed.as_deref(), Some("labeled"));
        assert!(lines[0].starts_with("!!! policy `labeled` may DELETE "), "{lines:?}");
    } else {
        fill(&mut app, &[("clear_password", "true")]);
    }
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("update upstream {name}: done"));
    let u: Value = c.get(&href, &[]).await.unwrap();
    assert_eq!((u["policy"].as_str(), set(&u, "username"), set(&u, "password")), (policy.then_some("labeled"), true, false));

    let r = row(&app, "name", &name).unwrap();
    run(&mut app, Act::Delete, r);
    settle(&mut app, &mut rx).await; // the impact is read before the confirmation
    let (lines, typed) = yes(&mut app);
    assert_eq!((typed.as_deref(), lines[0].clone()), (Some(name.as_str()), format!("Delete the Upstream Pulp definition {name}.")));
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("delete upstream {name}: done"));
    assert!(row(&app, "name", &name).is_none(), "the list reloads without it");
    assert!(c.get::<Value>(&href, &[]).await.unwrap_err().is_not_found());
}

/// The role assignments of user or group `owner`: (role, object).
async fn roles_of(c: &PulpClient, owner: &str) -> Vec<(String, Value)> {
    let p: pulp_tui::pulp::client::Page<Value> = c.list(&format!("{owner}roles/"), &[]).await.unwrap();
    p.results.iter().map(|r| (s(r, "role"), r["content_object"].clone())).collect()
}

/// Groups and roles through the TUI: Create a group; on its Roles screen Assign a model-level owner role (a powerful
/// role asks first) and Remove it; on a user's Roles screen Assign the viewer role of one repository (the user then
/// sees that repository) and Remove it (the user sees none); Delete the group.
#[tokio::test]
async fn group_create_roles_and_delete_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, name) = (client(), unique("admin-roles"));
    let pw = "Xq7!mLp2-zR9v";
    let user = create(&c, "users/", json!({ "username": name, "password": pw }), "/users/").await;
    let repo = create(&c, RepoType::File.repositories(), json!({ "name": name }), "/repositories/").await;
    let sees = async || {
        let u = user_client(&name, pw);
        u.get::<Value>(&u.api(RepoType::File.repositories()), &[]).await.unwrap()["count"].clone()
    };
    let (mut app, mut rx) = app().await;
    open_list(&mut app, &mut rx, Kind::Groups, &name).await;
    run(&mut app, Act::Create, Value::Null);
    fill(&mut app, &[("name", &name)]);
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("create group {name}: done"));
    let g = row(&app, "name", &name).unwrap_or_else(|| panic!("the list reloads with it: {:?}", app.list.items));
    let group = s(&g, "pulp_href");

    // Group roles: a model-level owner role asks first.
    let owner = "file.filerepository_owner";
    run(&mut app, Act::Roles, g);
    settle(&mut app, &mut rx).await;
    run(&mut app, Act::Create, Value::Null);
    fill(&mut app, &[("role", owner)]);
    let (lines, typed) = yes(&mut app);
    assert_eq!((typed, lines[0].clone()), (None, format!("{owner} grants owner/admin level permissions on ALL objects of its type.")));
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("assign {owner} to group {name}: done"));
    assert_eq!(roles_of(&c, &group).await, [(owner.to_string(), Value::Null)]);
    let r = row(&app, "role", owner).unwrap_or_else(|| panic!("the list reloads with it: {:?}", app.list.items));
    run(&mut app, Act::Delete, r);
    let (lines, _) = yes(&mut app);
    assert_eq!(lines, [format!("Remove role {owner} from group {name}.")]);
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("remove {owner}: done"));
    assert_eq!((roles_of(&c, &group).await, app.list.items.len()), (vec![], 0));
    app.back();

    // User roles: the viewer role of one repository; the user sees that repository only while it has the role.
    let viewer = "file.filerepository_viewer";
    open_list(&mut app, &mut rx, Kind::Users, &name).await;
    let u = row(&app, "username", &name).unwrap();
    run(&mut app, Act::Roles, u);
    settle(&mut app, &mut rx).await;
    assert_eq!(sees().await, 0);
    run(&mut app, Act::Create, Value::Null);
    fill(&mut app, &[("role", viewer), ("content_object", &repo)]);
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("assign {viewer} to user {name}: done"));
    assert_eq!(roles_of(&c, &user).await, [(viewer.to_string(), json!(repo))]);
    assert_eq!(sees().await, 1);
    let r = row(&app, "role", viewer).unwrap_or_else(|| panic!("the list reloads with it: {:?}", app.list.items));
    run(&mut app, Act::Delete, r);
    yes(&mut app);
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("remove {viewer}: done"));
    assert_eq!((roles_of(&c, &user).await, sees().await), (vec![], json!(0)));
    app.back();

    // Delete the group: the impact, the typed name.
    open_list(&mut app, &mut rx, Kind::Groups, &name).await;
    let g = row(&app, "name", &name).unwrap();
    run(&mut app, Act::Delete, g);
    settle(&mut app, &mut rx).await;
    let (lines, typed) = yes(&mut app);
    assert_eq!((typed.as_deref(), lines[0].clone()), (Some(name.as_str()), format!("Delete group {name}.")));
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("delete group {name}: done"));
    assert!(row(&app, "name", &name).is_none(), "the list reloads without it");
    assert!(c.get::<Value>(&group, &[]).await.unwrap_err().is_not_found());
    cleanup(&c, &[&repo, &user]).await;
}

/// An app that is not connected, on the config file `path` (its profiles as loaded), with a local store in memory.
async fn app_on_file(path: &std::path::Path) -> (App, UnboundedReceiver<Msg>) {
    let local = Arc::new(Store::open_in_memory().await.unwrap());
    let mut app = App::new(Config::load(path).unwrap(), path.into(), local, Err(NO_PROFILE.into()));
    let rx = app.rx.take().unwrap();
    (app, rx)
}

fn test_url() -> String {
    std::env::var("PULP_TUI_TEST_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into())
}

/// Server profiles through the TUI on a config file with a comment: Create (c) two profiles for pulp1 with a file
/// secret; Connect (o) to one; Edit (e) it to read-only: saved, and a mutation is refused at once; Delete (d) of the
/// profile of the session is refused, of the other one after the typed name. Every save keeps the comment.
#[tokio::test]
async fn server_profiles_create_edit_and_delete_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (path, pw) = (dir.path().join("config.toml"), dir.path().join("pw"));
    std::fs::write(&pw, std::env::var("PULP_TUI_TEST_PASSWORD").unwrap_or_else(|_| "password".into())).unwrap();
    let comment = "# operator note: keep this line";
    std::fs::write(&path, format!("{comment}\n")).unwrap();
    let (mut app, mut rx) = app_on_file(&path).await;
    let saved = || Config::load(&path).unwrap();
    let kept = || std::fs::read_to_string(&path).unwrap().contains(comment);
    let (url, secret) = (test_url(), format!("file:{}", pw.display()));

    // Create (c) two profiles: each in the file, the first one the default.
    open_list(&mut app, &mut rx, Kind::Profiles, "").await;
    for name in ["it-a", "it-b"] {
        run(&mut app, Act::Create, Value::Null);
        fill(&mut app, &[("name", name), ("url", &url), ("username", "admin"), ("secret", &secret)]);
        settle(&mut app, &mut rx).await;
        shown(&app, &format!("profile {name} saved"));
        let p = &saved().profiles[name];
        assert_eq!((p.url.as_str(), p.username.as_deref(), p.secret.as_deref()), (url.as_str(), Some("admin"), Some(secret.as_str())));
        assert!(row(&app, "name", name).is_some(), "the list reloads with it: {:?}", app.list.items);
    }
    assert!(saved().default_profile.as_deref() == Some("it-a") && kept(), "{}", std::fs::read_to_string(&path).unwrap());

    // Connect (o) to it-a.
    let a = row(&app, "name", "it-a").unwrap();
    run(&mut app, Act::Connect, a.clone());
    settle(&mut app, &mut rx).await;
    assert!(app.conn.is_some() && app.profile.as_deref() == Some("it-a"), "{:?}", texts(&app));
    shown(&app, "connected to it-a as admin");
    open_list(&mut app, &mut rx, Kind::Profiles, "").await; // Connect shows the Dashboard
    assert_eq!(row(&app, "name", "it-a").unwrap()["active"], true);

    // Edit (e) it-a to read-only: saved, and it applies to the session at once.
    run(&mut app, Act::Edit, a.clone());
    fill(&mut app, &[("read_only", "true")]);
    settle(&mut app, &mut rx).await;
    shown(&app, "profile it-a saved (read-only and production apply at once; the rest at the next Connect)");
    assert!(saved().profiles["it-a"].read_only && kept());
    assert!(s(&row(&app, "name", "it-a").unwrap(), "flags").contains("read-only"), "the list reloads: {:?}", app.list.items);
    app.list.kind = Kind::Repos(RepoType::File, false);
    run(&mut app, Act::Create, Value::Null);
    assert!(app.modal.is_none() && texts(&app).last().unwrap() == "read-only mode: this action changes the server", "{:?}", texts(&app));
    open_list(&mut app, &mut rx, Kind::Profiles, "").await;

    // Delete (d): not the profile of the session; it-b after the typed name.
    run(&mut app, Act::Delete, a);
    assert!(app.modal.is_none(), "refused at once");
    shown(&app, "refused: it-a is the profile of this session; connect to another profile first");
    let b = row(&app, "name", "it-b").unwrap();
    run(&mut app, Act::Delete, b);
    let (lines, typed) = yes(&mut app);
    assert_eq!((typed.as_deref(), lines[0].as_str()), (Some("it-b"), "Remove profile it-b from the config file."));
    settle(&mut app, &mut rx).await;
    assert!(row(&app, "name", "it-b").is_none(), "the list reloads without it");
    let left: Vec<String> = saved().profiles.into_keys().collect();
    assert!(left == ["it-a"] && kept(), "{left:?}");
}

/// Store secret (K) through the TUI into the system keyring, where one answers in this session: a profile whose secret
/// is `keyring` then connects with it. The keyring entry is removed afterwards.
#[cfg(feature = "keyring")]
#[tokio::test]
async fn store_secret_in_the_keyring_through_the_tui() {
    use pulp_tui::config::{KEYRING_SERVICE, Profile};
    if !live_enabled() {
        return;
    }
    let name = unique("it-keyring");
    let entry = |user: &str| keyring::Entry::new(KEYRING_SERVICE, user);
    // A probe in a thread of its own: a locked or missing keyring can fail or block, and the test must not hang.
    let (tx, answer) = std::sync::mpsc::channel();
    let probe = format!("{name}-probe");
    std::thread::spawn(move || {
        let r = entry(&probe).and_then(|e| e.set_password("probe").and_then(|_| e.delete_credential()));
        let _ = tx.send(r.map_err(|e| e.to_string()));
    });
    match answer.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return eprintln!("skipping: no usable system keyring in this session ({e})"),
        Err(_) => return eprintln!("skipping: the system keyring did not answer within 10 s (locked?)"),
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut cfg = Config::default();
    let p = Profile { url: test_url(), username: Some("admin".into()), secret: Some("keyring".into()), ..Default::default() };
    cfg.profiles.insert(name.clone(), p);
    cfg.save(&path).unwrap();
    let (mut app, mut rx) = app_on_file(&path).await;
    open_list(&mut app, &mut rx, Kind::Profiles, "").await;
    let r = row(&app, "name", &name).unwrap();
    run(&mut app, Act::StoreSecret, r);
    fill(&mut app, &[("secret", &std::env::var("PULP_TUI_TEST_PASSWORD").unwrap_or_else(|_| "password".into()))]);
    settle(&mut app, &mut rx).await;
    app.connect(&name);
    settle(&mut app, &mut rx).await;
    let (connected, notices) = (app.conn.is_some(), texts(&app));
    entry(&name).and_then(|e| e.delete_credential()).unwrap_or_else(|e| panic!("the secret was not stored: {e}; {notices:?}"));
    assert!(notices.contains(&"secret stored in keyring".to_string()) && connected, "{notices:?}");
}

/// Create (c) of each content guard type of the server other than core/header (it has its own test): the type form,
/// then the fields of that type; Pulp has the guard with them. The type choice offers only the types of the server.
#[tokio::test]
async fn content_guards_of_the_other_types_are_created_through_the_tui() {
    if !live_enabled() {
        return;
    }
    use pulp_tui::tui::form::FieldKind;
    let c = client();
    let name = unique("admin-guards");
    let dir = tempfile::tempdir().unwrap();
    let pem = dir.path().join("ca.pem");
    // A throw-away self-signed CA: Pulp checks the certificate with OpenSSL. The key is not kept.
    let made = std::process::Command::new("openssl")
        .args(["req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes", "-days", "1"])
        .args(["-subj", "/CN=pulp-tui test CA", "-keyout", "/dev/null", "-out"])
        .arg(&pem)
        .output()
        .unwrap();
    assert!(made.status.success(), "openssl: {}", String::from_utf8_lossy(&made.stderr));
    let (ca, pem) = (std::fs::read_to_string(&pem).unwrap(), pem.display().to_string());
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::ContentGuards;
    let caps = app.conn.as_ref().unwrap().caps.clone();
    let has = |t: &str| caps.has_endpoint(&format!("contentguards/{t}/"));

    run(&mut app, Act::Create, Value::Null);
    let Some(Modal::Form { form, .. }) = app.modal.take() else { panic!("no type form: {:?}", texts(&app)) };
    let FieldKind::Choice(offered) = form.fields[0].kind.clone() else { panic!("no type choice") };
    let all =
        ["core/rbac", "core/header", "core/envvar_header", "core/content_redirect", "core/composite", "certguard/x509", "certguard/rhsm"];
    assert_eq!(offered, all.into_iter().filter(|t| has(t)).collect::<Vec<_>>());

    // The type form, then the form of the type; the last notice, and the guard as Pulp has it.
    let make = async |app: &mut App, rx: &mut UnboundedReceiver<Msg>, t: &str, values: &[(&str, &str)]| {
        let gname = format!("{name}-{}", t.rsplit('/').next().unwrap());
        run(app, Act::Create, Value::Null);
        fill(app, &[("type", t)]);
        fill(app, &[&[("name", gname.as_str())], values].concat());
        settle(app, rx).await;
        let found: Option<Value> = ops::find_by_name(&c, &c.api(&format!("contentguards/{t}/")), &gname).await.unwrap();
        (texts(app).pop().unwrap_or_default(), found)
    };
    let made_ok = async |app: &mut App, rx: &mut UnboundedReceiver<Msg>, t: &str, values: &[(&str, &str)]| -> Value {
        let (said, found) = make(app, rx, t, values).await;
        assert!(said.ends_with(": done"), "{t}: {said}");
        found.unwrap_or_else(|| panic!("{t} not created: {said}"))
    };
    let rbac = made_ok(&mut app, &mut rx, "core/rbac", &[]).await;
    let redirect = made_ok(&mut app, &mut rx, "core/content_redirect", &[]).await;
    let composite = made_ok(&mut app, &mut rx, "core/composite", &[("guards", &s(&rbac, "pulp_href"))]).await;
    assert_eq!(composite["guards"], json!([rbac["pulp_href"]]));
    let mut made = vec![composite, rbac, redirect];
    for t in ["certguard/x509", "certguard/rhsm"].into_iter().filter(|t| has(t)) {
        let g = made_ok(&mut app, &mut rx, t, &[("ca_certificate", &pem)]).await;
        assert_eq!(g["ca_certificate"].as_str().map(str::trim), Some(ca.trim()), "{t}: the file's certificate");
        made.push(g);
    }
    // pulpcore 3.95 has no core/envvar_header; the type choice does not offer it (checked above).
    if has("core/envvar_header") {
        match make(&mut app, &mut rx, "core/envvar_header", &[("header_name", "X-Org"), ("env_var", "PULP_TUI_ORG")]).await {
            (_, Some(env)) => {
                assert_eq!((s(&env, "header_name"), s(&env, "env_var")), ("X-Org".into(), "PULP_TUI_ORG".into()));
                made.push(env);
            }
            // pulpcore allows no variable by default (`ENVVAR_HEADER_CONTENT_GUARD_ALLOWED_VARS = []`), so the form's
            // request gets Pulp's refusal of that variable.
            (said, None) => {
                let refused =
                    "rejected [400]: env_var: Environment variable 'PULP_TUI_ORG' is not in ENVVAR_HEADER_CONTENT_GUARD_ALLOWED_VARS.";
                assert!(said.ends_with(refused), "{said}");
            }
        }
    }
    let hrefs: Vec<String> = made.iter().map(|g| s(g, "pulp_href")).collect();
    cleanup(&c, &hrefs.iter().map(String::as_str).collect::<Vec<_>>()).await;
}

/// Track (a) and Watch (W) on the Task groups screen (audit E01): the ACS refresh group of this test is tracked under
/// its description, Watch opens its live view, and the Task Center polls it to its end; the view follows.
#[tokio::test]
async fn task_groups_screen_tracks_and_watches_a_group() {
    if !live_enabled() {
        return;
    }
    use pulp_tui::pulp::Mutation;
    let c = client();
    let name = unique("admin-track");
    let body = json!({ "name": name, "url": format!("{}/file/PULP_MANIFEST", upstream()), "policy": "on_demand" });
    let remote = create(&c, RepoType::File.remotes(), body, "/remotes/").await;
    let acs = create(&c, RepoType::File.acs(), json!({ "name": name, "remote": remote }), "/acs/").await;
    let Mutation::TaskGroup(group) = c.post(&format!("{acs}refresh/"), &json!({})).await.unwrap() else { panic!("no task group") };

    let (mut app, mut rx) = app().await;
    app.open(Screen::List(Kind::TaskGroups));
    settle(&mut app, &mut rx).await;
    let row = app.list.items.iter().find(|g| g["pulp_href"] == group.as_str()).cloned();
    let row = row.unwrap_or_else(|| panic!("the group is not on the first page: {:?}", app.list.error));
    let description = s(&row, "description");
    run(&mut app, Act::TrackTask, row.clone());
    shown(&app, "tracking task");
    assert_eq!(app.tasks.items.iter().find(|t| t.href == group).map(|t| t.label.clone()), Some(description.clone()));
    run(&mut app, Act::WatchTask, row);
    let Some(Modal::Text { title, watch, .. }) = &app.modal else { panic!("no watch: {:?}", texts(&app)) };
    assert_eq!((title.as_str(), watch.as_deref()), (format!("Watch task {description}").as_str(), Some(group.as_str())));

    settle_tasks(&mut app, &mut rx).await;
    assert_eq!(app.tasks.items.iter().find(|t| t.href == group).unwrap().state, "completed", "{:?}", texts(&app));
    shown(&app, &format!("task completed: {description}"));
    let Some(Modal::Text { lines, .. }) = &app.modal else { panic!("the watch closed: {:?}", texts(&app)) };
    assert!(lines.iter().any(|l| l.contains("completed")), "the view follows the group: {lines:?}");
    cleanup(&c, &[&acs, &remote]).await;
}

/// The Task Center keeps its task references in the local file: a Sync (s) submitted in one session that quits before
/// a poll is re-attached by the Connect of the next session on the same file and polled there to its end. A third
/// session shows it as ended and does not re-attach it.
#[tokio::test]
async fn task_center_reattaches_a_task_of_a_previous_session() {
    if !live_enabled() {
        return;
    }
    use pulp_tui::pulp::Mutation;
    let c = client();
    let t = RepoType::File;
    let name = unique("admin-reattach");
    let body = json!({ "name": name, "url": format!("{}/file/PULP_MANIFEST", upstream()), "policy": "immediate" });
    let remote = create(&c, t.remotes(), body, "/remotes/").await;
    let repo = create(&c, t.repositories(), json!({ "name": name, "remote": remote }), "/repositories/").await;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pulp-tui.db");
    // A session on the local file `db`: Connect reads its task references.
    let session = async || {
        let (mut app, mut rx) = app().await;
        app.store = Arc::new(Store::open(&db).await.unwrap());
        app.connect("pulp1");
        settle(&mut app, &mut rx).await;
        (app, rx)
    };

    let (mut app, mut rx) = session().await;
    app.list.kind = Kind::Repos(t, false);
    run(&mut app, Act::Sync, c.get::<Value>(&repo, &[]).await.unwrap());
    settle(&mut app, &mut rx).await;
    fill(&mut app, &[]);
    settle(&mut app, &mut rx).await; // the POST, then the stored reference
    let first = app.tasks.items.first().unwrap_or_else(|| panic!("no task: {:?}", texts(&app)));
    let (href, label) = (first.href.clone(), first.label.clone());
    assert_eq!(first.state, "waiting", "quit before a poll");
    drop((app, rx));
    ops::settle(&c, Mutation::Task(href.clone()), &no_progress).await.unwrap(); // the task ends while no session runs

    let (mut app, mut rx) = session().await;
    shown(&app, "re-attached to 1 unfinished task(s) from a previous session");
    let state = |app: &App| app.tasks.items.iter().find(|r| r.href == href).map(|r| (r.label.clone(), r.state.clone()));
    assert_eq!(state(&app), Some((label.clone(), "waiting".into())));
    settle_tasks(&mut app, &mut rx).await;
    shown(&app, &format!("task completed: {label}"));
    drop((app, rx));

    let (app, _rx) = session().await;
    assert!(!texts(&app).iter().any(|t| t.starts_with("re-attached")), "{:?}", texts(&app));
    assert_eq!(state(&app), Some((label, "completed".into())), "stored as ended");
    cleanup(&c, &[&repo, &remote]).await;
}
