//! Administration screens against a live Pulp, driven through the TUI actions: the same
//! forms, confirmations and jobs that an operator uses. Nothing here is server-wide
//! destructive: a purge uses `finished_before` 2000-01-01, which matches no task.

mod common;

use common::*;
use pulp_tui::config::{Config, Profile};
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{PulpClient, RepoType};
use pulp_tui::store::Store;
use pulp_tui::tui::actions::run;
use pulp_tui::tui::app::{App, Modal, Msg, NO_PROFILE, Screen};
use pulp_tui::tui::kinds::{Act, Kind};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;

/// An app connected to the test Pulp as profile `pulp1`, with its jobs done.
async fn app() -> (App, UnboundedReceiver<Msg>) {
    let url = std::env::var("PULP_TUI_TEST_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let dir = tempfile::tempdir().unwrap().keep();
    std::fs::write(dir.join("pw"), std::env::var("PULP_TUI_TEST_PASSWORD").unwrap_or_else(|_| "password".into())).unwrap();
    let secret = Some(format!("file:{}", dir.join("pw").display()));
    let mut cfg = Config::default();
    cfg.profiles.insert("pulp1".into(), Profile { url, username: Some("admin".into()), secret, ..Default::default() });
    let local = Arc::new(Store::open_in_memory().await.unwrap());
    let mut app = App::new(cfg, "/dev/null".into(), local, Err(NO_PROFILE.into()));
    let mut rx = app.rx.take().unwrap();
    app.connect("pulp1");
    settle(&mut app, &mut rx).await;
    assert!(app.conn.is_some(), "not connected: {:?}", texts(&app));
    (app, rx)
}

async fn settle(app: &mut App, rx: &mut UnboundedReceiver<Msg>) {
    while !app.jobs.is_empty() {
        let m = rx.recv().await.unwrap();
        app.handle_msg(m);
    }
}

fn texts(app: &App) -> Vec<String> {
    app.notices.iter().map(|n| n.text.clone()).collect()
}

/// Set `values` in the open form and submit it.
fn fill(app: &mut App, values: &[(&str, &str)]) {
    let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form: {:?}", texts(app)) };
    for (k, v) in values {
        form.fields.iter_mut().find(|f| f.key == *k).unwrap_or_else(|| panic!("no field {k}")).value = v.to_string();
    }
    submit(app, &form).unwrap();
}

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

/// Answer yes to the open confirmation; returns its lines and the typed name it asks for.
fn yes(app: &mut App) -> (Vec<String>, Option<String>) {
    let Some(Modal::Confirm { lines, typed, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm: {:?}", texts(app)) };
    yes(app);
    (lines, typed)
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
    let ex2 = ServerExport { format: 1, remotes: vec![entry(mine(&ex.remotes))], repositories: vec![p2] };
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
        ops::upload(&c, t, &path, Some(&format!("{i}.txt")), Some(&repo), &|_, _| {}, &no_progress).await.unwrap();
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
