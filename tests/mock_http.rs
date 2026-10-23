//! Transport behavior against a mock HTTP server: pagination, auth failures, malformed
//! responses, retry rules (reads only), timeouts, task polling, capability negotiation,
//! read-only mode and size limits.

use pulp_tui::config::{AuthMethod, Profile, Settings};
use pulp_tui::pulp::models::Task;
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{Capabilities, Feature, PulpClient, PulpError};
use serde_json::{Value, json};
use std::time::Duration;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const API: &str = "/pulp/api/v3/";

fn client(server: &MockServer, f: impl FnOnce(&mut Profile, &mut Settings)) -> PulpClient {
    let dir = tempfile::tempdir().unwrap().keep();
    std::fs::write(dir.join("pw"), "s3cret").unwrap();
    let mut p = Profile {
        url: server.uri(),
        auth: AuthMethod::Basic,
        username: Some("admin".into()),
        secret: Some(format!("file:{}", dir.join("pw").display())),
        timeout_secs: 5,
        ..Default::default()
    };
    let mut s = Settings { poll_min_ms: 100, poll_max_ms: 200, ..Default::default() };
    f(&mut p, &mut s);
    PulpClient::new("mock", &p, &s).unwrap()
}

#[tokio::test]
async fn pagination_follows_next_links_with_auth() {
    let server = MockServer::start().await;
    let next = format!("{}{API}repositories/?limit=2&offset=2", server.uri());
    Mock::given(method("GET"))
        .and(path(format!("{API}repositories/")))
        .and(query_param("offset", "2"))
        .and(header("authorization", "Basic YWRtaW46czNjcmV0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "count": 3, "next": null, "results": [{"name": "c"}] })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API}repositories/")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "count": 3, "next": next, "results": [{"name": "a"}, {"name": "b"}] })),
        )
        .mount(&server)
        .await;
    let c = client(&server, |_, _| {});
    let all: Vec<Value> = c.list_all(&c.api("repositories/"), &[], 100).await.unwrap();
    assert_eq!(all.iter().map(|v| v["name"].as_str().unwrap()).collect::<Vec<_>>(), ["a", "b", "c"]);
}

#[tokio::test]
async fn next_link_to_foreign_origin_is_rejected() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "count": 2, "next": "https://evil.example.com/pulp/api/v3/x/", "results": [1] })),
        )
        .mount(&server)
        .await;
    let c = client(&server, |_, _| {});
    let err = c.list_all::<Value>(&c.api("x/"), &[], 100).await.unwrap_err();
    assert!(matches!(err, PulpError::BadHref(_)), "{err:?}");
}

#[tokio::test]
async fn auth_failure_and_permission_errors_are_clear() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API}a/")))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"detail": "Invalid username/password."})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{API}b/")))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"detail": "You do not have permission to perform this action."})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{API}c/")))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({"name": ["This field must be unique."]})))
        .mount(&server)
        .await;
    let c = client(&server, |_, _| {});
    let e = c.get::<Value>(&c.api("a/"), &[]).await.unwrap_err();
    assert_eq!(e.status(), Some(401));
    assert!(e.to_string().contains("authentication failed") && e.to_string().contains("Invalid username"), "{e}");
    let e = c.post(&c.api("b/"), &json!({})).await.unwrap_err();
    assert!(e.to_string().starts_with("permission denied [403]"), "{e}");
    let e = c.post(&c.api("c/"), &json!({})).await.unwrap_err();
    assert_eq!(e.to_string(), "conflict [409]: name: This field must be unique.");
}

#[tokio::test]
async fn malformed_json_is_a_decode_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_string("{not json")).mount(&server).await;
    let c = client(&server, |_, _| {});
    assert!(matches!(c.get::<Value>(&c.api("x/"), &[]).await.unwrap_err(), PulpError::Decode(_)));
    // Valid JSON with the wrong shape is also a decode error, not a panic.
    let server2 = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"unexpected": true}))).mount(&server2).await;
    let c2 = client(&server2, |_, _| {});
    assert!(matches!(c2.get::<Task>(&c2.api("tasks/1/"), &[]).await.unwrap_err(), PulpError::Decode(_)));
}

#[tokio::test]
async fn reads_are_retried_but_mutations_are_not() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(503)).up_to_n_times(2).expect(2).mount(&server).await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true}))).expect(1).mount(&server).await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(503)).expect(1).mount(&server).await;
    let c = client(&server, |_, _| {});
    assert_eq!(c.get::<Value>(&c.api("x/"), &[]).await.unwrap()["ok"], true);
    let e = c.post(&c.api("x/"), &json!({})).await.unwrap_err();
    assert_eq!(e.status(), Some(503));
    server.verify().await;
}

#[tokio::test]
async fn rate_limit_honors_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_json(json!(1))).mount(&server).await;
    let c = client(&server, |_, _| {});
    let t = std::time::Instant::now();
    assert_eq!(c.get::<Value>(&c.api("x/"), &[]).await.unwrap(), json!(1));
    assert!(t.elapsed() >= Duration::from_millis(900));
}

#[tokio::test]
async fn timeouts_are_reported() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(3))).mount(&server).await;
    let c = client(&server, |p, _| p.timeout_secs = 1);
    assert!(matches!(c.post(&c.api("slow/"), &json!({})).await.unwrap_err(), PulpError::Timeout));
}

