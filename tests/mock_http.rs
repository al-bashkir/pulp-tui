//! Transport behavior against a mock HTTP server: pagination, auth failures, malformed
//! responses, retry rules (reads only), timeouts, task polling, capability negotiation,
//! read-only mode and size limits.

use pulp_tui::config::{AuthMethod, Profile, Settings};
use pulp_tui::pulp::models::Task;
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{Capabilities, Feature, Mutation, PulpClient, PulpError, RepoType, deb};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;
use wiremock::matchers::{body_json, body_partial_json, header, method, path, query_param};
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
async fn task_polling_survives_a_short_outage_and_never_calls_an_unread_task_failed() {
    let server = MockServer::start().await;
    // More 503s than the client retries for one GET (4 attempts).
    Mock::given(method("GET"))
        .and(path(format!("{API}tasks/3/")))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(5)
        .mount(&server)
        .await;
    let done = json!({ "pulp_href": format!("{API}tasks/3/"), "state": "completed", "name": "publish" });
    Mock::given(method("GET"))
        .and(path(format!("{API}tasks/3/")))
        .respond_with(ResponseTemplate::new(200).set_body_json(done))
        .mount(&server)
        .await;
    let c = client(&server, |_, _| {});
    assert_eq!(ops::wait_task(&c, &format!("{API}tasks/3/"), &no_progress).await.unwrap().state, "completed");

    Mock::given(method("GET")).and(path(format!("{API}tasks/4/"))).respond_with(ResponseTemplate::new(403)).mount(&server).await;
    let e = ops::wait_task(&c, &format!("{API}tasks/4/"), &no_progress).await.unwrap_err();
    assert!(matches!(e, PulpError::TaskUnknown { .. }), "{e}");
    assert!(e.to_string().contains("it may still be running (see Tasks)"), "{e}");
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

#[tokio::test]
async fn logs_never_contain_credentials() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{API}remotes/rpm/rpm/")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "name": "r1", "pulp_href": "/x/" })))
        .mount(&server)
        .await;
    let c = client(&server, |_, _| {});
    c.post(&c.api("remotes/rpm/rpm/"), &json!({ "name": "r1", "url": "https://u/", "password": "hunter2" })).await.unwrap();
    let rows = pulp_tui::session_log::rows(true);
    let row = rows.iter().find(|r| r["summary"].as_str().unwrap().starts_with("POST /pulp/api/v3/remotes/rpm/rpm/")).unwrap();
    let text = pulp_tui::session_log::detail(row["id"].as_u64().unwrap()).unwrap().join("\n");
    assert!(text.contains("\"r1\""));
    for bad in ["hunter2", "s3cret", "Basic ", "YWRtaW46czNjcmV0"] {
        assert!(!text.contains(bad), "{bad} in {text}");
    }
}

/// Detail text of the newest logged row whose summary starts with `prefix`.
fn logged(prefix: &str) -> String {
    let rows = pulp_tui::session_log::rows(true);
    let row =
        rows.iter().find(|r| r["summary"].as_str().unwrap().starts_with(prefix)).unwrap_or_else(|| panic!("no row {prefix}: {rows:?}"));
    pulp_tui::session_log::detail(row["id"].as_u64().unwrap()).unwrap().join("\n")
}

