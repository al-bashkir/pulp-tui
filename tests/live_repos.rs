//! End-to-end RPM and File repository lifecycles against a live Pulp:
//! local repo + upload, remote + sync, versions, publication, distribution, served
//! content, updated content, rollback by re-pointing the distribution.

mod common;

use common::*;
use pulp_tui::pulp::models::{Package, RepoVersion};
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{Mutation, PulpClient, RepoType, file, rpm};
use serde_json::{Value, json};

async fn dist_url(c: &PulpClient, dist: &str) -> String {
    let d: Value = c.get(dist, &[]).await.unwrap();
    d["base_url"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn rpm_lifecycle() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::Rpm;
    let name = unique("it-rpm");

    // Local repository populated by upload; the digest is verified by ops::upload.
    let local = create(&c, t.repositories(), json!({ "name": format!("{name}-local") }), "/repositories/").await;
    let rpm = fixtures().join("rpms/fixture-upload-3.0-1.noarch.rpm");
    let up = ops::upload(&c, t, &rpm, None, Some(&local), &|_, _| {}, &no_progress).await.unwrap();
    assert!(up.content_href.contains("/content/rpm/packages/"));
    assert!(latest(&c, &local).await.ends_with("/versions/1/"));
    // Uploading the same package again is idempotent for content and creates no new version.
    ops::upload(&c, t, &rpm, None, Some(&local), &|_, _| {}, &no_progress).await.unwrap();
    assert!(latest(&c, &local).await.ends_with("/versions/1/"));

    // Remote + sync; advisories survive (metadata is not dropped).
    let (repo, remote) = synced_repo(&c, t, &name, "rpm/", "mirror_complete").await;
    let v1 = latest(&c, &repo).await;
    let names: Vec<String> = rpm::packages_in(&c, &v1).await.unwrap().iter().map(Package::nevra).collect();
    assert_eq!(names.len(), 3, "{names:?}");
    assert!(names.contains(&"fixture-alpha-1.0-1.noarch".to_string()));
    let rv: RepoVersion = c.get(&v1, &[]).await.unwrap();
    assert_eq!(rv.content_summary.present["rpm.advisory"].count, 1);

    // Publish exact version and distribute it.
    let pub1 = ops::ensure_publication(&c, t, &v1, None, &no_progress).await.unwrap();
    assert_eq!(ops::ensure_publication(&c, t, &v1, None, &no_progress).await.unwrap(), pub1, "publication is reused");
    let dist = ops::ensure_distribution(&c, t, &name, &name, Some(&pub1), &json!({}), None, &no_progress).await.unwrap();
    let base = dist_url(&c, &dist).await;
    let msg = ops::verify_served(&c, t, &base, &v1, "nested_alphabetically").await.unwrap();
    assert!(msg.contains("matching digest"), "{msg}");
    let repomd1 = served(&c, &format!("{base}repodata/repomd.xml")).await.1;
    let alpha11 = format!("{base}Packages/f/fixture-alpha-1.1-1.noarch.rpm");
    assert_eq!(served(&c, &alpha11).await.0, 404);

    // Upstream changes: point the remote to the newer repository and sync additively.
    let m = c.patch(&remote, &json!({ "url": format!("{}/rpm2/", upstream()) })).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let m = ops::sync(&c, t, &repo, None, "additive").await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let v2 = latest(&c, &repo).await;
    assert_ne!(v1, v2);
    let d = ops::diff(&ops::content_keys(&c, t, Some(&v1)).await.unwrap(), &ops::content_keys(&c, t, Some(&v2)).await.unwrap());
    assert_eq!(d.added, vec!["fixture-alpha-1.1-1.noarch", "fixture-gamma-0.5-1.noarch"]);
    assert!(d.removed.is_empty());

    let pub2 = ops::ensure_publication(&c, t, &v2, None, &no_progress).await.unwrap();
    ops::ensure_distribution(&c, t, &name, &name, Some(&pub2), &json!({}), None, &no_progress).await.unwrap();
    assert_eq!(served(&c, &alpha11).await.0, 200);
    assert_ne!(served(&c, &format!("{base}repodata/repomd.xml")).await.1, repomd1);

    // Rollback = re-point to the older publication. Newer versions stay. Metadata switches
    // immediately; files of the superseded publication may stay downloadable during
    // DISTRIBUTED_PUBLICATION_RETENTION_PERIOD (pulpcore grace period), so no 404 check here.
    ops::ensure_distribution(&c, t, &name, &name, Some(&pub1), &json!({}), None, &no_progress).await.unwrap();
    assert_eq!(served(&c, &format!("{base}repodata/repomd.xml")).await.1, repomd1);
    ops::verify_served(&c, t, &base, &v1, "nested_alphabetically").await.unwrap();
    let _: RepoVersion = c.get(&v2, &[]).await.unwrap();

    // A base path owned by another distribution is refused.
    let err =
        ops::ensure_distribution(&c, t, &format!("{name}-other"), &name, Some(&pub1), &json!({}), None, &no_progress).await.unwrap_err();
    assert_eq!(err.status(), Some(409), "{err}");

    cleanup(&c, &[&dist, &repo, &local, &remote]).await;
}

#[tokio::test]
async fn file_lifecycle() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::File;
    let name = unique("it-file");

    // Local repository: small single-request upload and a chunked upload (> 1 MiB chunk).
    let local = create(&c, t.repositories(), json!({ "name": format!("{name}-local") }), "/repositories/").await;
    ops::upload(&c, t, &fixtures().join("files/one.txt"), Some("a/one.txt"), Some(&local), &|_, _| {}, &no_progress).await.unwrap();
    let progress = std::sync::Mutex::new(vec![]);
    let up = ops::upload(
        &c,
        t,
        &fixtures().join("files/big.bin"),
        Some("big.bin"),
        Some(&local),
        &|d, _| progress.lock().unwrap().push(d),
        &no_progress,
    )
    .await
    .unwrap();
    assert!(progress.lock().unwrap().len() >= 4, "chunked upload reports progress");
    assert_eq!(up.sha256.len(), 64);
    let files: Vec<String> = file::files_in(&c, &latest(&c, &local).await).await.unwrap().into_iter().map(|f| f.relative_path).collect();
    assert_eq!(files.len(), 2);
    let uploads: Value = c.get(&c.api("uploads/"), &[]).await.unwrap();
    assert_eq!(uploads["count"], 0, "no leftover chunked uploads");

    // Remove content without touching older versions.
    let one = file::files_in(&c, &latest(&c, &local).await).await.unwrap().into_iter().find(|f| f.relative_path == "a/one.txt").unwrap();
    let before = latest(&c, &local).await;
    let m = c.post(&format!("{local}modify/"), &json!({ "remove_content_units": [one.pulp_href] })).await.unwrap();
    assert!(matches!(m, Mutation::Task(_)));
    ops::settle(&c, m, &no_progress).await.unwrap();
    assert_eq!(file::files_in(&c, &before).await.unwrap().len(), 2, "old version unchanged");
    assert_eq!(file::files_in(&c, &latest(&c, &local).await).await.unwrap().len(), 1);

    // Sync from a manifest, publish, distribute, verify served manifest.
    let (repo, remote) = synced_repo(&c, t, &name, "file/PULP_MANIFEST", "mirror").await;
    let v1 = latest(&c, &repo).await;
    assert_eq!(file::files_in(&c, &v1).await.unwrap().len(), 3);
    let pub1 = ops::ensure_publication(&c, t, &v1, None, &no_progress).await.unwrap();
    let dist = ops::ensure_distribution(&c, t, &name, &name, Some(&pub1), &json!({}), None, &no_progress).await.unwrap();
    let base = dist_url(&c, &dist).await;
    ops::verify_served(&c, t, &base, &v1, "").await.unwrap();
    let (status, body) = served(&c, &format!("{base}docs/readme.txt")).await;
    assert_eq!((status, body.as_slice()), (200, &b"readme v1\n"[..]));

    // New content (copy a unit from the local repo), publish, serve, then roll back.
    let big = file::files_in(&c, &latest(&c, &local).await).await.unwrap().remove(0);
    let m = c.post(&format!("{repo}modify/"), &json!({ "add_content_units": [big.pulp_href] })).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let v2 = latest(&c, &repo).await;
    let pub2 = ops::ensure_publication(&c, t, &v2, None, &no_progress).await.unwrap();
    ops::ensure_distribution(&c, t, &name, &name, Some(&pub2), &json!({}), None, &no_progress).await.unwrap();
    assert_eq!(served(&c, &format!("{base}big.bin")).await.0, 200);
    ops::verify_served(&c, t, &base, &v2, "").await.unwrap();
    // Rollback: the served manifest is exactly v1 again (see grace-period note in rpm_lifecycle).
    ops::ensure_distribution(&c, t, &name, &name, Some(&pub1), &json!({}), None, &no_progress).await.unwrap();
    ops::verify_served(&c, t, &base, &v1, "").await.unwrap();
    assert!(ops::verify_served(&c, t, &base, &v2, "").await.is_err(), "detects mismatch");

    cleanup(&c, &[&dist, &repo, &local, &remote]).await;
}

#[tokio::test]
async fn file_path_with_a_comma_is_served_and_verified() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::File;
    let name = unique("it-comma");
    let repo = create(&c, t.repositories(), json!({ "name": name }), "/repositories/").await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("q1.txt");
    // Unique bytes, so a rerun does not find the content unit of an earlier run.
    std::fs::write(&path, format!("{name}\n")).unwrap();
    ops::upload(&c, t, &path, Some("reports/q1,2024.txt"), Some(&repo), &|_, _| {}, &no_progress).await.unwrap();
    let version = latest(&c, &repo).await;
    let publication = ops::ensure_publication(&c, t, &version, None, &no_progress).await.unwrap();
    let dist = ops::ensure_distribution(&c, t, &name, &name, Some(&publication), &json!({}), None, &no_progress).await.unwrap();
    let base = dist_url(&c, &dist).await;
    let msg = ops::verify_served(&c, t, &base, &version, "").await;
    cleanup(&c, &[&dist, &publication, &repo]).await;
    assert_eq!(msg.unwrap(), "PULP_MANIFEST matches 1 files");
}
