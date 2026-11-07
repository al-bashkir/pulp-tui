//! Transport behavior against a mock HTTP server: pagination, auth failures, malformed
//! responses, retry rules (reads only), timeouts, task polling, capability negotiation,
//! read-only mode and size limits.

use pulp_tui::config::{AuthMethod, Profile, Settings};
use pulp_tui::pulp::models::Task;
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{Capabilities, Feature, Mutation, PulpClient, PulpError, RepoType, deb, rpm};
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
async fn task_group_polling_survives_a_short_outage() {
    let server = MockServer::start().await;
    let href = format!("{API}task-groups/1/");
    // More 503s than the client retries for one GET (4 attempts).
    Mock::given(method("GET")).and(path(&href)).respond_with(ResponseTemplate::new(503)).up_to_n_times(5).mount(&server).await;
    let done = json!({ "pulp_href": href, "all_tasks_dispatched": true, "completed": 1, "tasks": [] });
    Mock::given(method("GET")).and(path(&href)).respond_with(ResponseTemplate::new(200).set_body_json(done)).mount(&server).await;
    let c = client(&server, |_, _| {});
    assert!(ops::wait_task_group(&c, &href, &|_| {}).await.is_ok());

    let denied = format!("{API}task-groups/2/");
    Mock::given(method("GET")).and(path(&denied)).respond_with(ResponseTemplate::new(403)).expect(1).mount(&server).await;
    let e = ops::wait_task_group(&c, &denied, &|_| {}).await.unwrap_err();
    assert_eq!(e.status(), Some(403), "{e}");
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
    // A skipped task did not run: no caller may take its (empty) result as done.
    let skipped = json!({ "pulp_href": format!("{API}tasks/5/"), "state": "skipped" });
    Mock::given(method("GET"))
        .and(path(format!("{API}tasks/5/")))
        .respond_with(ResponseTemplate::new(200).set_body_json(skipped))
        .mount(&server)
        .await;
    let e = ops::wait_task(&c, &format!("{API}tasks/5/"), &no_progress).await.unwrap_err();
    assert!(matches!(&e, PulpError::TaskFailed { state, .. } if state == "skipped"), "{e:?}");
}

#[tokio::test]
async fn settled_task_group_is_done_only_when_every_task_completed() {
    let server = MockServer::start().await;
    let task =
        |n: u32, state: &str| json!({ "pulp_href": format!("{API}tasks/{n}/"), "state": state, "name": "pulp_rpm.app.tasks.synchronize" });
    let groups = [
        (json!({ "completed": 2 }), [task(1, "completed"), task(2, "completed")]),
        (json!({ "completed": 1, "failed": 1 }), [task(3, "completed"), task(4, "failed")]),
        (json!({ "completed": 1, "canceled": 1 }), [task(5, "completed"), task(6, "canceled")]),
    ];
    for (n, (counts, tasks)) in groups.into_iter().enumerate() {
        let href = format!("{API}task-groups/{n}/");
        let mut g = json!({ "pulp_href": href, "description": "g", "all_tasks_dispatched": true, "tasks": tasks });
        g.as_object_mut().unwrap().extend(counts.as_object().unwrap().clone());
        Mock::given(method("GET")).and(path(&href)).respond_with(ResponseTemplate::new(200).set_body_json(g)).mount(&server).await;
    }
    let failed = json!({ "pulp_href": format!("{API}tasks/4/"), "state": "failed", "error": {"description": "404 upstream"} });
    Mock::given(method("GET"))
        .and(path(format!("{API}tasks/4/")))
        .respond_with(ResponseTemplate::new(200).set_body_json(failed))
        .mount(&server)
        .await;
    let c = client(&server, |_, _| {});
    let mut ends = vec![];
    for n in 0..3 {
        ends.push(match ops::settle(&c, Mutation::TaskGroup(format!("{API}task-groups/{n}/")), &no_progress).await {
            Ok(t) => (format!("ok {t:?}"), String::new()),
            Err(PulpError::TaskFailed { state, message, .. }) => (state, message),
            Err(e) => panic!("{e:?}"),
        });
    }
    assert_eq!(ends.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(), ["ok None", "failed", "canceled"]);
    // The error names the task that did not complete, with the error text `wait_task` gives.
    let named =
        |msg: &str, state: &str, n: u32| msg.lines().any(|l| l.trim_start().starts_with(state) && l.ends_with(&format!("{API}tasks/{n}/")));
    assert!(named(&ends[1].1, "failed", 4) && ends[1].1.contains("error: 404 upstream"), "{}", ends[1].1);
    assert!(named(&ends[2].1, "canceled", 6), "{}", ends[2].1);
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
    let signed = ops::ensure_publication(&c, RepoType::Deb, "/v/", &json!({ "signing_service": "/ss/1/" }), &no_progress).await.unwrap();
    assert_eq!(signed, format!("{coll}2/"));
    let plain = ops::ensure_publication(&c, RepoType::Deb, "/v/", &json!({ "signing_service": null }), &no_progress).await.unwrap();
    assert_eq!(plain, format!("{coll}1/"));
    server.verify().await;
}

/// The mode of a verbatim publication is `verbatim`: a publication in that mode is a verbatim one of the version,
/// reused or made with only the version (pulp_deb has no options for it).
#[tokio::test]
async fn deb_verbatim_mode_reuses_or_makes_a_verbatim_publication() {
    let server = MockServer::start().await;
    let coll = format!("{API}publications/deb/verbatim/");
    for (v, listed) in [("/v1/", json!([{ "pulp_href": format!("{coll}1/"), "repository_version": "/v1/" }])), ("/v2/", json!([]))] {
        let page = json!({ "count": listed.as_array().unwrap().len(), "next": null, "results": listed });
        Mock::given(method("GET"))
            .and(path(&coll))
            .and(query_param("repository_version", v))
            .respond_with(ResponseTemplate::new(200).set_body_json(page))
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path(&coll))
        .and(body_json(json!({ "repository_version": "/v2/" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "pulp_href": format!("{coll}2/") })))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server, |_, _| {});
    let verbatim = json!({ "verbatim": true });
    assert_eq!(ops::publication_mode(RepoType::Deb, &json!({ "pulp_href": format!("{coll}1/"), "repository_version": "/v1/" })), verbatim);
    assert_eq!(ops::ensure_publication(&c, RepoType::Deb, "/v1/", &verbatim, &no_progress).await.unwrap(), format!("{coll}1/"));
    assert_eq!(ops::ensure_publication(&c, RepoType::Deb, "/v2/", &verbatim, &no_progress).await.unwrap(), format!("{coll}2/"));
    server.verify().await;
}

