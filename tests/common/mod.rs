//! Shared helpers for live tests against a running Pulp (see tests/env/pulp-env.sh).
//! Live tests are skipped unless `PULP_TUI_LIVE=1`.
#![allow(dead_code)]

use pulp_tui::config::{Config, Profile, Settings};
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{Mutation, PulpClient, RepoType};
use pulp_tui::store::Store;
use pulp_tui::tui::app::{App, Modal, Msg, NO_PROFILE, Screen};
use pulp_tui::tui::kinds::Kind;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;

pub fn live_enabled() -> bool {
    if std::env::var("PULP_TUI_LIVE").as_deref() == Ok("1") {
        true
    } else {
        eprintln!("skipping live test: set PULP_TUI_LIVE=1 (and start tests/env/pulp-env.sh up)");
        false
    }
}

/// Client for a test instance. The password is passed through a temporary secret file,
/// the same mechanism used in containers.
pub fn client_for(url: &str, name: &str) -> PulpClient {
    let dir = std::env::temp_dir().join(format!("pulp-tui-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let secret = dir.join(name);
    std::fs::write(&secret, std::env::var("PULP_TUI_TEST_PASSWORD").unwrap_or_else(|_| "password".into())).unwrap();
    let p = Profile {
        url: url.into(),
        username: Some("admin".into()),
        secret: Some(format!("file:{}", secret.display())),
        ..Default::default()
    };
    let s = Settings { poll_min_ms: 300, poll_max_ms: 2000, upload_chunk_mb: 1, ..Default::default() };
    PulpClient::new(name, &p, &s).unwrap()
}

pub fn client() -> PulpClient {
    client_for(&std::env::var("PULP_TUI_TEST_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into()), "pulp1")
}

/// Upstream fixture repositories as seen from inside the Pulp container.
pub fn upstream() -> String {
    std::env::var("PULP_TUI_TEST_UPSTREAM").unwrap_or_else(|_| "http://host.containers.internal:8000".into())
}

pub fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/fixtures")
}

pub fn unique(prefix: &str) -> String {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    format!("{prefix}-{}", n % 1_000_000_000_000)
}

pub async fn create(c: &PulpClient, path: &str, body: Value, needle: &str) -> String {
    ops::create(c, &c.api(path), &body, needle, &no_progress).await.unwrap()
}

pub async fn synced_repo(c: &PulpClient, t: RepoType, name: &str, upstream_path: &str, policy: &str) -> (String, String) {
    let url = format!("{}/{upstream_path}", upstream());
    let remote = create(c, t.remotes(), json!({ "name": name, "url": url, "policy": "immediate" }), "/remotes/").await;
    let repo = create(c, t.repositories(), json!({ "name": name, "remote": remote }), "/repositories/").await;
    let m = ops::sync(c, t, &repo, None, policy).await.unwrap();
    ops::settle(c, m, &no_progress).await.unwrap();
    (repo, remote)
}

pub async fn latest(c: &PulpClient, repo: &str) -> String {
    let r: Value = c.get(repo, &[]).await.unwrap();
    r["latest_version_href"].as_str().unwrap().to_string()
}

pub async fn served(c: &PulpClient, url: &str) -> (u16, Vec<u8>) {
    c.fetch_content(url, 64 << 20).await.unwrap()
}

/// An app connected to the test Pulp as profile `pulp1`, with its jobs done: tests drive the same forms,
/// confirmations and jobs that an operator uses.
pub async fn app() -> (App, UnboundedReceiver<Msg>) {
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

pub async fn settle(app: &mut App, rx: &mut UnboundedReceiver<Msg>) {
    while !app.jobs.is_empty() {
        let m = rx.recv().await.unwrap();
        app.handle_msg(m);
    }
}

pub fn texts(app: &App) -> Vec<String> {
    app.notices.iter().map(|n| n.text.clone()).collect()
}

/// Wait until the newest task of the Task Center (the last one that the TUI submitted) completes.
pub async fn settle_submitted(c: &PulpClient, app: &App) {
    let href = app.tasks.items.first().unwrap_or_else(|| panic!("no task: {:?}", texts(app))).href.clone();
    ops::settle(c, Mutation::Task(href), &no_progress).await.unwrap();
}

/// Let the jobs end (one submits a task), then poll the Task Center as the UI loop does until every tracked task is
/// final, then let the jobs (the list reload that a finished task starts) end.
pub async fn settle_tasks(app: &mut App, rx: &mut UnboundedReceiver<Msg>) {
    use std::time::{Duration, Instant};
    settle(app, rx).await;
    let deadline = Instant::now() + Duration::from_secs(300);
    while app.tasks.items.iter().any(|t| !t.is_final()) {
        assert!(Instant::now() < deadline, "the tasks did not end: {:?}", texts(app));
        app.tick();
        if let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
            app.handle_msg(m);
        }
    }
    settle(app, rx).await;
}

/// Answer yes to the open confirmation; returns its lines and the typed name it asks for.
pub fn yes(app: &mut App) -> (Vec<String>, Option<String>) {
    let Some(Modal::Confirm { lines, typed, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm: {:?}", texts(app)) };
    yes(app);
    (lines, typed)
}

/// Open list `kind` filtered by `filter` (as `/` does) and load it.
pub async fn open_list(app: &mut App, rx: &mut UnboundedReceiver<Msg>, kind: Kind, filter: &str) {
    app.open(Screen::List(kind));
    app.list.filter = filter.into();
    app.reload();
    settle(app, rx).await;
    assert_eq!(app.list.error, None);
}

/// The row of the open list whose `key` is `value`.
pub fn row(app: &App, key: &str, value: &str) -> Option<Value> {
    app.list.items.iter().find(|r| r[key] == value).cloned()
}

/// Set `values` in the open form and submit it.
pub fn fill(app: &mut App, values: &[(&str, &str)]) {
    let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form: {:?}", texts(app)) };
    for (k, v) in values {
        form.fields.iter_mut().find(|f| f.key == *k).unwrap_or_else(|| panic!("no field {k}")).value = v.to_string();
    }
    submit(app, &form).unwrap();
}

/// Best-effort removal; tests use unique names so leftovers never collide.
pub async fn cleanup(c: &PulpClient, hrefs: &[&str]) {
    for h in hrefs {
        if let Ok(m) = c.delete(h).await {
            let _ = ops::settle(c, m, &no_progress).await;
        }
    }
}
