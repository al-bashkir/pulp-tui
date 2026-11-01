//! End-to-end RPM and File repository lifecycles against a live Pulp:
//! local repo + upload, remote + sync, versions, publication, distribution, served
//! content, updated content, rollback by re-pointing the distribution.

mod common;

use common::*;
use pulp_tui::pulp::models::{Package, RepoVersion};
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{Capabilities, Mutation, PulpClient, RepoType, file, rpm};
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
    // Versions -> Compare covers every content type: the sync (v0 -> v1) also added the advisory.
    let v0 = format!("{repo}versions/0/");
    let k1 = ops::all_content_keys(&c, &Capabilities::default(), t, &v1).await.unwrap();
    assert_eq!(rv.content_summary.present_count(), k1.len() as u64, "Pulp counts the units the compare lists");
    let d = ops::diff(&ops::all_content_keys(&c, &Capabilities::default(), t, &v0).await.unwrap(), &k1);
    assert_eq!(
        d.added,
        [
            "advisory FIXTURE-2026:0001",
            "package fixture-alpha-1.0-1.noarch",
            "package fixture-beta-2.0-1.noarch",
            "package fixture-beta-debuginfo-2.0-1.noarch"
        ]
    );
    assert!(d.removed.is_empty());

    // Publish exact version and distribute it.
    let pub1 = ops::ensure_publication(&c, t, &v1, None, &no_progress).await.unwrap();
    assert_eq!(ops::ensure_publication(&c, t, &v1, None, &no_progress).await.unwrap(), pub1, "publication is reused");
    let dist = ops::ensure_distribution(&c, t, &name, &name, Some(&pub1), &json!({}), None, &no_progress).await.unwrap();
    let base = dist_url(&c, &dist).await;
    let msg = ops::verify_served(&c, t, &base, &v1, &json!({ "layout": "nested_alphabetically" })).await.unwrap();
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
    ops::verify_served(&c, t, &base, &v1, &json!({ "layout": "nested_alphabetically" })).await.unwrap();
    let _: RepoVersion = c.get(&v2, &[]).await.unwrap();

    // A base path owned by another distribution is refused.
    let err =
        ops::ensure_distribution(&c, t, &format!("{name}-other"), &name, Some(&pub1), &json!({}), None, &no_progress).await.unwrap_err();
    assert_eq!(err.status(), Some(409), "{err}");

    // Restore as latest: pulp_rpm takes `base_version` too; the new version has every unit of v1.
    let m = ops::restore_version(&c, &repo, &v1).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let v3 = latest(&c, &repo).await;
    assert!(v3.ends_with("/versions/3/"), "{v3}");
    assert_eq!(
        ops::all_content_keys(&c, &Capabilities::default(), t, &v3).await.unwrap(),
        ops::all_content_keys(&c, &Capabilities::default(), t, &v1).await.unwrap()
    );

    cleanup(&c, &[&dist, &repo, &local, &remote]).await;
}

/// A comps-only change (`rpm/comps/` upload) shows in the version compare, and Pulp counts the units it lists.
#[tokio::test]
async fn rpm_compare_lists_comps_units() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::Rpm;
    let name = unique("it-comps");
    let repo = create(&c, t.repositories(), json!({ "name": name }), "/repositories/").await;
    // Ids with the unique name, so a rerun does not find the units of an earlier run.
    let comps = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE comps PUBLIC "-//Red Hat, Inc.//DTD Comps info//EN" "comps.dtd">
<comps>
  <group><id>{name}-group</id><name>G</name><description>g</description><default>false</default><uservisible>true</uservisible>
    <packagelist><packagereq type="mandatory">fixture-alpha</packagereq></packagelist></group>
  <category><id>{name}-category</id><name>C</name><description>c</description>
    <grouplist><groupid>{name}-group</groupid></grouplist></category>
  <environment><id>{name}-env</id><name>E</name><description>e</description>
    <grouplist><groupid>{name}-group</groupid></grouplist><optionlist></optionlist></environment>
  <langpacks><match install="{name}-%s" name="{name}"/></langpacks>
