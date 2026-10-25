//! Deb repositories against a live Pulp with pulp_deb: mirror sync of the Debian fixture,
//! filtered structured copy, publication, distribution and served-content checks.

mod common;

use common::*;
use pulp_tui::filters::{Selector, parse_rules};
use pulp_tui::pulp::client::Page;
use pulp_tui::pulp::models::DebPackage;
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{RepoType, deb};
use serde_json::{Value, json};

#[tokio::test]
async fn deb_mirror_copy_publish_verify() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::Deb;
    let name = unique("it-deb");

    // Remote + repository; a mirror sync brings every fixture package.
    let url = format!("{}/deb/", upstream());
    let body = json!({ "name": name, "url": url, "policy": "immediate", "distributions": "fixture" });
    let remote = create(&c, t.remotes(), body, "/remotes/").await;
    let repo = create(&c, t.repositories(), json!({ "name": name, "remote": remote }), "/repositories/").await;
    let m = ops::sync(&c, t, &repo, None, "mirror").await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let v1 = latest(&c, &repo).await;
    let keys = ops::content_keys(&c, t, Some(&v1)).await.unwrap();
    assert_eq!(keys.len(), 4, "{keys:?}");
    assert!(keys.contains("fixture-beta-dbg_2.0-1_amd64"), "{keys:?}");

    // A second repository gets the packages that pass the filter; the structured copy brings
    // the release and component records with them.
    let copy = create(&c, t.repositories(), json!({ "name": format!("{name}-copy") }), "/repositories/").await;
    let sel = Selector::new(&parse_rules("exclude name=*-dbg", t).unwrap());
    let chosen: Vec<DebPackage> = deb::packages_in(&c, &v1).await.unwrap().into_iter().filter(|p| sel.deb_package(p)).collect();
    let hrefs = chosen.into_iter().map(|p| p.pulp_href).collect();
    let m = deb::copy(&c, &v1, &copy, Some(0), Some(hrefs)).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let v2 = latest(&c, &copy).await;
    let kept = deb::packages_in(&c, &v2).await.unwrap();
    assert_eq!(kept.len(), 3, "{kept:?}");
    let q = [("repository_version", v2.clone()), ("limit", "1".into())];
    let releases: Page<Value> = c.list(&c.api("content/deb/releases/"), &q).await.unwrap();
    assert!(releases.count >= 1, "the copy has a Release");

    // Publish the copy, serve it, and check what clients would download.
    let publication = ops::ensure_publication(&c, t, &v2, &no_progress).await.unwrap();
    let dist = ops::ensure_distribution(&c, t, &name, &name, Some(&publication), &json!({}), None, &no_progress).await.unwrap();
    let d: Value = c.get(&dist, &[]).await.unwrap();
    let base = d["base_url"].as_str().unwrap().to_string();
    let msg = ops::verify_served(&c, t, &base, &v2, "").await.unwrap();
    assert!(msg.contains("matching digest"), "{msg}");
    let (status, index) = served(&c, &format!("{base}dists/fixture/main/binary-amd64/Packages")).await;
    let index = String::from_utf8_lossy(&index);
    assert_eq!(status, 200, "{index}");
    assert!(index.contains("Package: fixture-beta\n"), "{index}");
    assert!(!index.contains("fixture-beta-dbg"), "the filtered package is not served");

    cleanup(&c, &[&dist, &repo, &copy, &remote]).await;
}
