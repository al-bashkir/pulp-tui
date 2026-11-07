//! Failure handling against a live Pulp: server-side task cancellation (waiting and
//! running tasks) and TLS errors.

mod common;

use common::*;
use pulp_tui::config::{AuthMethod, Profile, Settings};
use pulp_tui::pulp::models::Task;
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{Mutation, PulpClient, PulpError, RepoType};
use pulp_tui::tui::actions::run;
use pulp_tui::tui::app::App;
use pulp_tui::tui::kinds::{Act, Kind, s};
use serde_json::json;

#[tokio::test]
async fn cancel_waiting_and_running_tasks() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let name = unique("cancel");
    // An upstream that accepts connections but never answers keeps the sync running.
    let hang = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = hang.local_addr().unwrap().port();
    let host = upstream().rsplit_once(':').map(|(h, _)| h.to_string()).unwrap();
    let remote = create(
        &c,
        RepoType::File.remotes(),
        json!({ "name": name, "url": format!("{host}:{port}/PULP_MANIFEST"), "sock_read_timeout": 10.0 }),
        "/remotes/",
    )
    .await;
    let repo = create(&c, RepoType::File.repositories(), json!({ "name": name, "remote": remote }), "/repositories/").await;
    let Mutation::Task(running) = ops::sync(&c, RepoType::File, &repo, None, "additive").await.unwrap() else { panic!() };
    for _ in 0..50 {
        if c.get::<pulp_tui::pulp::models::Task>(&running, &[]).await.unwrap().state == "running" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    // A second task on the same repository waits for the lock.
    let Mutation::Task(waiting) = c.post(&format!("{repo}modify/"), &json!({ "add_content_units": [] })).await.unwrap() else { panic!() };

    // The cancel request is accepted; Pulp decides the final state, which is reported as is.
    for t in [&waiting, &running] {
        let Mutation::Object(v) = c.patch(t, &json!({ "state": "canceled" })).await.unwrap() else { panic!() };
        assert!(matches!(v["state"].as_str(), Some("canceling" | "canceled")), "{v}");
    }
    let state = |r: Result<pulp_tui::pulp::models::Task, PulpError>| match r {
        Err(PulpError::TaskFailed { state, .. }) => state,
        other => panic!("{other:?}"),
    };
    assert_eq!(state(ops::wait_task(&c, &waiting, &no_progress).await), "canceled");
    // A task blocked in a network read may finish as failed before the worker sees the cancel.
    assert!(matches!(state(ops::wait_task(&c, &running, &no_progress).await).as_str(), "canceled" | "failed"));
    drop(hang);
    // No new repository version was created.
    assert!(latest(&c, &repo).await.ends_with("/versions/0/"));
    // Cancelling a completed task is refused by Pulp (reported, not hidden).
    let Mutation::Task(done) = c.post(&format!("{repo}modify/"), &json!({ "add_content_units": [] })).await.unwrap() else { panic!() };
    ops::wait_task(&c, &done, &no_progress).await.unwrap();
    let e = c.patch(&done, &json!({ "state": "canceled" })).await.unwrap_err();
    assert_eq!(e.status(), Some(409), "{e}");
    assert!(e.to_string().ends_with("is already completed"), "{e}");
    cleanup(&c, &[&repo, &remote]).await;
}

#[tokio::test]
async fn tls_errors_are_reported_and_not_downgraded() {
    if !live_enabled() {
        return;
    }
    // HTTPS to a plain-HTTP port: the handshake fails; there is no silent fallback to HTTP.
    let p = Profile { url: "https://localhost:8080".into(), auth: AuthMethod::None, timeout_secs: 5, ..Default::default() };
    let c = PulpClient::new("tls", &p, &Settings::default()).unwrap();
    let e = c.get::<serde_json::Value>(&c.api("status/"), &[]).await.unwrap_err();
    assert!(matches!(e, PulpError::Network(_)), "{e:?}");
    // An unreadable CA bundle is a configuration error before any request.
    let p = Profile { ca_cert: Some("/nonexistent/ca.pem".into()), ..p };
    assert!(matches!(PulpClient::new("tls", &p, &Settings::default()).unwrap_err(), PulpError::Config(_)));
}