/// A verbatim publication serves the synced files at their upstream paths: verify reads the Release file where the
/// version's ReleaseFile has it, `dists/<release>/` or, for a flat repository, the root with one `Packages` index.
#[tokio::test]
async fn deb_verify_of_a_verbatim_publication_reads_the_synced_paths() {
    let deb = b"deb bytes";
    let index = "Package: hello\nVersion: 1.0\nArchitecture: amd64\nFilename: pool/hello.deb\n";
    let flat =
        (json!({ "distribution": "flat-repo", "relative_path": "Release" }), "flat-repo", "flat-repo-component", ["Release", "Packages"]);
    let dists = (
        json!({ "distribution": "fixture", "relative_path": "dists/fixture/Release" }),
        "fixture",
        "main",
        ["dists/fixture/Release", "dists/fixture/main/binary-amd64/Packages"],
    );
    for (release_file, d, comp, [release, packages]) in [flat, dists] {
        let server = MockServer::start().await;
        let ok = |b: Value| ResponseTemplate::new(200).set_body_json(json!({ "count": 1, "next": null, "results": [b] }));
        Mock::given(method("GET"))
            .and(path(format!("{API}content/deb/release_files/")))
            .respond_with(ok(release_file))
            .mount(&server)
            .await;
        let c = json!({ "distribution": d, "plain_component": comp });
        Mock::given(method("GET")).and(path(format!("{API}content/deb/release_components/"))).respond_with(ok(c)).mount(&server).await;
        let p = json!({ "pulp_href": "/p/1/", "package": "hello", "version": "1.0", "architecture": "amd64",
            "relative_path": "pool/hello.deb", "sha256": hex::encode(Sha256::digest(deb)) });
        Mock::given(method("GET")).and(path(format!("{API}content/deb/packages/"))).respond_with(ok(p)).mount(&server).await;
        for (f, body) in [(release, &b"Suite: x\n"[..]), (packages, index.as_bytes()), ("pool/hello.deb", deb)] {
            let served = ResponseTemplate::new(200).set_body_bytes(body.to_vec());
            Mock::given(method("GET")).and(path(format!("/pulp/content/r/{f}"))).respond_with(served).mount(&server).await;
        }
        let base = format!("{}/pulp/content/r/", server.uri());
        let verbatim = json!({ "pulp_href": format!("{API}publications/deb/verbatim/1/") });
        let msg = deb::verify(&client(&server, |_, _| {}), &base, "/v1/", &verbatim).await.unwrap();
        assert_eq!(msg, "Release and pool/hello.deb served with matching digest (1 packages)", "{d}");
    }
}

/// A `.deb` goes to `content/deb/packages/` with the release that lists it, in one request or (above one chunk)
/// through `uploads/`. The task names the new release records before the package: the digest is checked on the package.
#[tokio::test]
async fn deb_package_upload_sends_the_release_and_checks_the_package() {
    let repo = format!("{API}repositories/deb/apt/r/");
    let pkg = format!("{API}content/deb/packages/1/");
    let dir = tempfile::tempdir().unwrap();
    for (size, release) in [(10, Some(("fixture", "main"))), (1024 * 1024 + 5, None)] {
        let file = dir.path().join(format!("p{size}.deb"));
        std::fs::write(&file, vec![7u8; size]).unwrap();
        let sha = hex::encode(Sha256::digest(vec![7u8; size]));
        let server = MockServer::start().await;
        let answer = |verb: &str, p: &str, status: u16, body: Value| {
            Mock::given(method(verb)).and(path(p.to_string())).respond_with(ResponseTemplate::new(status).set_body_json(body))
        };
        answer("POST", &format!("{API}uploads/"), 201, json!({ "pulp_href": format!("{API}uploads/1/") })).mount(&server).await;
        answer("PUT", &format!("{API}uploads/1/"), 200, json!({})).mount(&server).await;
        answer("POST", &format!("{API}content/deb/packages/"), 202, json!({ "task": format!("{API}tasks/1/") })).mount(&server).await;
        let created = [format!("{API}content/deb/release_components/5/"), format!("{repo}versions/2/"), pkg.clone()];
        let task = json!({ "pulp_href": format!("{API}tasks/1/"), "state": "completed", "created_resources": created });
        answer("GET", &format!("{API}tasks/1/"), 200, task).mount(&server).await;
        answer("GET", &pkg, 200, json!({ "sha256": sha })).mount(&server).await;
        let c = client(&server, |_, s| s.upload_chunk_mb = 1);

        let up = deb::Upload::Package { distribution: release.map(|r| r.0.into()), component: release.map(|r| r.1.into()) };
        // Overwrite on is the server default: not sent.
        let r = deb::upload(&c, &up, &file, Some(&repo), true, &|_, _| {}, &no_progress).await.unwrap();
        assert_eq!((r.content_href, r.sha256), (pkg.clone(), sha));
        let sent = server.received_requests().await.unwrap();
        let post = sent.iter().find(|r| r.url.path() == format!("{API}content/deb/packages/")).unwrap();
        let body = String::from_utf8_lossy(&post.body).replace("\r\n", "\n");
        let field = |k: &str, v: &str| format!("name=\"{k}\"\n\n{v}\n");
        assert!(body.contains(&field("repository", &repo)) && !body.contains("relative_path") && !body.contains("overwrite"), "{body}");
        match release {
            Some((d, cp)) => assert!(body.contains(&field("distribution", d)) && body.contains(&field("component", cp)), "{body}"),
            None => assert!(!body.contains("distribution") && !body.contains("component"), "{body}"),
        }
        let ranges: Vec<&str> =
            sent.iter().filter(|r| r.method.as_str() == "PUT").map(|r| r.headers["content-range"].to_str().unwrap()).collect();
        if size > 1024 * 1024 {
            assert!(body.contains(&field("upload", &format!("{API}uploads/1/"))) && !body.contains("filename="), "{body}");
            assert_eq!(ranges, ["bytes 0-1048575/1048581", "bytes 1048576-1048580/1048581"], "two chunks");
        } else {
            assert!(body.contains("filename=\"p10.deb\"") && !body.contains("name=\"upload\""), "{body}");
            assert!(ranges.is_empty());
        }
    }
}