</comps>
"#
    );
    let m =
        c.post_form(&c.api("rpm/comps/"), &[("repository", repo.clone())], Some(("comps.xml".into(), comps.into_bytes()))).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let (v0, v1) = (format!("{repo}versions/0/"), latest(&c, &repo).await);
    let keys = ops::all_content_keys(&c, &Capabilities::default(), t, &v1).await.unwrap();
    let d = ops::diff(&ops::all_content_keys(&c, &Capabilities::default(), t, &v0).await.unwrap(), &keys);
    let langpacks = d.added.iter().find(|k| k.starts_with("package langpacks ")).cloned().unwrap_or_default();
    let want =
        [format!("package category {name}-category"), format!("package environment {name}-env"), format!("package group {name}-group")];
    assert_eq!(d.added, [&want[..], &[langpacks]].concat(), "the compare lists every comps unit");
    let v: RepoVersion = c.get(&v1, &[]).await.unwrap();
    assert_eq!(v.content_summary.present_count(), keys.len() as u64, "Pulp counts the units the compare lists");
    cleanup(&c, &[&repo]).await;
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
    let m = c.post(&format!("{local}modify/"), &json!({ "remove_content_units": [&one.pulp_href] })).await.unwrap();
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
    ops::verify_served(&c, t, &base, &v1, &c.get(&pub1, &[]).await.unwrap()).await.unwrap();
    let (status, body) = served(&c, &format!("{base}docs/readme.txt")).await;
    assert_eq!((status, body.as_slice()), (200, &b"readme v1\n"[..]));

    // New content (copy a unit from the local repo), publish, serve, then roll back.
    let big = file::files_in(&c, &latest(&c, &local).await).await.unwrap().remove(0);
    let m = c.post(&format!("{repo}modify/"), &json!({ "add_content_units": [&big.pulp_href] })).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let v2 = latest(&c, &repo).await;
    let pub2 = ops::ensure_publication(&c, t, &v2, None, &no_progress).await.unwrap();
    ops::ensure_distribution(&c, t, &name, &name, Some(&pub2), &json!({}), None, &no_progress).await.unwrap();
    assert_eq!(served(&c, &format!("{base}big.bin")).await.0, 200);
    ops::verify_served(&c, t, &base, &v2, &c.get(&pub2, &[]).await.unwrap()).await.unwrap();
    // Rollback: the served manifest is exactly v1 again (see grace-period note in rpm_lifecycle).
    ops::ensure_distribution(&c, t, &name, &name, Some(&pub1), &json!({}), None, &no_progress).await.unwrap();
    ops::verify_served(&c, t, &base, &v1, &c.get(&pub1, &[]).await.unwrap()).await.unwrap();
    let e = ops::verify_served(&c, t, &base, &v2, &c.get(&pub1, &[]).await.unwrap()).await.unwrap_err().to_string();
    assert_eq!(e, "served content differs: PULP_MANIFEST does not match the version (3 served, 4 expected)");

    // A custom manifest name: Publish keeps it, and Verify reads the one of the autopublished publication.
    let custom = json!({ "name": format!("{name}-custom"), "manifest": "CUSTOM_MANIFEST", "autopublish": true });
    let custom = create(&c, t.repositories(), custom, "/repositories/").await;
    let m = c.post(&format!("{custom}modify/"), &json!({ "add_content_units": [&big.pulp_href] })).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let published = ops::create_publication(&c, t, &latest(&c, &custom).await, None, &no_progress).await.unwrap();
    assert_eq!(c.get::<Value>(&published, &[]).await.unwrap()["manifest"], "CUSTOM_MANIFEST");
    let m = c.post(&format!("{custom}modify/"), &json!({ "add_content_units": [&one.pulp_href] })).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let v = latest(&c, &custom).await;
    let auto: Vec<Value> = c.list_all(&c.api(t.publications()), &[("repository_version", v.clone())], 10).await.unwrap();
    let follow = json!({ "name": format!("{name}-custom"), "base_path": format!("{name}-custom"), "repository": custom });
    let follow = create(&c, t.distributions(), follow, "/distributions/").await;
    let msg = ops::verify_served(&c, t, &dist_url(&c, &follow).await, &v, &auto[0]).await;
    assert_eq!(msg.unwrap(), "CUSTOM_MANIFEST matches 2 files");

    cleanup(&c, &[&follow, &custom, &dist, &repo, &local, &remote]).await;
}