#[tokio::test]
async fn logs_every_exit_path() {
    let server = MockServer::start().await;
    let get = |p: &str, r: ResponseTemplate| Mock::given(method("GET")).and(path(format!("{API}{p}"))).respond_with(r);
    get("log-plain/", ResponseTemplate::new(200).set_body_json(json!({ "ok": 1 }))).mount(&server).await;
    get("log-q/", ResponseTemplate::new(200).set_body_json(json!({ "ok": 1 }))).mount(&server).await;
    get("log-html/", ResponseTemplate::new(200).set_body_string("<html>not json</html>")).mount(&server).await;
    get("log-err/", ResponseTemplate::new(404).set_body_json(json!({ "detail": "gone" }))).mount(&server).await;
    get("log-big/", ResponseTemplate::new(200).set_body_string("x".repeat(2 * 1024 * 1024))).mount(&server).await;
    Mock::given(method("POST"))
        .and(path(format!("{API}log-form/")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "pulp_href": "/x/" })))
        .mount(&server)
        .await;
    Mock::given(method("PUT")).and(path(format!("{API}log-chunk/"))).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/content/log-file"))
        .respond_with(ResponseTemplate::new(200).set_body_string("0123456789"))
        .mount(&server)
        .await;
    let c = client(&server, |_, _| {});

    // No query: no trailing '?'. With a query: it is part of the shown URL.
    c.get::<Value>(&c.api("log-plain/"), &[]).await.unwrap();
    assert!(logged("GET /pulp/api/v3/log-plain/ 200").contains("\"ok\": 1"));
    c.get::<Value>(&c.api("log-q/"), &[("limit", "5".into())]).await.unwrap();
    assert!(logged("GET /pulp/api/v3/log-q/?limit=5 200").contains("status: 200"));

    // A body that is not JSON is kept as text, even though the call itself fails to decode.
    assert!(matches!(c.get::<Value>(&c.api("log-html/"), &[]).await.unwrap_err(), PulpError::Decode(_)));
    assert!(logged("GET /pulp/api/v3/log-html/ 200").contains("<html>not json</html>"));

    // A non-success status keeps its body.
    c.get::<Value>(&c.api("log-err/"), &[]).await.unwrap_err();
    let text = logged("GET /pulp/api/v3/log-err/ 404");
    assert!(text.contains("status: 404") && text.contains("gone"), "{text}");

    // The body is rejected as too large: the failure is recorded.
    let small = client(&server, |_, s| s.max_response_mb = 1);
    assert!(matches!(small.get::<Value>(&small.api("log-big/"), &[]).await.unwrap_err(), PulpError::TooLarge(1)));
    assert!(logged("GET /pulp/api/v3/log-big/ response larger than 1 MiB rejected").contains("error: response larger"));

    // Uploads and served content are logged by size only.
    c.post_form(&c.api("log-form/"), &[("sha256", "abc".into())], Some(("a.rpm".into(), vec![7; 5]))).await.unwrap();
    let text = logged("POST /pulp/api/v3/log-form/ 201");
    assert!(text.contains("<upload, 5 bytes>") && !text.contains("sha256"), "{text}");
    c.put_chunk(&c.api("log-chunk/"), (0, 3, 4), vec![1; 4]).await.unwrap();
    assert!(logged("PUT /pulp/api/v3/log-chunk/ 200").contains("<upload, 4 bytes>"));
    c.fetch_content(&format!("{}/content/log-file", server.uri()), 1024).await.unwrap();
    let text = logged("GET /content/log-file 200");
    assert!(text.contains("<content, 10 bytes>") && !text.contains("0123456789"), "{text}");

    // A network error has no status.
    let down = client(&server, |p, _| p.url = "http://127.0.0.1:1".into());
    down.post(&down.api("log-down/"), &json!({})).await.unwrap_err();
    assert!(logged("POST /pulp/api/v3/log-down/ network error").contains("error: network error"));
}

fn dist(labels: Value) -> Value {
    json!({
        "pulp_href": format!("{API}distributions/rpm/rpm/1/"),
        "name": "d",
        "base_path": "bp",
        "publication": format!("{API}publications/rpm/rpm/1/"),
        "pulp_labels": labels,
    })
}

async fn serve_dist(server: &MockServer, d: Value) {
    let page = json!({ "count": 1, "next": null, "results": [d] });
    let list = Mock::given(method("GET")).and(path(format!("{API}distributions/rpm/rpm/")));
    list.respond_with(ResponseTemplate::new(200).set_body_json(page)).mount(server).await;
}