/// A chunked upload that the content POST refuses, or whose task fails, is deleted; a unit whose server digest is not
/// the local one is an error.
#[tokio::test]
async fn a_failed_chunked_upload_is_deleted_and_a_digest_mismatch_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("big.txt");
    std::fs::write(&file, vec![1u8; 1024 * 1024 + 1]).unwrap();
    let (files, upload, unit) = (format!("{API}content/file/files/"), format!("{API}uploads/1/"), format!("{API}content/file/files/1/"));
    let answer = |verb: &str, p: &str, status: u16, body: Value| {
        Mock::given(method(verb)).and(path(p.to_string())).respond_with(ResponseTemplate::new(status).set_body_json(body))
    };
    for (status, task_state) in [(400, ""), (202, "failed")] {
        let server = MockServer::start().await;
        answer("POST", &format!("{API}uploads/"), 201, json!({ "pulp_href": upload })).mount(&server).await;
        answer("PUT", &upload, 200, json!({})).mount(&server).await;
        answer("POST", &files, status, json!({ "task": format!("{API}tasks/1/") })).mount(&server).await;
        let task = json!({ "pulp_href": format!("{API}tasks/1/"), "state": task_state, "error": { "description": "boom" } });
        answer("GET", &format!("{API}tasks/1/"), 200, task).mount(&server).await;
        answer("DELETE", &upload, 204, json!({})).expect(1).mount(&server).await;
        let c = client(&server, |_, s| s.upload_chunk_mb = 1);
        let e = ops::upload(&c, RepoType::File, &file, None, None, true, &|_, _| {}, &no_progress).await.unwrap_err();
        assert!(matches!(e, PulpError::Api { status: 400, .. } | PulpError::TaskFailed { .. }), "{e}");
        server.verify().await;
    }

    let server = MockServer::start().await;
    answer("POST", &files, 202, json!({ "task": format!("{API}tasks/1/") })).mount(&server).await;
    let task = json!({ "pulp_href": format!("{API}tasks/1/"), "state": "completed", "created_resources": [unit] });
    answer("GET", &format!("{API}tasks/1/"), 200, task).mount(&server).await;
    answer("GET", &unit, 200, json!({ "sha256": "other" })).mount(&server).await;
    let e = ops::upload(&client(&server, |_, _| {}), RepoType::File, &file, None, None, true, &|_, _| {}, &no_progress).await.unwrap_err();
    assert!(e.to_string().contains(&format!("digest mismatch for {unit}: local ")), "{e}");
}

/// A chunked upload sends each byte once, in chunks of the chunk size: an exact multiple of it has no empty last
/// chunk, and a short last chunk ends at the file size. An empty file goes in one request, with no chunk.
#[tokio::test]
async fn chunk_ranges_cover_the_file_once() {
    let (dir, mib) = (tempfile::tempdir().unwrap(), 1024 * 1024);
    let (files, upload, unit) = (format!("{API}content/file/files/"), format!("{API}uploads/1/"), format!("{API}content/file/files/1/"));
    for (size, want) in [
        (2 * mib, vec!["bytes 0-1048575/2097152", "bytes 1048576-2097151/2097152"]),
        (mib + 5, vec!["bytes 0-1048575/1048581", "bytes 1048576-1048580/1048581"]),
        (0, vec![]),
    ] {
        let bytes: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let file = dir.path().join(format!("{size}.bin"));
        std::fs::write(&file, &bytes).unwrap();
        let server = MockServer::start().await;
        let answer = |verb: &str, p: &str, status: u16, body: Value| {
            Mock::given(method(verb)).and(path(p.to_string())).respond_with(ResponseTemplate::new(status).set_body_json(body))
        };
        answer("POST", &format!("{API}uploads/"), 201, json!({ "pulp_href": upload })).mount(&server).await;
        answer("PUT", &upload, 200, json!({})).mount(&server).await;
        answer("POST", &files, 202, json!({ "task": format!("{API}tasks/1/") })).mount(&server).await;
        let task = json!({ "pulp_href": format!("{API}tasks/1/"), "state": "completed", "created_resources": [unit] });
        answer("GET", &format!("{API}tasks/1/"), 200, task).mount(&server).await;
        answer("GET", &unit, 200, json!({ "sha256": hex::encode(Sha256::digest(&bytes)) })).mount(&server).await;
        let c = client(&server, |_, s| s.upload_chunk_mb = 1);

        let r = ops::upload(&c, RepoType::File, &file, None, None, true, &|_, _| {}, &no_progress).await.unwrap();
        assert_eq!(r.sha256, hex::encode(Sha256::digest(&bytes)), "{size}");
        let sent = server.received_requests().await.unwrap();
        let ranges: Vec<&str> =
            sent.iter().filter(|r| r.method.as_str() == "PUT").map(|r| r.headers["content-range"].to_str().unwrap()).collect();
        assert_eq!(ranges, want, "{size}");
    }
}

/// A `.udeb` goes to `content/deb/installer_packages/`, any other file to `content/deb/generic_contents/` with its
/// relative path (required there). Neither takes a release; the digest is checked on the unit that the task names.
#[tokio::test]
async fn deb_installer_and_generic_uploads_go_to_their_endpoints() {
    let repo = format!("{API}repositories/deb/apt/r/");
    let dir = tempfile::tempdir().unwrap();
    let generic = deb::Upload::Generic { relative_path: "docs/x.txt".into() };
    for (up, endpoint) in [(deb::Upload::Installer, "installer_packages"), (generic, "generic_contents")] {
        let file = dir.path().join(endpoint);
        std::fs::write(&file, endpoint).unwrap();
        let unit = format!("{API}content/deb/{endpoint}/1/");
        let server = MockServer::start().await;
        let answer = |verb: &str, p: &str, status: u16, body: Value| {
            Mock::given(method(verb)).and(path(p.to_string())).respond_with(ResponseTemplate::new(status).set_body_json(body))
        };
        answer("POST", &format!("{API}content/deb/{endpoint}/"), 202, json!({ "task": format!("{API}tasks/1/") })).mount(&server).await;
        let task = json!({ "pulp_href": format!("{API}tasks/1/"), "state": "completed", "created_resources": [format!("{repo}versions/2/"), unit] });
        answer("GET", &format!("{API}tasks/1/"), 200, task).mount(&server).await;
        answer("GET", &unit, 200, json!({ "sha256": hex::encode(Sha256::digest(endpoint)) })).mount(&server).await;
        let c = client(&server, |_, _| {});

        let r = deb::upload(&c, &up, &file, Some(&repo), false, &|_, _| {}, &no_progress).await.unwrap();
        assert_eq!(r.content_href, unit);
        let sent = server.received_requests().await.unwrap();
        let body = String::from_utf8_lossy(&sent.iter().find(|r| r.method.as_str() == "POST").unwrap().body).replace("\r\n", "\n");
        let field = |k: &str, v: &str| format!("name=\"{k}\"\n\n{v}\n");
        assert!(body.contains(&field("repository", &repo)) && body.contains(&field("overwrite", "false")), "{body}");
        assert!(!body.contains("distribution") && !body.contains("component"), "{body}");
        assert_eq!(body.contains(&field("relative_path", "docs/x.txt")), endpoint == "generic_contents", "{body}");
    }
}