#[tokio::test]
async fn unreachable_server_is_a_network_error() {
    let p = Profile { url: "http://127.0.0.1:9".into(), auth: AuthMethod::None, timeout_secs: 2, ..Default::default() };
    let c = PulpClient::new("down", &p, &Settings::default()).unwrap();
    let e = c.post(&c.api("x/"), &json!({})).await.unwrap_err();
    assert!(matches!(e, PulpError::Network(_)), "{e:?}");
}

#[tokio::test]
async fn task_polling_until_completion_or_failure() {
    let server = MockServer::start().await;
    let running = json!({ "pulp_href": format!("{API}tasks/1/"), "state": "running", "name": "sync" });
    let done = json!({ "pulp_href": format!("{API}tasks/1/"), "state": "completed", "name": "sync", "created_resources": [format!("{API}repositories/file/file/r/versions/2/")] });
    Mock::given(method("GET"))
        .and(path(format!("{API}tasks/1/")))
        .respond_with(ResponseTemplate::new(200).set_body_json(running))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API}tasks/1/")))
        .respond_with(ResponseTemplate::new(200).set_body_json(done))
        .mount(&server)
        .await;
    let failed = json!({ "pulp_href": format!("{API}tasks/2/"), "state": "failed", "error": {"description": "404 upstream"} });
    Mock::given(method("GET"))
        .and(path(format!("{API}tasks/2/")))
        .respond_with(ResponseTemplate::new(200).set_body_json(failed))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{API}repositories/file/file/r/sync/")))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({"task": format!("{API}tasks/1/")})))
        .mount(&server)
        .await;

    let c = client(&server, |_, _| {});
    let seen = std::sync::Mutex::new(vec![]);
    let m = c.post(&format!("{API}repositories/file/file/r/sync/"), &json!({})).await.unwrap();
    let t = ops::settle(&c, m, &|t: &Task| seen.lock().unwrap().push(t.state.clone())).await.unwrap().unwrap();
    assert_eq!(t.created_matching("/versions/").unwrap(), format!("{API}repositories/file/file/r/versions/2/"));
    assert_eq!(*seen.lock().unwrap(), ["running", "completed"], "updates are reported only on change");
    match ops::wait_task(&c, &format!("{API}tasks/2/"), &no_progress).await.unwrap_err() {
        PulpError::TaskFailed { state, message, .. } => assert_eq!((state.as_str(), message.as_str()), ("failed", "404 upstream")),
        e => panic!("{e:?}"),
    }
}

#[tokio::test]
async fn capability_negotiation_tolerates_old_or_restricted_servers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API}status/")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({ "versions": [{"component": "core", "version": "3.21.0"}, {"component": "file", "version": "1.11.0"}] }),
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path(format!("{API}login/"))).respond_with(ResponseTemplate::new(404)).mount(&server).await;
    Mock::given(method("GET")).and(path(format!("{API}docs/api.json"))).respond_with(ResponseTemplate::new(403)).mount(&server).await;
    let c = client(&server, |_, _| {});
    let caps = Capabilities::detect(&c).await.unwrap();
    assert!(caps.user.is_none());
    assert!(caps.openapi_error.is_some());
    assert!(caps.supports(Feature::File) && !caps.supports(Feature::Rpm));
    let e = caps.require(Feature::NativeReplication).unwrap_err();
    assert!(e.to_string().contains("not supported by this server: Upstream Pulp replication"), "{e}");
}

#[tokio::test]
async fn current_user_without_login_endpoint_is_found_by_username() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API}status/")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "versions": [{"component": "core", "version": "3.63.32"}] })))
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path(format!("{API}login/"))).respond_with(ResponseTemplate::new(404)).mount(&server).await;
    Mock::given(method("GET"))
        .and(path(format!("{API}users/")))
        .and(query_param("username", "admin"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "count": 1, "next": null, "results": [{ "pulp_href": format!("{API}users/1/"), "username": "admin", "is_staff": true }]
        })))
        .mount(&server)
        .await;
    let caps = Capabilities::detect(&client(&server, |_, _| {})).await.unwrap();
    let u = caps.user.expect("user from users/?username=");
    assert_eq!((u.username.as_str(), u.href.as_str(), u.is_staff), ("admin", format!("{API}users/1/").as_str(), Some(true)));
}

#[tokio::test]
async fn read_only_blocks_mutations_without_contacting_the_server() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(201)).expect(0).mount(&server).await;
    Mock::given(method("DELETE")).respond_with(ResponseTemplate::new(204)).expect(0).mount(&server).await;
    let c = client(&server, |p, _| p.read_only = true);
    assert!(matches!(c.post(&c.api("x/"), &json!({})).await.unwrap_err(), PulpError::ReadOnly(_)));
    assert!(matches!(c.delete(&c.api("x/1/")).await.unwrap_err(), PulpError::ReadOnly(_)));
    assert!(matches!(c.post_form(&c.api("x/"), &[], None).await.unwrap_err(), PulpError::ReadOnly(_)));
    server.verify().await;
}

#[tokio::test]
async fn oversized_responses_are_rejected() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(2 * 1024 * 1024))).mount(&server).await;
    let c = client(&server, |_, s| s.max_response_mb = 1);
    assert!(matches!(c.get::<Value>(&c.api("big/"), &[]).await.unwrap_err(), PulpError::TooLarge(1)));
}