#[tokio::test]
async fn distribution_of_another_database_is_refused() {
    let server = MockServer::start().await;
    serve_dist(&server, dist(json!({ "pulp_tui_cv": "web", "pulp_tui_db": "other" }))).await;
    Mock::given(method("PATCH")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&server).await;
    let c = client(&server, |_, _| {});
    let labels = json!({ "pulp_tui_cv": "web" });
    let pubn = format!("{API}publications/rpm/rpm/1/");
    let db = Some(("pulp_tui_db", "me", &[][..]));
    let err = ops::ensure_distribution(&c, RepoType::Rpm, "d", "bp", Some(&pubn), &labels, db, &no_progress).await.unwrap_err();
    let want = "distribution d is owned by another pulp-tui database (other)";
    assert!(matches!(&err, PulpError::Api { status: 409, message } if message == want), "{err}");
    server.verify().await;
}

#[tokio::test]
async fn distribution_without_database_label_is_adopted_even_when_the_publication_is_right() {
    let server = MockServer::start().await;
    serve_dist(&server, dist(json!({ "pulp_tui_cv": "web", "keep": "x" }))).await;
    Mock::given(method("PATCH"))
        .and(body_partial_json(json!({ "pulp_labels": { "pulp_tui_cv": "web", "keep": "x", "pulp_tui_db": "me" } })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server, |_, _| {});
    let labels = json!({ "pulp_tui_cv": "web" });
    let pubn = format!("{API}publications/rpm/rpm/1/");
    let db = Some(("pulp_tui_db", "me", &[][..]));
    ops::ensure_distribution(&c, RepoType::Rpm, "d", "bp", Some(&pubn), &labels, db, &no_progress).await.unwrap();
    server.verify().await;
}

#[tokio::test]
async fn file_sync_maps_any_mirror_policy_to_mirror() {
    let server = MockServer::start().await;
    let task = json!({ "task": format!("{API}tasks/1/") });
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(202).set_body_json(task)).expect(4).mount(&server).await;
    let c = client(&server, |_, _| {});
    for (t, policy) in [
        (RepoType::File, "additive"),
        (RepoType::File, "mirror"),
        (RepoType::File, "mirror_content_only"),
        (RepoType::Rpm, "mirror_complete"),
    ] {
        let repo = format!("{API}repositories/{}/r/", if t == RepoType::File { "file/file" } else { "rpm/rpm" });
        ops::sync(&c, t, &repo, None, policy).await.unwrap();
    }
    let bodies: Vec<Value> = server.received_requests().await.unwrap().iter().map(|r| r.body_json().unwrap()).collect();
    assert_eq!(
        bodies,
        [json!({"mirror": false}), json!({"mirror": true}), json!({"mirror": true}), json!({"sync_policy": "mirror_complete"})]
    );
    server.verify().await;
}

#[tokio::test]
async fn deb_sync_maps_rpm_mirror_policies_to_mirror() {
    let server = MockServer::start().await;
    let repo = format!("{API}repositories/deb/apt/r/");
    Mock::given(method("POST"))
        .and(path(format!("{repo}sync/")))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({ "task": format!("{API}tasks/1/") })))
        .expect(3)
        .mount(&server)
        .await;
    let c = client(&server, |_, _| {});
    for policy in ["additive", "mirror", "mirror_complete"] {
        ops::sync(&c, RepoType::Deb, &repo, Some("/rm/"), policy).await.unwrap();
    }
    let bodies: Vec<Value> = server.received_requests().await.unwrap().iter().map(|r| r.body_json().unwrap()).collect();
    assert_eq!(
        bodies,
        [json!({"remote": "/rm/", "mirror": false}), json!({"remote": "/rm/", "mirror": true}), json!({"remote": "/rm/", "mirror": true})]
    );
    server.verify().await;
}