/// A source package: pulp_deb reads the `.dsc` from an artifact and needs an artifact of each file that it lists. An
/// artifact with the listed sha256 is reused, the other files and the `.dsc` are uploaded first (a file above one
/// chunk through `uploads/` and its `commit/`); then the source package is made from the `.dsc` artifact. A listed
/// file that is not next to the `.dsc` stops it before that.
#[tokio::test]
async fn deb_source_upload_sends_the_listed_files_and_the_dsc_as_artifacts() {
    let (repo, src, arts) =
        (format!("{API}repositories/deb/apt/r/"), format!("{API}content/deb/source_packages/1/"), format!("{API}artifacts/"));
    let sha = |b: &[u8]| hex::encode(Sha256::digest(b));
    let dir = tempfile::tempdir().unwrap();
    let (orig, debian) = (dir.path().join("h_1.0.orig.tar.gz"), dir.path().join("h_1.0-1.debian.tar.xz"));
    let big = vec![7u8; 1024 * 1024 + 6];
    std::fs::write(&orig, "orig").unwrap();
    std::fs::write(&debian, &big).unwrap();
    let text = format!("Source: h\nChecksums-Sha256:\n {} 4 h_1.0.orig.tar.gz\n {} 6 h_1.0-1.debian.tar.xz\n", sha(b"orig"), sha(&big));
    let dsc = dir.path().join("h_1.0-1.dsc");
    std::fs::write(&dsc, &text).unwrap();

    let server = MockServer::start().await;
    let ok = |status: u16, body: Value| ResponseTemplate::new(status).set_body_json(body);
    let answer =
        |verb: &str, p: &str, status: u16, body: Value| Mock::given(method(verb)).and(path(p.to_string())).respond_with(ok(status, body));
    let page = |r: Value| json!({ "count": r.as_array().unwrap().len(), "next": null, "results": r });
    let found = page(json!([{ "pulp_href": format!("{arts}1/") }]));
    let lookup = Mock::given(method("GET")).and(path(&arts)).and(query_param("sha256", sha(b"orig")));
    lookup.respond_with(ok(200, found)).with_priority(1).mount(&server).await;
    answer("GET", &arts, 200, page(json!([]))).mount(&server).await;
    // The large file: an upload and its chunks, then commit/ makes the artifact in a task.
    let (upload, commit) = (format!("{API}uploads/1/"), format!("{API}uploads/1/commit/"));
    answer("POST", &format!("{API}uploads/"), 201, json!({ "pulp_href": upload })).mount(&server).await;
    answer("PUT", &upload, 200, json!({})).mount(&server).await;
    answer("POST", &commit, 202, json!({ "task": format!("{API}tasks/2/") })).mount(&server).await;
    let made = json!({ "pulp_href": format!("{API}tasks/2/"), "state": "completed", "created_resources": [format!("{arts}2/")] });
    answer("GET", &format!("{API}tasks/2/"), 200, made).mount(&server).await;
    answer("GET", &format!("{arts}2/"), 200, json!({ "sha256": sha(&big) })).mount(&server).await;
    // The .dsc: one request.
    answer("POST", &arts, 201, json!({ "pulp_href": format!("{arts}3/") })).mount(&server).await;
    answer("GET", &format!("{arts}3/"), 200, json!({ "sha256": sha(text.as_bytes()) })).mount(&server).await;
    let source_packages = format!("{API}content/deb/source_packages/");
    answer("POST", &source_packages, 202, json!({ "task": format!("{API}tasks/1/") })).mount(&server).await;
    let task =
        json!({ "pulp_href": format!("{API}tasks/1/"), "state": "completed", "created_resources": [format!("{repo}versions/2/"), src] });
    answer("GET", &format!("{API}tasks/1/"), 200, task).mount(&server).await;
    let c = client(&server, |_, s| s.upload_chunk_mb = 1);

    let r = deb::upload(&c, &deb::Upload::Source, &dsc, Some(&repo), false, &|_, _| {}, &no_progress).await.unwrap();
    assert_eq!((r.content_href, r.sha256), (src, sha(text.as_bytes())));
    let sent = server.received_requests().await.unwrap();
    let posts: Vec<_> = sent.iter().filter(|r| r.method.as_str() == "POST").collect();
    let names: Vec<String> = posts.iter().map(|p| p.url.path().to_string()).collect();
    assert_eq!(names, [format!("{API}uploads/"), commit, arts.clone(), source_packages.clone()], "orig is there already: not sent");
    assert_eq!(posts[1].body_json::<Value>().unwrap(), json!({ "sha256": sha(&big) }));
    assert!(String::from_utf8_lossy(&posts[2].body).contains("filename=\"h_1.0-1.dsc\""));
    let body: Value = posts[3].body_json().unwrap();
    assert_eq!(body, json!({ "artifact": format!("{arts}3/"), "repository": repo, "overwrite": false }));

    std::fs::remove_file(&debian).unwrap();
    let e = deb::upload(&c, &deb::Upload::Source, &dsc, Some(&repo), true, &|_, _| {}, &no_progress).await.unwrap_err();
    assert!(e.to_string().contains("h_1.0-1.debian.tar.xz"), "{e}");
    let sent = server.received_requests().await.unwrap();
    assert_eq!(sent.iter().filter(|r| r.url.path() == source_packages).count(), 1, "no second source package");
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

/// A server that lists two components (of two distributions) and `packages`, and serves the Release
/// files, a `Packages` index for `amd64` and the package bytes.
async fn deb_server(packages: Value) -> (MockServer, String) {
    let server = MockServer::start().await;
    let page = |results: Value| {
        ResponseTemplate::new(200).set_body_json(json!({ "count": results.as_array().unwrap().len(), "next": null, "results": results }))
    };
    let api = |p: &str| path(format!("{API}content/deb/{p}/"));
    // pulp_deb strips the slashes of the distribution and keeps the last part of the component in the path.
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
    served("dists/other/Release", "Suite: other\n").mount(&server).await;
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
    let ok = deb::verify(&c, &base, "/v/", &json!({})).await.unwrap();
    assert_eq!(ok, "Release and pool/main/f/fixture-beta_1.0-1_amd64.deb served with matching digest (1 packages)");

    let (server, base) = deb_server(pkg("00")).await;
    let c = client(&server, |_, _| {});
    let err = deb::verify(&c, &base, "/v/", &json!({})).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "served content differs: package pool/main/f/fixture-beta_1.0-1_amd64.deb not served with the expected digest"
    );

    let (server, base) = deb_server(json!([])).await;
    let c = client(&server, |_, _| {});
    assert_eq!(deb::verify(&c, &base, "/v/", &json!({})).await.unwrap(), "Release served (no packages)");
}

#[tokio::test]
async fn deb_verify_checks_the_digest_of_the_index_for_packages_not_downloaded_yet() {
    // An on_demand package has no sha256 in the API until its file is downloaded; the index has the one apt checks.
    let pkg = |sha: Value| json!([{ "pulp_href": "/p/1/", "package": "fixture-beta", "version": "1.0-1", "architecture": "amd64", "relative_path": "pool/x.deb", "sha256": sha }]);
    let good = hex::encode(Sha256::digest(b"deb bytes"));
    let index = |sha: &str| {
        format!(
            "Package: fixture-beta\nVersion: 1.0-1\nArchitecture: amd64\nFilename: pool/main/f/fixture-beta_1.0-1_amd64.deb\nSHA256: {sha}\n"
        )
    };
    for (api, listed, ok) in [(Value::Null, good.as_str(), true), (Value::Null, "00", false), (json!("00"), good.as_str(), false)] {
        let (server, base) = deb_server(pkg(api.clone())).await;
        Mock::given(method("GET"))
            .and(path("/pulp/content/d/dists/stable/main/binary-amd64/Packages"))
            .respond_with(ResponseTemplate::new(200).set_body_string(index(listed)))
            .with_priority(1)
            .mount(&server)
            .await;
        let c = client(&server, |_, _| {});
        let res = deb::verify(&c, &base, "/v/", &json!({})).await;
        assert_eq!(res.is_ok(), ok, "api {api}, index {listed}: {res:?}");
    }
}

