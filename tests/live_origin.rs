//! A Pulp without CONTENT_ORIGIN (the default of pulpcore 3.95 and later): a distribution's `base_url` is a path on
//! the server. Requires `tests/env/pulp-env.sh up pulp3` (port 8082); `tests/run-live.sh origin` starts it.

mod common;

use common::*;
use pulp_tui::cv::CvService;
use pulp_tui::pulp::RepoType;
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::store::CvRepo;
use pulp_tui::tui::actions::run;
use pulp_tui::tui::app::Modal;
use pulp_tui::tui::kinds::{Act, Kind};
use serde_json::{Value, json};

fn quiet(_: String) {}

fn url() -> String {
    std::env::var("PULP_TUI_TEST_NO_ORIGIN_URL").unwrap_or_else(|_| "http://127.0.0.1:8082".into())
}

/// F99 (R44, R45) through the TUI on pulp3: the Dashboard names where content is served; Verify (V) of a File
/// distribution reads its manifest at the URL joined with the profile URL and says OK; Copy URL (y) copies that URL;
/// the drift check (D) of a published Content View finds Library OK.
#[tokio::test]
async fn served_content_is_found_on_a_server_without_a_content_origin_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (url, id) = (url(), unique("f99"));
    let c = client_for(&url, "pulp3");
    let (repo, remote) = synced_repo(&c, RepoType::File, &id, "file/PULP_MANIFEST", "mirror").await;
    let p = ops::create_publication(&c, RepoType::File, &latest(&c, &repo).await, &json!({}), &no_progress).await.unwrap();
    let dist = ops::ensure_distribution(&c, RepoType::File, &id, &id, Some(&p), &json!({}), None, &no_progress).await.unwrap();
    let d: Value = c.get(&dist, &[]).await.unwrap();
    assert_eq!(d["base_url"], format!("/pulp/content/{id}/"), "pulp3 has no CONTENT_ORIGIN");
    let served = format!("{url}/pulp/content/{id}/");

    let (mut app, mut rx) = app_at("pulp3", &url).await;
    let content = format!("content: {url}/pulp/content/");
    assert!(app.dashboard.iter().any(|l| l.ends_with(&content)), "{:?}", app.dashboard);
    open_list(&mut app, &mut rx, Kind::Distributions(RepoType::File), &id).await;
    let r = row(&app, "name", &id).unwrap();
    run(&mut app, Act::Verify, r.clone());
    settle(&mut app, &mut rx).await;
    let Some(Modal::Text { lines, .. }) = app.modal.take() else { panic!("no verify result: {:?}", texts(&app)) };
    assert_eq!(lines[2], format!("URL: {served}"));
    assert!(lines.last().is_some_and(|l| l.starts_with("OK: ")), "{lines:?}");
    run(&mut app, Act::CopyUrl, r);
    shown(&app, &format!("copied {served}"));

    // A Content View of the repository: Library serves it at a path too, and the drift check finds it OK.
    let st = app.cv_store.clone().unwrap();
    let scope = app.conn.as_ref().unwrap().scope.clone();
    let svc = CvService { client: c.clone(), store: st.clone(), scope: scope.clone(), actor: "test@live".into(), lock: None };
    let cv = st.create_cv(&scope, &id, "").await.unwrap();
    let docs = CvRepo { repo_href: repo.clone(), repo_name: "docs".into(), repo_type: RepoType::File, filters: vec![] };
    st.set_cv_repo(cv, &docs).await.unwrap();
    svc.publish(cv, "", "", &quiet, &no_progress).await.unwrap();
    open_list(&mut app, &mut rx, Kind::ContentViews, "").await;
    let r = row(&app, "name", &id).unwrap();
    run(&mut app, Act::CvDrift, r);
    settle(&mut app, &mut rx).await;
    let Some(Modal::Text { lines, .. }) = app.modal.take() else { panic!("no drift result: {:?}", texts(&app)) };
    assert_eq!(lines, ["Library: v1 OK - served metadata match; RPM/Deb: also the smallest package"]);

    svc.delete_view(cv, &quiet, &no_progress).await.unwrap();
    cleanup(&c, &[&dist, &repo, &remote]).await;
}