#[tokio::test]
async fn deb_publication_reuse_respects_signing_service() {
    let server = MockServer::start().await;
    let coll = format!("{API}publications/deb/apt/");
    let listed = json!({ "pulp_href": format!("{coll}1/"), "repository_version": "/v/", "signing_service": null });
    Mock::given(method("GET"))
        .and(path(&coll))
        .and(query_param("repository_version", "/v/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "count": 1, "next": null, "results": [listed] })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(&coll))
        .and(body_json(json!({ "repository_version": "/v/", "signing_service": "/ss/1/" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "pulp_href": format!("{coll}2/") })))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server, |_, _| {});
    let signed = ops::ensure_publication(&c, RepoType::Deb, "/v/", Some("/ss/1/"), &no_progress).await.unwrap();
    assert_eq!(signed, format!("{coll}2/"));
    let plain = ops::ensure_publication(&c, RepoType::Deb, "/v/", None, &no_progress).await.unwrap();
    assert_eq!(plain, format!("{coll}1/"));
    server.verify().await;
}

#[tokio::test]
async fn deb_packages_cannot_be_uploaded() {
    let server = MockServer::start().await;
    let c = client(&server, |_, _| {});
    let file = tempfile::NamedTempFile::new().unwrap();
    let e = ops::upload(&c, RepoType::Deb, file.path(), None, None, &|_, _| {}, &no_progress).await.unwrap_err();
    assert!(matches!(&e, PulpError::Unsupported(m) if m == "upload of Deb packages is not supported"), "{e}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn deb_copy_is_structured_without_dependency_solving() {
    let server = MockServer::start().await;
    let body = json!({
        "structured": true,
        "dependency_solving": false,
        "config": [{ "source_repo_version": "/sv/", "dest_repo": "/dr/", "dest_base_version": 0, "content": ["/p/1/"] }],
    });
    Mock::given(method("POST"))
        .and(path(format!("{API}deb/copy/")))
        .and(body_partial_json(body))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({ "task": "/t/1/" })))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server, |_, _| {});
    let m = deb::copy(&c, "/sv/", "/dr/", Some(0), Some(vec!["/p/1/".into()])).await.unwrap();
    assert!(matches!(m, Mutation::Task(t) if t == "/t/1/"));
    server.verify().await;

    // Without a content list the config has no `content` key (copy everything).
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(202).set_body_json(json!({ "task": "/t/2/" }))).mount(&server).await;
    let c = client(&server, |_, _| {});
    deb::copy(&c, "/sv/", "/dr/", None, None).await.unwrap();
    let sent: Value = server.received_requests().await.unwrap()[0].body_json().unwrap();
    assert_eq!(
        sent,
        json!({ "structured": true, "dependency_solving": false, "config": [{ "source_repo_version": "/sv/", "dest_repo": "/dr/" }] })
    );
}

#[tokio::test]
async fn deb_copy_names_the_body_limit_of_the_server() {
    // Pulp refuses a body above Django's DATA_UPLOAD_MAX_MEMORY_SIZE with an HTML 400 (no message).
    let server = MockServer::start().await;
    let page = "<!doctype html><title>Bad Request (400)</title>";
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(400).set_body_string(page)).mount(&server).await;
    let c = client(&server, |_, _| {});
    // 74 bytes per href of this length in compact JSON: 36,000 hrefs are above the limit.
    let hrefs: Vec<String> = (0..36_000).map(|i| format!("/pulp/api/v3/content/deb/packages/{i:036}/")).collect();
    let e = deb::copy(&c, "/sv/", "/dr/", Some(0), Some(hrefs)).await.unwrap_err().to_string();
    assert!(e.starts_with("rejected [400]: request body ") && e.contains(" bytes, 36000 hrefs; "), "{e}");
    assert!(e.ends_with("raise it in the Pulp settings, or narrow the selection with include rules"), "{e}");
    let e = deb::copy(&c, "/sv/", "/dr/", Some(0), Some(vec!["/p/1/".into()])).await.unwrap_err().to_string();
    assert_eq!(e, "rejected [400]", "a small body: the server's answer as it is");
}