#[tokio::test]
async fn deb_verify_probes_the_paths_of_the_publication_mode() {
    let sha = hex::encode(Sha256::digest(b"deb bytes"));
    let pkg = json!([{ "pulp_href": "/p/1/", "package": "fixture-beta", "version": "1.0-1", "architecture": "amd64", "relative_path": "pool/x.deb", "sha256": sha }]);
    let simple = json!({ "simple": true, "structured": false });
    // The server serves only the structured paths: a simple publication is not served.
    let (server, base) = deb_server(pkg.clone()).await;
    let c = client(&server, |_, _| {});
    let err = deb::verify(&c, &base, "/v/", &simple).await.unwrap_err();
    assert_eq!(err.to_string(), "served content differs: dists/default/Release returned HTTP 404");
    // A simple publication: default/all.
    let (server, base) = deb_server(pkg).await;
    let index = "Package: fixture-beta\nVersion: 1.0-1\nArchitecture: amd64\nFilename: pool/all/f/fixture-beta_1.0-1_amd64.deb\n";
    for (p, body) in [
        ("dists/default/Release", "Suite: default\n"),
        ("dists/default/all/binary-amd64/Packages", index),
        ("pool/all/f/fixture-beta_1.0-1_amd64.deb", "deb bytes"),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/pulp/content/d/{p}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
    }
    let c = client(&server, |_, _| {});
    let ok = deb::verify(&c, &base, "/v/", &simple).await.unwrap();
    assert_eq!(ok, "Release and pool/all/f/fixture-beta_1.0-1_amd64.deb served with matching digest (1 packages)");
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
    for (p, body) in [
        ("dists/stable-updates/Release", "Suite: stable-updates\n"),
        ("dists/stable-updates/main/binary-amd64/Packages", index.as_str()),
        (file, "gamma bytes"),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/pulp/content/d/{p}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
    }
    let c = client(&server, |_, _| {});
    let ok = deb::verify(&c, &base, "/v/", &json!({})).await.unwrap();
    assert_eq!(ok, format!("Release and {file} served with matching digest (1 packages)"));
}

/// fixture-beta 1.0-1 (amd64), which `deb_layout_server` serves.
fn beta() -> Value {
    json!([{ "pulp_href": "/p/1/", "package": "fixture-beta", "version": "1.0-1", "architecture": "amd64",
        "relative_path": "pool/x.deb", "sha256": hex::encode(Sha256::digest(b"deb bytes")) }])
}

const BETA_OK: &str = "Release and pool/main/f/fixture-beta_1.0-1_amd64.deb served with matching digest (1 packages)";

/// A server with these release components and `packages` in the version, but no Release record. It
/// serves the file of fixture-beta, and the `served` paths: a `Packages` index that lists fixture-beta,
/// or else a Release file. Any other path is HTTP 404.
async fn deb_layout_server(components: Value, packages: Value, served: &[&str]) -> (MockServer, String) {
    let server = MockServer::start().await;
    let page = |results: Value| {
        ResponseTemplate::new(200).set_body_json(json!({ "count": results.as_array().unwrap().len(), "next": null, "results": results }))
    };
    let api = |p: &str| path(format!("{API}content/deb/{p}/"));
    Mock::given(method("GET")).and(api("releases")).respond_with(page(json!([]))).mount(&server).await;
    Mock::given(method("GET")).and(api("release_components")).respond_with(page(components)).mount(&server).await;
    Mock::given(method("GET")).and(api("packages")).respond_with(page(packages)).mount(&server).await;
    let file = "pool/main/f/fixture-beta_1.0-1_amd64.deb";
    let index = format!("Package: fixture-beta\nVersion: 1.0-1\nArchitecture: amd64\nFilename: {file}\n");
    for p in served.iter().copied().chain([file]) {
        let body = if p == file {
            "deb bytes"
        } else if p.ends_with("/Packages") {
            index.as_str()
        } else {
            "Suite: x\n"
        };
        Mock::given(method("GET"))
            .and(path(format!("/pulp/content/d/{p}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
    }
    let base = format!("{}/pulp/content/d/", server.uri());
    (server, base)
}

/// How often `server` was asked for the served path `p`.
async fn hits(server: &MockServer, p: &str) -> usize {
    let want = format!("/pulp/content/d/{p}");
    server.received_requests().await.unwrap().iter().filter(|r| r.url.path() == want).count()
}

fn component(distribution: &str, plain: &str) -> Value {
    json!({ "component": plain, "plain_component": plain, "distribution": distribution })
}

/// pulp_deb publishes `dists/<distribution>/` for each distribution of the release components of the
/// version; without a Release record (uploads, removed releases) it makes the Release file itself.
#[tokio::test]
async fn deb_verify_probes_the_release_of_every_distribution_of_the_release_components() {
    let one = json!([component("stable", "main")]);
    let (server, base) = deb_layout_server(one, beta(), &["dists/stable/Release", "dists/stable/main/binary-amd64/Packages"]).await;
    let c = client(&server, |_, _| {});
    assert_eq!(deb::verify(&c, &base, "/v/", &json!({})).await.unwrap(), BETA_OK);

    // Two distributions (one with two components): each Release file is probed once.
    let two = json!([component("stable", "main"), component("stable", "contrib"), component("/stable-updates/", "main")]);
    let all = ["dists/stable/Release", "dists/stable-updates/Release", "dists/stable/main/binary-amd64/Packages"];
    let (server, base) = deb_layout_server(two.clone(), beta(), &all).await;
    let c = client(&server, |_, _| {});
    assert_eq!(deb::verify(&c, &base, "/v/", &json!({})).await.unwrap(), BETA_OK);
    assert_eq!((hits(&server, all[0]).await, hits(&server, all[1]).await), (1, 1));
    for missing in &all[..2] {
        let served: Vec<&str> = all.into_iter().filter(|p| p != missing).collect();
        let (server, base) = deb_layout_server(two.clone(), beta(), &served).await;
        let c = client(&server, |_, _| {});
        let err = deb::verify(&c, &base, "/v/", &json!({})).await.unwrap_err();
        assert_eq!(err.to_string(), format!("served content differs: {missing} returned HTTP 404"));
    }

    // pulp_deb strips the outer slashes; a flat repository ("/", pulp_deb 3.5.4) is `flat-repo`.
    let odd = json!([component("/", "flat-repo-component"), component("bookworm/updates/", "main")]);
    let (server, base) = deb_layout_server(odd, json!([]), &["dists/flat-repo/Release", "dists/bookworm/updates/Release"]).await;
    let c = client(&server, |_, _| {});
    assert_eq!(deb::verify(&c, &base, "/v/", &json!({})).await.unwrap(), "Release served (no packages)");
}

/// A version without release components: pulp_deb 3.11.1 publishes an empty `dists/default/`
/// (component `empty`), and none of its packages is in a structured index.
#[tokio::test]
async fn deb_verify_of_a_version_without_release_components_probes_the_empty_default() {
    let (server, base) = deb_layout_server(json!([]), json!([]), &["dists/default/Release"]).await;
    let c = client(&server, |_, _| {});
    assert_eq!(deb::verify(&c, &base, "/v/", &json!({})).await.unwrap(), "Release served (no packages)");
    let (server, base) = deb_layout_server(json!([]), beta(), &["dists/default/Release"]).await;
    let c = client(&server, |_, _| {});
    let err = deb::verify(&c, &base, "/v/", &json!({})).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "served content differs: package fixture-beta_1.0-1_amd64 not listed in any Packages index \
         (tried dists/default/empty/binary-amd64/Packages)"
    );
}

/// A publication that is also simple: pulp_deb drops a structured `default` distribution (simple serves
/// `dists/default/`) and makes no empty structured one.
#[tokio::test]
async fn deb_verify_of_a_simple_and_structured_publication_drops_the_structured_default() {
    let both = json!({ "simple": true, "structured": true });
    let served = ["dists/stable/Release", "dists/default/Release", "dists/default/all/binary-amd64/Packages"];
    for comps in [json!([component("default", "main"), component("stable", "main")]), json!([])] {
        let (server, base) = deb_layout_server(comps.clone(), beta(), &served).await;
        let c = client(&server, |_, _| {});
        assert_eq!(deb::verify(&c, &base, "/v/", &both).await.unwrap(), BETA_OK, "{comps}");
        assert_eq!(hits(&server, "dists/default/Release").await, 1, "{comps}");
        for structured in ["main", "empty"] {
            assert_eq!(hits(&server, &format!("dists/default/{structured}/binary-amd64/Packages")).await, 0, "{comps}");
        }
    }
}

/// An RPM repository version; its repository is `{API}repositories/rpm/rpm/r/`.
const RPM_V: &str = "/pulp/api/v3/repositories/rpm/rpm/r/versions/1/";

/// The repository of `RPM_V`, without a metadata signing service.
async fn unsigned_repo(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path(format!("{API}repositories/rpm/rpm/r/")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "metadata_signing_service": null })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn rpm_verify_needs_the_signature_when_the_repository_signs_its_metadata() {
    let good = hex::encode(Sha256::digest(b"rpm bytes"));
    let pkgs = json!([{ "location_href": "a.rpm", "sha256": good }]);
    let mut asc = vec![];
    for signed in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("{API}content/rpm/packages/")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "count": 1, "next": null, "results": pkgs })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("{API}repositories/rpm/rpm/r/")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "metadata_signing_service": "/ss/1/" })))
            .mount(&server)
            .await;
        let mut files = vec![("repodata/repomd.xml", "<repomd/>"), ("Packages/a/a.rpm", "rpm bytes")];
        if signed {
            files.push(("repodata/repomd.xml.asc", "-----BEGIN PGP SIGNATURE-----"));
        }
        for (p, body) in files {
            Mock::given(method("GET"))
                .and(path(format!("/pulp/content/d/{p}")))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
        }
        let c = client(&server, |_, _| {});
        asc.push(pulp_tui::pulp::rpm::verify(&c, &format!("{}/pulp/content/d/", server.uri()), RPM_V, "").await.map_err(|e| e.to_string()));
    }
    // A publication made before the repository got its signing service has no signature.
    let unsigned = "served content differs: repodata/repomd.xml.asc returned HTTP 404 (the repository signs its metadata)";
    assert_eq!(asc[0], Err(unsigned.into()));
    assert_eq!(asc[1], Ok("repomd.xml, repomd.xml.asc and Packages/a/a.rpm served with matching digest (1 packages)".into()));
}