/// Server Tasks through the TUI, filtered by a repository whose sync hangs (an upstream that never answers) and a
/// second task that waits for it: Track (a) of the running task puts it in the Task Center; Cancel (X) of the waiting
/// task after the confirmation; Cancel of the running one: the Task Center row ends canceled (or failed, when the worker
/// ends the network read first). No version is made.
#[tokio::test]
async fn track_and_cancel_server_tasks_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, name) = (client(), unique("tuicancel"));
    let hang = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let host = upstream().rsplit_once(':').map(|(h, _)| h.to_string()).unwrap();
    let url = format!("{host}:{}/PULP_MANIFEST", hang.local_addr().unwrap().port());
    let remote = create(&c, RepoType::File.remotes(), json!({ "name": name, "url": url, "sock_read_timeout": 10.0 }), "/remotes/").await;
    let repo = create(&c, RepoType::File.repositories(), json!({ "name": name, "remote": remote }), "/repositories/").await;
    let Mutation::Task(running) = ops::sync(&c, RepoType::File, &repo, None, "additive").await.unwrap() else { panic!() };
    for _ in 0..50 {
        if c.get::<Task>(&running, &[]).await.unwrap().state == "running" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let Mutation::Task(waiting) = c.post(&format!("{repo}modify/"), &json!({ "add_content_units": [] })).await.unwrap() else { panic!() };

    let (mut app, mut rx) = app().await;
    open_list(&mut app, &mut rx, Kind::ServerTasks(vec![("reserved_resources", repo.clone())]), "").await;
    let task_row = |app: &App, href: &str| row(app, "pulp_href", href).unwrap_or_else(|| panic!("no task {href}: {:?}", app.list.items));
    let (r_running, r_waiting) = (task_row(&app, &running), task_row(&app, &waiting));
    assert_eq!((s(&r_running, "state"), s(&r_waiting, "state")), ("running".into(), "waiting".into()));

    // Track (a) the running task.
    run(&mut app, Act::TrackTask, r_running.clone());
    shown(&app, "tracking task");
    assert!(app.tasks.items.iter().any(|t| t.href == running && !t.is_final()), "in the Task Center");

    // Cancel (X) the waiting task: the confirmation, then Pulp's state.
    run(&mut app, Act::CancelTask, r_waiting);
    let (lines, typed) = yes(&mut app);
    assert_eq!((typed, lines[0].clone()), (None, format!("Ask Pulp to cancel task {waiting}.")));
    settle(&mut app, &mut rx).await;
    let said = texts(&app).into_iter().find_map(|n| n.strip_prefix("cancel requested; Pulp reports state ").map(String::from));
    assert!(matches!(said.as_deref(), Some("canceling" | "canceled")), "{:?}", texts(&app));
    let state = |r: Result<Task, PulpError>| match r {
        Err(PulpError::TaskFailed { state, .. }) => state,
        other => panic!("{other:?}"),
    };
    assert_eq!(state(ops::wait_task(&c, &waiting, &no_progress).await), "canceled");

    // Cancel (X) the running task: its Task Center row ends.
    run(&mut app, Act::CancelTask, r_running);
    yes(&mut app);
    settle_tasks(&mut app, &mut rx).await;
    let t = app.tasks.items.iter().find(|t| t.href == running).unwrap();
    assert!(t.is_final() && matches!(t.state.as_str(), "canceled" | "failed"), "{t:?}");
    assert!(texts(&app).iter().any(|n| n.starts_with(&format!("task {}: ", t.state))), "{:?}", texts(&app));
    drop(hang);
    assert!(latest(&c, &repo).await.ends_with("/versions/0/"), "no new version");
    cleanup(&c, &[&repo, &remote]).await;
}