/// The version delete preview predicts Pulp's `protected_versions` refusal (HTTP 400).
#[tokio::test]
async fn served_versions_are_protected_from_delete() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::File;
    let name = unique("it-protect");
    let repo = create(&c, t.repositories(), json!({ "name": name }), "/repositories/").await;
    let dir = tempfile::tempdir().unwrap();
    let mut v = vec![];
    for i in 1..=3 {
        let path = dir.path().join(format!("{i}.txt"));
        // Unique bytes, so a rerun does not find the content unit of an earlier run.
        std::fs::write(&path, format!("{name} {i}\n")).unwrap();
        ops::upload(&c, t, &path, Some(&format!("{i}.txt")), Some(&repo), &|_, _| {}, &no_progress).await.unwrap();
        let version = latest(&c, &repo).await;
        ops::ensure_publication(&c, t, &version, None, &no_progress).await.unwrap();
        v.push(version);
    }
    let pub2 = ops::ensure_publication(&c, t, &v[1], None, &no_progress).await.unwrap();
    let pin = ops::ensure_distribution(&c, t, &format!("{name}-pin"), &format!("{name}-pin"), Some(&pub2), &json!({}), None, &no_progress)
        .await
        .unwrap();
    let follow = json!({ "name": format!("{name}-follow"), "base_path": format!("{name}-follow"), "repository": repo });
    let follow = create(&c, t.distributions(), follow, "/distributions/").await;

    let caps = Capabilities::default(); // no capability changes File protection
    let why = |by: Vec<(pulp_tui::pulp::models::Distribution, &'static str)>| -> Vec<(String, &str)> {
        by.into_iter().map(|(d, why)| (d.name, why)).collect()
    };
    let (pubs, by) = ops::version_protection(&c, &caps, t, &v[1]).await.unwrap();
    assert_eq!((pubs.len(), why(by)), (1, vec![(format!("{name}-pin"), "pinned to a publication of this version")]));
    let (_, by) = ops::version_protection(&c, &caps, t, &v[2]).await.unwrap();
    assert_eq!(why(by), [(format!("{name}-follow"), "follows the repository; this is its newest published version")]);
    // Pulp agrees: both deletes are refused before any task starts.
    for version in &v[1..] {
        let err = c.delete(version).await.unwrap_err();
        assert!(err.status() == Some(400) && err.to_string().contains("currently being used to distribute content"), "{err}");
    }

    // v1 is published, but nothing serves it: not protected, and the delete completes.
    let (pubs, by) = ops::version_protection(&c, &caps, t, &v[0]).await.unwrap();
    assert_eq!((pubs.len(), by.len()), (1, 0));
    let m = c.delete(&v[0]).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    assert!(c.get::<Value>(&v[0], &[]).await.unwrap_err().is_not_found());
    assert!(c.get::<Value>(&pubs[0].pulp_href, &[]).await.unwrap_err().is_not_found(), "its publication is deleted too");

    cleanup(&c, &[&pin, &follow, &repo]).await;
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
    let msg = ops::verify_served(&c, t, &base, &version, &c.get(&publication, &[]).await.unwrap()).await;
    cleanup(&c, &[&dist, &publication, &repo]).await;
    assert_eq!(msg.unwrap(), "PULP_MANIFEST matches 1 files");
}

/// Restore as latest: `modify/` with `base_version` makes a new version with the content of an older one;
/// a distribution that follows the repository serves it after the publish (no autopublish here).
/// A second restore of the same content makes no version.
#[tokio::test]
async fn restored_version_is_served_after_publish() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::File;
    let name = unique("it-restore");
    let repo = create(&c, t.repositories(), json!({ "name": name }), "/repositories/").await;
    let dir = tempfile::tempdir().unwrap();
    let mut v = vec![];
    for i in 1..=2 {
        let path = dir.path().join(format!("{i}.txt"));
        // Unique bytes, so a rerun does not find the content unit of an earlier run.
        std::fs::write(&path, format!("{name} {i}\n")).unwrap();
        ops::upload(&c, t, &path, Some(&format!("{i}.txt")), Some(&repo), &|_, _| {}, &no_progress).await.unwrap();
        v.push(latest(&c, &repo).await);
    }
    let follow = json!({ "name": name, "base_path": name, "repository": repo });
    let follow = create(&c, t.distributions(), follow, "/distributions/").await;
    let base = dist_url(&c, &follow).await;
    let p2: Value = c.get(&ops::create_publication(&c, t, &v[1], None, &no_progress).await.unwrap(), &[]).await.unwrap();
    ops::verify_served(&c, t, &base, &v[1], &p2).await.unwrap();

    let m = ops::restore_version(&c, &repo, &v[0]).await.unwrap();
    let task = ops::settle(&c, m, &no_progress).await.unwrap().unwrap();
    let v3 = latest(&c, &repo).await;
    assert_eq!(task.created_matching("/versions/").as_deref(), Some(v3.as_str()));
    assert!(v3.ends_with("/versions/3/"), "{v3}");
    assert_eq!(
        ops::all_content_keys(&c, &Capabilities::default(), t, &v3).await.unwrap(),
        ops::all_content_keys(&c, &Capabilities::default(), t, &v[0]).await.unwrap()
    );
    let _: Value = c.get(&v[1], &[]).await.unwrap(); // the newer version is kept
    ops::verify_served(&c, t, &base, &v[1], &p2).await.unwrap(); // not served before a publish
    let p3: Value = c.get(&ops::create_publication(&c, t, &v3, None, &no_progress).await.unwrap(), &[]).await.unwrap();
    assert_eq!(ops::verify_served(&c, t, &base, &v3, &p3).await.unwrap(), "PULP_MANIFEST matches 1 files");

    let m = ops::restore_version(&c, &repo, &v[0]).await.unwrap();
    let task = ops::settle(&c, m, &no_progress).await.unwrap().unwrap();
    assert_eq!((task.created_matching("/versions/"), latest(&c, &repo).await), (None, v3), "same content: no new version");
    cleanup(&c, &[&follow, &repo]).await;
}