/// pulp_rpm stores no signing service in a publication, so one of a repository that signs its metadata may be
/// unsigned (made before the service was set): it is never reused. Unset or absent (an older server): reused.
#[tokio::test]
async fn rpm_publication_is_new_when_the_repository_signs_its_metadata() {
    let coll = format!("{API}publications/rpm/rpm/");
    let mode = json!({ "checksum_type": "sha256" });
    for (repo, want) in
        [(json!({ "metadata_signing_service": null }), "1/"), (json!({}), "1/"), (json!({ "metadata_signing_service": "/ss/1/" }), "2/")]
    {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("{API}repositories/rpm/rpm/r/")))
            .respond_with(ResponseTemplate::new(200).set_body_json(&repo))
            .mount(&server)
            .await;
        let listed = json!({ "pulp_href": format!("{coll}1/"), "repository_version": RPM_V, "checksum_type": "sha256" });
        Mock::given(method("GET"))
            .and(path(&coll))
            .and(query_param("repository_version", RPM_V))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "count": 1, "next": null, "results": [listed] })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(&coll))
            .and(body_json(json!({ "repository_version": RPM_V, "checksum_type": "sha256" })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "pulp_href": format!("{coll}2/") })))
            .expect(u64::from(want == "2/"))
            .mount(&server)
            .await;
        let c = client(&server, |_, _| {});
        assert_eq!(
            ops::ensure_publication(&c, RepoType::Rpm, RPM_V, &mode, &no_progress).await.unwrap(),
            format!("{coll}{want}"),
            "{repo}"
        );
        server.verify().await;
    }
}

#[tokio::test]
async fn rpm_and_file_verify_units_not_downloaded_yet() {
    // An on_demand unit has no sha256 in the API until its file is downloaded.
    let good = hex::encode(Sha256::digest(b"rpm bytes"));
    let page = |results: Value| {
        ResponseTemplate::new(200).set_body_json(json!({ "count": results.as_array().unwrap().len(), "next": null, "results": results }))
    };
    let served = |p: &str, body: &str| {
        Mock::given(method("GET"))
            .and(path(format!("/pulp/content/d/{p}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(body.to_string()))
    };
    let pkg = |sha: &Value, pkg_id: &str, kind: &str| json!([{ "location_href": "Packages/f/fixture-a.rpm", "sha256": sha, "pkgId": pkg_id, "checksum_type": kind }]);
    for (api, pkg_id, kind, ok) in [
        (Value::Null, good.as_str(), "sha256", true),
        (Value::Null, "00", "sha256", false),
        (Value::Null, good.as_str(), "sha1", false),
        (json!(good), "00", "sha1", true),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("{API}content/rpm/packages/")))
            .respond_with(page(pkg(&api, pkg_id, kind)))
            .mount(&server)
            .await;
        served("repodata/repomd.xml", "<repomd/>").mount(&server).await;
        served("Packages/f/fixture-a.rpm", "rpm bytes").mount(&server).await;
        unsigned_repo(&server).await;
        let c = client(&server, |_, _| {});
        let res = pulp_tui::pulp::rpm::verify(&c, &format!("{}/pulp/content/d/", server.uri()), RPM_V, "").await;
        assert_eq!(res.is_ok(), ok, "api {api}, pkgId {pkg_id} ({kind}): {res:?}");
    }

    let manifest = format!("a.txt,{good},9\nb.txt,{good},9\n");
    let file = |p: &str, sha: Value| json!({ "pulp_href": format!("/f/{p}/"), "relative_path": p, "sha256": sha });
    for (files, ok) in [
        (json!([file("a.txt", json!(good)), file("b.txt", Value::Null)]), true),
        (json!([file("a.txt", json!("00")), file("b.txt", Value::Null)]), false),
        (json!([file("a.txt", Value::Null)]), false),
        (json!([file("a.txt", Value::Null), file("c.txt", Value::Null)]), false),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path(format!("{API}content/file/files/"))).respond_with(page(files.clone())).mount(&server).await;
        served("PULP_MANIFEST", &manifest).mount(&server).await;
        let c = client(&server, |_, _| {});
        let res = pulp_tui::pulp::file::verify(&c, &format!("{}/pulp/content/d/", server.uri()), "/v/", Some("PULP_MANIFEST")).await;
        assert_eq!(res.is_ok(), ok, "{files}: {res:?}");
    }
}