/// A server that lists one Release, two components (one of another release) and `packages`, and
/// serves the Release file, a `Packages` index for `amd64` and the package bytes.
async fn deb_server(packages: Value) -> (MockServer, String) {
    let server = MockServer::start().await;
    let page = |results: Value| {
        ResponseTemplate::new(200).set_body_json(json!({ "count": results.as_array().unwrap().len(), "next": null, "results": results }))
    };
    let api = |p: &str| path(format!("{API}content/deb/{p}/"));
    // pulp_deb strips the slashes of the distribution and keeps the last part of the component in the path.
    Mock::given(method("GET")).and(api("releases")).respond_with(page(json!([{ "distribution": "/stable/" }]))).mount(&server).await;
    let components = json!([
        { "component": "contrib", "plain_component": "contrib", "distribution": "other" },
        { "component": "updates/main", "plain_component": "main", "distribution": "/stable/" },
    ]);
    Mock::given(method("GET"))
        .and(api("release_components"))
        .and(query_param("fields", "plain_component,distribution"))
        .respond_with(page(components))
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(api("packages")).respond_with(page(packages)).mount(&server).await;
    let served = |p: &str, body: &str| {
        Mock::given(method("GET"))
            .and(path(format!("/pulp/content/d/{p}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(body.to_string()))
    };
    served("dists/stable/Release", "Suite: stable\n").mount(&server).await;
    let index = "Package: fixture-beta\nVersion: 2.0-1\nArchitecture: amd64\nFilename: pool/main/f/fixture-beta_2.0-1_amd64.deb\n\n\
                 Package: fixture-beta\nVersion: 1.0-1\nArchitecture: amd64\nFilename: pool/main/f/fixture-beta_1.0-1_amd64.deb\n";
    served("dists/stable/main/binary-amd64/Packages", index).mount(&server).await;
    served("pool/main/f/fixture-beta_1.0-1_amd64.deb", "deb bytes").mount(&server).await;
    let base = format!("{}/pulp/content/d/", server.uri());
    (server, base)
}

#[tokio::test]
async fn deb_verify_checks_release_index_and_package_digest() {
    let pkg = |sha: &str| json!([{ "pulp_href": "/p/1/", "package": "fixture-beta", "version": "1.0-1", "architecture": "amd64", "relative_path": "pool/x.deb", "sha256": sha }]);
    let (server, base) = deb_server(pkg(&hex::encode(Sha256::digest(b"deb bytes")))).await;
    let c = client(&server, |_, _| {});
    let ok = deb::verify(&c, &base, "/v/").await.unwrap();
    assert_eq!(ok, "Release and pool/main/f/fixture-beta_1.0-1_amd64.deb served with matching digest (1 packages)");

    let (server, base) = deb_server(pkg("00")).await;
    let c = client(&server, |_, _| {});
    let err = deb::verify(&c, &base, "/v/").await.unwrap_err();
    assert!(matches!(&err, PulpError::Unsupported(m) if m.contains("not served with the expected digest")), "{err}");

    let (server, base) = deb_server(json!([])).await;
    let c = client(&server, |_, _| {});
    assert_eq!(deb::verify(&c, &base, "/v/").await.unwrap(), "Release served (no packages)");
}

#[tokio::test]
async fn deb_verify_finds_the_package_in_another_distribution() {
    // A remote of `stable stable-updates`: the smallest package is only in the second distribution.
    let gamma = json!([{ "pulp_href": "/p/3/", "package": "fixture-gamma", "version": "1.0-1", "architecture": "amd64",
        "relative_path": "pool/g.deb", "sha256": hex::encode(Sha256::digest(b"gamma bytes")) }]);
    let (server, base) = deb_server(gamma).await;
    let components = json!([
        { "component": "main", "plain_component": "main", "distribution": "/stable/" },
        { "component": "main", "plain_component": "main", "distribution": "stable-updates" },
    ]);
    Mock::given(method("GET"))
        .and(path(format!("{API}content/deb/release_components/")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "count": 2, "next": null, "results": components })))
        .with_priority(1)
        .mount(&server)
        .await;
    let file = "pool/main/f/fixture-gamma_1.0-1_amd64.deb";
    let index = format!("Package: fixture-gamma\nVersion: 1.0-1\nArchitecture: amd64\nFilename: {file}\n");
    for (p, body) in [("dists/stable-updates/main/binary-amd64/Packages", index.as_str()), (file, "gamma bytes")] {
        Mock::given(method("GET"))
            .and(path(format!("/pulp/content/d/{p}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
    }
    let c = client(&server, |_, _| {});
    let ok = deb::verify(&c, &base, "/v/").await.unwrap();
    assert_eq!(ok, format!("Release and {file} served with matching digest (1 packages)"));
}
