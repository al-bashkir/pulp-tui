//! Failure handling against a live Pulp: server-side task cancellation (waiting and
//! running tasks) and TLS errors.

mod common;

use common::*;
use pulp_tui::config::{AuthMethod, Profile, Settings};
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{Mutation, PulpClient, PulpError, RepoType};
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