#[tokio::test]
async fn file_verify_reads_paths_with_commas() {
    let good = hex::encode(Sha256::digest(b"file bytes"));
    let page = |results: Value| {
        ResponseTemplate::new(200).set_body_json(json!({ "count": results.as_array().unwrap().len(), "next": null, "results": results }))
    };
    let served = |p: &str, body: &str| {
        Mock::given(method("GET"))
            .and(path(format!("/pulp/content/d/{p}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(body.to_string()))
    };
    let file = |p: &str| json!({ "pulp_href": format!("/f/{p}/"), "relative_path": p, "sha256": good });
    for (manifest, ok) in
        [(format!("q1,2024.txt,{good},9\na,b,c.txt,{good},9\n"), true), (format!("q1,2024.txt,00,9\na,b,c.txt,{good},9\n"), false)]
    {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("{API}content/file/files/")))
            .respond_with(page(json!([file("q1,2024.txt"), file("a,b,c.txt")])))
            .mount(&server)
            .await;
        served("PULP_MANIFEST", &manifest).mount(&server).await;
        let c = client(&server, |_, _| {});
        let res = pulp_tui::pulp::file::verify(&c, &format!("{}/pulp/content/d/", server.uri()), "/v/", Some("PULP_MANIFEST")).await;
        assert_eq!(res.is_ok(), ok, "{manifest}: {res:?}");
    }
}

/// pulp_file gives a publication `PULP_MANIFEST` unless the request names one; autopublish uses the repository's.
#[tokio::test]
async fn file_publication_takes_the_manifest_of_the_repository() {
    let repo = format!("{API}repositories/file/file/1/");
    let version = format!("{repo}versions/2/");
    for manifest in [json!("CUSTOM"), Value::Null] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(&repo))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "pulp_href": repo, "manifest": manifest })))
            .mount(&server)
            .await;
        let coll = format!("{API}publications/file/file/");
        Mock::given(method("POST"))
            .and(path(&coll))
            .and(body_json(json!({ "repository_version": version, "manifest": manifest })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "pulp_href": format!("{coll}1/") })))
            .expect(1)
            .mount(&server)
            .await;
        let c = client(&server, |_, _| {});
        assert_eq!(ops::create_publication(&c, RepoType::File, &version, &json!({}), &no_progress).await.unwrap(), format!("{coll}1/"));
        server.verify().await;
    }
}

#[tokio::test]
async fn file_verify_reads_the_manifest_of_the_publication() {
    let server = MockServer::start().await;
    let good = hex::encode(Sha256::digest(b"file bytes"));
    let files = json!([{ "pulp_href": "/f/a/", "relative_path": "a.txt", "sha256": good }]);
    Mock::given(method("GET"))
        .and(path(format!("{API}content/file/files/")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "count": 1, "next": null, "results": files })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/pulp/content/d/CUSTOM"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!("a.txt,{good},10\n")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/pulp/content/d/OLD"))
        .respond_with(ResponseTemplate::new(200).set_body_string("a.txt,00,10\n"))
        .mount(&server)
        .await;
    let c = client(&server, |_, _| {});
    let base = format!("{}/pulp/content/d/", server.uri());
    let verify = |p: Value| {
        let (c, base) = (&c, &base);
        async move { ops::verify_served(c, RepoType::File, base, "/v/", &p).await }
    };
    assert_eq!(verify(json!({ "manifest": "CUSTOM" })).await.unwrap(), "CUSTOM matches 1 files");
    let e = |m: &str| verify(json!({ "manifest": m }));
    assert_eq!(e("PULP_MANIFEST").await.unwrap_err().to_string(), "served content differs: PULP_MANIFEST returned HTTP 404");
    let old = e("OLD").await.unwrap_err().to_string();
    assert_eq!(old, "served content differs: OLD does not match the version (1 served, 1 expected)");
    assert_eq!(verify(json!({ "manifest": null })).await.unwrap(), "the publication has no manifest: served files not compared");
}

#[tokio::test]
async fn rpm_verify_reads_the_layout_of_the_publication() {
    let server = MockServer::start().await;
    let good = hex::encode(Sha256::digest(b"rpm bytes"));
    let pkgs = json!([{ "location_href": "bash-5.2-1.x86_64.rpm", "sha256": good }]);
    Mock::given(method("GET"))
        .and(path(format!("{API}content/rpm/packages/")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "count": 1, "next": null, "results": pkgs })))
        .mount(&server)
        .await;
    for (p, body) in [("repodata/repomd.xml", "<repomd/>"), ("Packages/bash-5.2-1.x86_64.rpm", "rpm bytes")] {
        Mock::given(method("GET"))
            .and(path(format!("/pulp/content/d/{p}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
    }
    unsigned_repo(&server).await;
    let c = client(&server, |_, _| {});
    let base = format!("{}/pulp/content/d/", server.uri());
    let verify = |p: Value| {
        let (c, base) = (&c, &base);
        async move { ops::verify_served(c, RepoType::Rpm, base, RPM_V, &p).await }
    };
    let ok = "repomd.xml and Packages/bash-5.2-1.x86_64.rpm served with matching digest (1 packages)";
    assert_eq!(verify(json!({ "layout": "flat" })).await.unwrap(), ok);
    // Without a layout the package is looked for at the nested path (Pulp's default), where this one is not.
    let e = verify(json!({})).await.unwrap_err().to_string();
    let tried = "tried Packages/b/bash-5.2-1.x86_64.rpm, bash-5.2-1.x86_64.rpm";
    assert_eq!(e, format!("served content differs: package bash-5.2-1.x86_64.rpm not served with the expected digest ({tried})"));
}

#[tokio::test]
async fn version_content_is_listed_in_large_pages() {
    let server = MockServer::start().await;
    for ct in RepoType::ALL.iter().flat_map(|t| t.content_types()) {
        Mock::given(method("GET"))
            .and(path(format!("{API}{}", ct.path)))
            .and(query_param("limit", "5000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "count": 0, "next": null, "results": [] })))
            .mount(&server)
            .await;
    }
    let c = client(&server, |_, _| {});
    assert_eq!(pulp_tui::pulp::rpm::packages_in(&c, "/v/").await.unwrap().len(), 0);
    assert_eq!(pulp_tui::pulp::file::files_in(&c, "/v/").await.unwrap().len(), 0);
    assert_eq!(deb::packages_in(&c, "/v/").await.unwrap().len(), 0);
    for t in RepoType::ALL {
        assert!(ops::all_content_keys(&c, &Capabilities::default(), t, "/v/").await.unwrap().is_empty(), "{t:?}");
    }
}

#[tokio::test]
async fn a_deb_verbatim_publication_protects_its_version() {
    let server = MockServer::start().await;
    let (repo, version) = (format!("{API}repositories/deb/apt/1/"), format!("{API}repositories/deb/apt/1/versions/2/"));
    let verbatim = format!("{API}publications/deb/verbatim/7/");
    let page =
        |r: Value| ResponseTemplate::new(200).set_body_json(json!({ "count": r.as_array().unwrap().len(), "next": null, "results": r }));
    Mock::given(method("GET")).and(path(format!("{API}publications/deb/apt/"))).respond_with(page(json!([]))).mount(&server).await;
    Mock::given(method("GET"))
        .and(path(format!("{API}publications/deb/verbatim/")))
        .and(query_param("repository", repo.as_str()))
        .respond_with(page(json!([{ "pulp_href": verbatim, "repository_version": version }])))
        .mount(&server)
        .await;
    let d = json!({ "pulp_href": format!("{API}distributions/deb/apt/1/"), "name": "apt", "base_path": "apt", "publication": verbatim });
    Mock::given(method("GET")).and(path(format!("{API}distributions/deb/apt/"))).respond_with(page(json!([d]))).mount(&server).await;
    let c = client(&server, |_, _| {});
    let (pubs, by) = ops::version_protection(&c, &Capabilities::default(), RepoType::Deb, &version).await.unwrap();
    let by: Vec<_> = by.iter().map(|(d, why)| (d.name.as_str(), *why)).collect();
    assert_eq!((pubs.len(), by), (1, vec![("apt", "pinned to a publication of this version")]));

    // A server without verbatim publications is not asked for them.
    let mut caps = Capabilities::default();
    caps.load_openapi(&json!({ "paths": { "/pulp/api/v3/publications/deb/apt/": {} } }));
    let (pubs, by) = ops::version_protection(&c, &caps, RepoType::Deb, &version).await.unwrap();
    assert_eq!((pubs.len(), by.len()), (0, 0));
    let asked = server.received_requests().await.unwrap().iter().filter(|r| r.url.path().contains("/verbatim/")).count();
    assert_eq!(asked, 1, "only the first call lists verbatim publications");
}

/// A GET of `p` that answers `body`.
async fn serve_json(server: &MockServer, p: &str, body: Value) {
    Mock::given(method("GET")).and(path(p)).respond_with(ResponseTemplate::new(200).set_body_json(body)).mount(server).await;
}

/// A prune answers with a task group that never gets `all_tasks_dispatched` (pulp_rpm 3.39.5). Each repository
/// task reports how many packages go (`total`) and names its new version; a failed one fails the prune.
#[tokio::test]
async fn rpm_prune_reports_each_repository_of_its_task_group() {
    let server = MockServer::start().await;
    let (os, extras) = (format!("{API}repositories/rpm/rpm/os/"), format!("{API}repositories/rpm/rpm/extras/"));
    for (keep_days, g) in [(0, 1), (1, 2)] {
        Mock::given(method("POST"))
            .and(path(format!("{API}rpm/prune/")))
            .and(body_json(json!({ "repo_hrefs": [os, extras], "keep_days": keep_days, "dry_run": keep_days == 1 })))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({ "task_group": format!("{API}task-groups/{g}/") })))
            .expect(1)
            .mount(&server)
            .await;
    }
    let task = |n: u32, name: &str, state: &str| json!({ "pulp_href": format!("{API}tasks/{n}/"), "name": format!("pulp_rpm.app.tasks.prune.{name}"), "state": state });
    let group = |g: u32, tasks: Vec<Value>, counts: Value| {
        let mut v = json!({ "pulp_href": format!("{API}task-groups/{g}/"), "description": "Prune old Packages.", "all_tasks_dispatched": false, "tasks": tasks });
        v.as_object_mut().unwrap().extend(counts.as_object().unwrap().clone());
        v
    };
    let tasks = vec![
        task(1, "prune_packages", "completed"),
        task(2, "prune_repo_packages", "completed"),
        task(3, "prune_repo_packages", "completed"),
    ];
    serve_json(&server, &format!("{API}task-groups/1/"), group(1, tasks, json!({ "completed": 3 }))).await;
    let report = |name: &str, n: u64| json!([{ "code": "rpm.package.prune.repository", "message": format!("Pruning {name}"), "total": n, "done": n }]);
    let new = format!("{os}versions/3/");
    serve_json(&server, &format!("{API}tasks/2/"), json!({ "pulp_href": format!("{API}tasks/2/"), "state": "completed", "progress_reports": report("os", 2), "created_resources": [new] })).await;
    serve_json(
        &server,
        &format!("{API}tasks/3/"),
        // A report without `total`: no package goes.
        json!({ "pulp_href": format!("{API}tasks/3/"), "state": "completed",
            "progress_reports": [{ "code": "rpm.package.prune.repository", "message": "Pruning extras" }] }),
    )
    .await;
    // The new version lists none of the packages that it has as removed.
    Mock::given(method("GET"))
        .and(path(format!("{API}content/rpm/packages/")))
        .and(query_param("repository_version", &new))
        .and(query_param("repository_version_removed", &new))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "count": 0, "results": [] })))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server, |_, _| {});
    let repos = [os.clone(), extras.clone()];
    let got = rpm::prune(&c, &repos, 0, false, &|_| {}).await.unwrap();
    let want = [
        rpm::Pruned { repo: "extras".into(), packages: 0, version: None, not_removed: 0 },
        rpm::Pruned { repo: "os".into(), packages: 2, version: Some(new), not_removed: 0 },
    ];
    assert_eq!(got, want, "by repository name");

    let tasks = vec![task(4, "prune_packages", "completed"), task(5, "prune_repo_packages", "failed")];
    serve_json(&server, &format!("{API}task-groups/2/"), group(2, tasks, json!({ "completed": 1, "failed": 1 }))).await;
    let failed = json!({ "pulp_href": format!("{API}tasks/5/"), "state": "failed", "error": { "description": "repository is locked" } });
    serve_json(&server, &format!("{API}tasks/5/"), failed).await;
    let e = rpm::prune(&c, &repos, 1, true, &|_| {}).await.unwrap_err();
    assert!(
        matches!(&e, PulpError::TaskFailed { state, message, .. } if state == "failed" && message.contains("repository is locked")),
        "{e}"
    );
}
