//! End-to-end RPM and File repository lifecycles against a live Pulp:
//! local repo + upload, remote + sync, versions, publication, distribution, served
//! content, updated content, rollback by re-pointing the distribution.

mod common;

use common::*;
use pulp_tui::pulp::models::{Package, RepoVersion, Task};
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{Capabilities, Mutation, PulpClient, RepoType, file, rpm};
use pulp_tui::tui::actions::run;
use pulp_tui::tui::app::{App, Modal, Msg};
use pulp_tui::tui::kinds::{Act, Kind, s, short};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

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
    let up = ops::upload(&c, t, &rpm, None, Some(&local), true, &|_, _| {}, &no_progress).await.unwrap();
    assert!(up.content_href.contains("/content/rpm/packages/"));
    assert!(latest(&c, &local).await.ends_with("/versions/1/"));
    // Uploading the same package again is idempotent for content and creates no new version.
    ops::upload(&c, t, &rpm, None, Some(&local), true, &|_, _| {}, &no_progress).await.unwrap();
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
    let pub1 = ops::ensure_publication(&c, t, &v1, &json!({}), &no_progress).await.unwrap();
    assert_eq!(ops::ensure_publication(&c, t, &v1, &json!({}), &no_progress).await.unwrap(), pub1, "publication is reused");
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

    let pub2 = ops::ensure_publication(&c, t, &v2, &json!({}), &no_progress).await.unwrap();
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

/// v1 is published unsigned, then the repository gets a metadata signing service. A rollback to v1 (the TUI
/// path: the mode of the served publication, `ensure_publication`, re-point, verify) makes a new, signed
/// publication: pulp_rpm stores no signing service in a publication, so the unsigned one cannot be told apart.
/// Needs `tests/env/pulp-env.sh rpm-signing-up`.
#[tokio::test]
async fn rpm_rollback_of_a_signing_repository_serves_signed_metadata() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::Rpm;
    let name = unique("it-rpm-signed");
    let (repo, remote) = synced_repo(&c, t, &name, "rpm/", "additive").await;
    let v1 = latest(&c, &repo).await;
    let unsigned = ops::ensure_publication(&c, t, &v1, &json!({}), &no_progress).await.unwrap();
    ops::settle(&c, c.patch(&remote, &json!({ "url": format!("{}/rpm2/", upstream()) })).await.unwrap(), &no_progress).await.unwrap();
    ops::settle(&c, ops::sync(&c, t, &repo, None, "additive").await.unwrap(), &no_progress).await.unwrap();
    let v2 = latest(&c, &repo).await;
    let pub2 = ops::create_publication(&c, t, &v2, &json!({}), &no_progress).await.unwrap();
    let dist = ops::ensure_distribution(&c, t, &name, &name, Some(&pub2), &json!({}), None, &no_progress).await.unwrap();

    let svc: Value = ops::find_by_name(&c, &c.api("signing-services/"), "pulp-tui-rpm-test")
        .await
        .unwrap()
        .expect("RPM test signing service (tests/env/pulp-env.sh rpm-signing-up)");
    ops::settle(&c, c.patch(&repo, &json!({ "metadata_signing_service": svc["pulp_href"] })).await.unwrap(), &no_progress).await.unwrap();

    let d: Value = c.get(&dist, &[]).await.unwrap();
    let now = ops::served_publication(&c, &Capabilities::default(), t, &d).await.unwrap().unwrap();
    let p = ops::ensure_publication(&c, t, &v1, &ops::publication_mode(t, &now), &no_progress).await.unwrap();
    assert_ne!(p, unsigned, "the unsigned publication of v1 is not reused");
    ops::ensure_distribution(&c, t, &name, &name, Some(&p), &json!({}), None, &no_progress).await.unwrap();
    let base = dist_url(&c, &dist).await;
    assert_eq!(served(&c, &format!("{base}repodata/repomd.xml.asc")).await.0, 200);
    let pv: Value = c.get(&p, &[]).await.unwrap();
    let msg = ops::verify_served(&c, t, &base, &v1, &pv).await.unwrap();
    assert!(msg.starts_with("repomd.xml, repomd.xml.asc and ") && msg.contains("matching digest"), "{msg}");
    cleanup(&c, &[&dist, &repo, &remote]).await;
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
    ops::upload(&c, t, &fixtures().join("files/one.txt"), Some("a/one.txt"), Some(&local), true, &|_, _| {}, &no_progress).await.unwrap();
    let progress = std::sync::Mutex::new(vec![]);
    let up = ops::upload(
        &c,
        t,
        &fixtures().join("files/big.bin"),
        Some("big.bin"),
        Some(&local),
        true,
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
    let pub1 = ops::ensure_publication(&c, t, &v1, &json!({}), &no_progress).await.unwrap();
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
    let pub2 = ops::ensure_publication(&c, t, &v2, &json!({}), &no_progress).await.unwrap();
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
    let published = ops::create_publication(&c, t, &latest(&c, &custom).await, &json!({}), &no_progress).await.unwrap();
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
        ops::upload(&c, t, &path, Some(&format!("{i}.txt")), Some(&repo), true, &|_, _| {}, &no_progress).await.unwrap();
        let version = latest(&c, &repo).await;
        ops::ensure_publication(&c, t, &version, &json!({}), &no_progress).await.unwrap();
        v.push(version);
    }
    let pub2 = ops::ensure_publication(&c, t, &v[1], &json!({}), &no_progress).await.unwrap();
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
    ops::upload(&c, t, &path, Some("reports/q1,2024.txt"), Some(&repo), true, &|_, _| {}, &no_progress).await.unwrap();
    let version = latest(&c, &repo).await;
    let publication = ops::ensure_publication(&c, t, &version, &json!({}), &no_progress).await.unwrap();
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
        ops::upload(&c, t, &path, Some(&format!("{i}.txt")), Some(&repo), true, &|_, _| {}, &no_progress).await.unwrap();
        v.push(latest(&c, &repo).await);
    }
    let follow = json!({ "name": name, "base_path": name, "repository": repo });
    let follow = create(&c, t.distributions(), follow, "/distributions/").await;
    let base = dist_url(&c, &follow).await;
    let p2: Value = c.get(&ops::create_publication(&c, t, &v[1], &json!({}), &no_progress).await.unwrap(), &[]).await.unwrap();
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
    let p3: Value = c.get(&ops::create_publication(&c, t, &v3, &json!({}), &no_progress).await.unwrap(), &[]).await.unwrap();
    assert_eq!(ops::verify_served(&c, t, &base, &v3, &p3).await.unwrap(), "PULP_MANIFEST matches 1 files");

    let m = ops::restore_version(&c, &repo, &v[0]).await.unwrap();
    let task = ops::settle(&c, m, &no_progress).await.unwrap().unwrap();
    assert_eq!((task.created_matching("/versions/"), latest(&c, &repo).await), (None, v3), "same content: no new version");
    cleanup(&c, &[&follow, &repo]).await;
}

/// A ULN remote through the TUI (Remotes, `t` to RPM ULN): create, edit, delete. Pulp keeps the account
/// (write-only): an edit that does not type it keeps it. A ULN sync needs a ULN account: not tested.
#[tokio::test]
async fn uln_remote_create_edit_and_delete_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, name) = (client(), unique("it-uln"));
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Remotes(RepoType::Rpm, true);
    assert_eq!(app.list.kind.unsupported(&app.conn.as_ref().unwrap().caps), None);
    run(&mut app, Act::Create, Value::Null);
    let url = "uln://ol8_x86_64_baseos_latest";
    fill(&mut app, &[("name", &name), ("url", url), ("policy", "on_demand"), ("username", "uln-user"), ("password", "uln-pass")]);
    settle(&mut app, &mut rx).await;
    let found: Option<Value> = ops::find_by_name(&c, &c.api("remotes/rpm/uln/"), &name).await.unwrap();
    let r = found.unwrap_or_else(|| panic!("not created: {:?}", texts(&app)));
    let href = r["pulp_href"].as_str().unwrap().to_string();
    let set = |r: &Value| {
        let hidden = r["hidden_fields"].as_array().unwrap();
        ["username", "password"].map(|k| hidden.iter().any(|h| h["name"] == k && h["is_set"] == true))
    };
    assert_eq!((r["url"].as_str(), r["policy"].as_str(), set(&r)), (Some(url), Some("on_demand"), [true, true]));
    assert!(r.get("password").is_none() && r.get("username").is_none(), "write-only: {r}");
    let repo = create(&c, RepoType::Rpm.repositories(), json!({ "name": name, "remote": href }), "/repositories/").await;

    run(&mut app, Act::Edit, r);
    fill(&mut app, &[("uln_server_base_url", "https://linux-update.oracle.com/")]);
    settle(&mut app, &mut rx).await;
    settle_submitted(&c, &app).await;
    let r: Value = c.get(&href, &[]).await.unwrap();
    assert_eq!((r["uln_server_base_url"].as_str(), set(&r)), (Some("https://linux-update.oracle.com/"), [true, true]));

    run(&mut app, Act::Delete, json!({ "pulp_href": href, "name": name }));
    settle(&mut app, &mut rx).await;
    let Some(Modal::Confirm { lines, typed, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm: {:?}", texts(&app)) };
    assert_eq!(typed.as_deref(), Some(name.as_str()));
    assert!(lines.contains(&format!("1 repository(ies) use it as default remote: {name}")), "{lines:?}");
    yes(&mut app);
    settle(&mut app, &mut rx).await;
    settle_submitted(&c, &app).await;
    assert!(c.get::<Value>(&href, &[]).await.is_err(), "deleted");
    cleanup(&c, &[&repo]).await;
}

/// A File Git remote through the TUI (Remotes, `t` to File Git), and a File repository whose default remote it is,
/// made outside the TUI: the sync form shows the Git remote and, untouched, syncs from it (audit F09). The
/// fixture is a bare repository for the dumb HTTP protocol: pulp_file's shallow clone fails there and its full
/// clone works. `git_ref` v1 has one file; after an edit to `main`, the next sync adds the second.
#[tokio::test]
async fn file_git_remote_syncs_a_repository_with_the_prefilled_form() {
    if !live_enabled() {
        return;
    }
    let (c, t, name) = (client(), RepoType::File, unique("it-git"));
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Remotes(t, true);
    if let Some(e) = app.list.kind.unsupported(&app.conn.as_ref().unwrap().caps) {
        assert_eq!(e, "not supported by this server: File Git remotes (remotes/file/git/)");
        eprintln!("skipping: {e}");
        return;
    }
    run(&mut app, Act::Create, Value::Null);
    fill(&mut app, &[("name", &name), ("url", &format!("{}/git/files.git", upstream())), ("git_ref", "v1")]);
    settle(&mut app, &mut rx).await;
    let found: Option<Value> = ops::find_by_name(&c, &c.api("remotes/file/git/"), &name).await.unwrap();
    let remote = found.unwrap_or_else(|| panic!("not created: {:?}", texts(&app)));
    let href = remote["pulp_href"].as_str().unwrap().to_string();
    assert_eq!(remote["git_ref"], "v1");
    let repo = create(&c, t.repositories(), json!({ "name": name, "remote": href }), "/repositories/").await;

    let sync = async |app: &mut pulp_tui::tui::app::App, rx: &mut tokio::sync::mpsc::UnboundedReceiver<pulp_tui::tui::app::Msg>| {
        app.list.kind = Kind::Repos(t, false);
        run(app, Act::Sync, c.get::<Value>(&repo, &[]).await.unwrap());
        settle(app, rx).await;
        let Some(Modal::Form { form, .. }) = &app.modal else { panic!("no sync form: {:?}", texts(app)) };
        assert_eq!(form.get("remote"), name, "the default remote, by name");
        fill(app, &[]);
        settle(app, rx).await;
        settle_submitted(&c, app).await;
        let v = latest(&c, &repo).await;
        file::files_in(&c, &v).await.unwrap().into_iter().map(|f| f.relative_path).collect::<Vec<_>>()
    };
    assert_eq!(sync(&mut app, &mut rx).await, ["docs/readme.txt"]);

    app.list.kind = Kind::Remotes(t, true);
    run(&mut app, Act::Edit, remote);
    fill(&mut app, &[("git_ref", "main")]);
    settle(&mut app, &mut rx).await;
    settle_submitted(&c, &app).await;
    let mut paths = sync(&mut app, &mut rx).await;
    paths.sort();
    assert_eq!(paths, ["docs/guide.txt", "docs/readme.txt"]);
    cleanup(&c, &[&repo, &href]).await;
}

/// Sync (s) with the mirror policy from a File Git remote through the TUI (audit T04, F63): the name is typed into
/// the Remote field without loading the picker options (as when they fail to load). The confirm says that the task
/// fails (pulp_file has no manifest of a Git remote); Esc sends nothing.
#[tokio::test]
async fn file_mirror_sync_from_a_typed_git_remote_name_warns_that_the_task_fails_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, t, name) = (client(), RepoType::File, unique("it-gitmirror"));
    let (mut app, mut rx) = app().await;
    if let Some(e) = Kind::Remotes(t, true).unsupported(&app.conn.as_ref().unwrap().caps) {
        eprintln!("skipping: {e}");
        return;
    }
    let remote =
        create(&c, "remotes/file/git/", json!({ "name": name, "url": format!("{}/git/files.git", upstream()) }), "/remotes/").await;
    let repo = create(&c, t.repositories(), json!({ "name": name }), "/repositories/").await;
    app.list.kind = Kind::Repos(t, false);
    run(&mut app, Act::Sync, c.get::<Value>(&repo, &[]).await.unwrap());
    fill(&mut app, &[("remote", &name), ("policy", "mirror")]);
    settle(&mut app, &mut rx).await; // the remote by name, and the retention of the repository
    let Some(Modal::Confirm { lines, .. }) = app.modal.take() else { panic!("no confirm: {:?}", texts(&app)) };
    let fails = format!(
        "File: a mirror sync of {name} from a Git remote makes the new version, then its task fails \
         (pulp_file has no manifest to publish for a Git remote). Use additive."
    );
    assert!(lines.contains(&fails), "{lines:?}");
    assert!(!lines.iter().any(|l| l.contains("upstream manifest")), "{lines:?}");
    assert!(latest(&c, &repo).await.ends_with("/versions/0/"), "Esc: no sync");
    cleanup(&c, &[&repo, &remote]).await;
}

/// Prune through the TUI on a repository synced from `rpm2` (fixture-alpha 1.0 and 1.1): with keep_days 0 the
/// dry run reports the older alpha and changes nothing; after the typed name, the new version lacks it (on a server
/// whose new version still lists it, the result says so).
#[tokio::test]
async fn rpm_prune_dry_run_then_prune_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, name) = (client(), unique("it-prune"));
    let (repo, remote) = synced_repo(&c, RepoType::Rpm, &name, "rpm2/", "additive").await;
    let v1 = latest(&c, &repo).await;
    let nevras = async |v: &str| -> Vec<String> {
        let mut n: Vec<String> = rpm::packages_in(&c, v).await.unwrap().iter().map(Package::nevra).collect();
        n.sort();
        n
    };
    assert!(nevras(&v1).await.contains(&"fixture-alpha-1.0-1.noarch".to_string()));
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Repos(RepoType::Rpm, false);
    run(&mut app, Act::PrunePackages, json!({ "pulp_href": repo, "name": name }));
    fill(&mut app, &[("keep_days", "0")]);
    settle(&mut app, &mut rx).await;
    let Some(Modal::Confirm { lines, typed, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm: {:?}", texts(&app)) };
    assert_eq!(typed.as_deref(), Some(name.as_str()));
    assert!(lines.contains(&format!("  {name}: 1 package(s)")), "{lines:?}");
    assert_eq!(latest(&c, &repo).await, v1, "the dry run made no version");
    yes(&mut app);
    settle(&mut app, &mut rx).await;
    let Some(Modal::Text { lines, .. }) = app.modal.take() else { panic!("no result: {:?}", texts(&app)) };
    let v2 = latest(&c, &repo).await;
    assert!(v2.ends_with("/versions/2/"), "{v2}");
    let left = nevras(&v2).await;
    if left.contains(&"fixture-alpha-1.0-1.noarch".to_string()) {
        // pulp_rpm 3.34.0 (pulpcore 3.95.3): the task marks the package removed, but version 2 still lists it.
        let bug = "new version 2 still has 1 of the 1 package(s) that Pulp marked removed \
            (pulp_rpm bug, fixed in 3.35.0 and in 3.32.15, 3.29.13, 3.27.14): upgrade pulp_rpm";
        assert_eq!(lines, [format!("{name}: {bug}")]);
        return cleanup(&c, &[&repo, &remote]).await;
    }
    assert_eq!(lines, [format!("{name}: removed 1 package(s): new version 2")]);
    assert!(
        left.contains(&"fixture-alpha-1.1-1.noarch".to_string()) && !left.iter().any(|n| n.starts_with("fixture-alpha-1.0")),
        "{left:?}"
    );
    assert_eq!(nevras(&v1).await.len(), left.len() + 1, "version 1 keeps it");
    cleanup(&c, &[&repo, &remote]).await;
}

/// Re-point (b on Distributions) through the TUI: a version number that the repository does not have is refused
/// with its latest number; a version of another repository than the served one gets a WARNING line in the preview.
#[tokio::test]
async fn re_point_checks_the_version_and_warns_about_another_repository() {
    if !live_enabled() {
        return;
    }
    let (c, t, name) = (client(), RepoType::Rpm, unique("it-repoint"));
    let (a, b) = (format!("{name}-a"), format!("{name}-b"));
    let ra = create(&c, t.repositories(), json!({ "name": a }), "/repositories/").await;
    let rb = create(&c, t.repositories(), json!({ "name": b }), "/repositories/").await;
    let p = ops::ensure_publication(&c, t, &format!("{ra}versions/0/"), &json!({}), &no_progress).await.unwrap();
    let dist = ops::ensure_distribution(&c, t, &name, &name, Some(&p), &json!({}), None, &no_progress).await.unwrap();
    let item: Value = c.get(&dist, &[]).await.unwrap();
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Distributions(t);
    run(&mut app, Act::Rollback, item.clone());
    fill(&mut app, &[("repo", b.as_str()), ("number", "5")]);
    settle(&mut app, &mut rx).await;
    assert!(app.modal.is_none(), "no preview");
    assert_eq!(texts(&app).last().unwrap(), &format!("resolve version: repository {b} has no version 5 (latest: 0)"));
    run(&mut app, Act::Rollback, item);
    fill(&mut app, &[("repo", b.as_str()), ("number", "0")]);
    settle(&mut app, &mut rx).await;
    let Some(Modal::Confirm { lines, .. }) = app.modal.take() else { panic!("no preview: {:?}", texts(&app)) };
    assert_eq!(lines[3], format!("  WARNING: version 0 of repository {b}, but the distribution serves repository {a} now"));
    cleanup(&c, &[&dist, &p, &ra, &rb]).await;
}

/// Distribute from Repository Versions (all repositories), whose rows have only hrefs: the form offers the
/// repository name and a base path made safe from it, not a fragment of the repository id.
#[tokio::test]
async fn distribute_from_all_versions_offers_the_repository_name() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let name = format!("{} x", unique("it-dname"));
    let repo = create(&c, RepoType::Rpm.repositories(), json!({ "name": name }), "/repositories/").await;
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Versions(None);
    run(&mut app, Act::Distribute, json!({ "pulp_href": format!("{repo}versions/0/"), "number": 0 }));
    settle(&mut app, &mut rx).await;
    let Some(Modal::Form { form, .. }) = app.modal.take() else { panic!("no form: {:?}", texts(&app)) };
    let value = |k: &str| form.fields.iter().find(|f| f.key == k).unwrap().value.clone();
    assert_eq!([value("dist"), value("base_path")], [name.clone(), name.replace(' ', "-")]);
    cleanup(&c, &[&repo]).await;
}

/// Copy with filter rules through the TUI from the rpm fixture (alpha, beta, beta-debuginfo, an advisory of beta):
/// the advisory comes along only when its package is chosen.
#[tokio::test]
async fn rpm_filtered_copy_through_the_tui_takes_the_advisory_of_the_chosen_packages() {
    if !live_enabled() {
        return;
    }
    let (c, name) = (client(), unique("it-fcopy"));
    let (src, remote) = synced_repo(&c, RepoType::Rpm, &name, "rpm/", "additive").await;
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Repos(RepoType::Rpm, false);
    let mut made = vec![];
    for (rule, want, advisory) in [
        ("exclude name=*-debuginfo", ["fixture-alpha-1.0-1.noarch", "fixture-beta-2.0-1.noarch"], true),
        ("exclude name=fixture-beta", ["fixture-alpha-1.0-1.noarch", "fixture-beta-debuginfo-2.0-1.noarch"], false),
    ] {
        let dst_name = format!("{name}-dst{}", made.len());
        let dst = create(&c, RepoType::Rpm.repositories(), json!({ "name": dst_name }), "/repositories/").await;
        run(&mut app, Act::CopyContent, json!({ "pulp_href": dst, "name": dst_name }));
        fill(&mut app, &[("source", name.as_str()), ("filters", rule), ("deps", "false")]);
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm: {:?}", texts(&app)) };
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        settle_submitted(&c, &app).await;
        let v = latest(&c, &dst).await;
        let mut got: Vec<String> = rpm::packages_in(&c, &v).await.unwrap().iter().map(Package::nevra).collect();
        got.sort();
        assert_eq!(got, want, "{rule}");
        let q = [("repository_version", v.clone()), ("fields", "id".to_string())];
        let ids: Vec<Value> = c.list_all(&c.api("content/rpm/advisories/"), &q, 100).await.unwrap();
        assert_eq!(ids.iter().any(|a| a["id"] == "FIXTURE-2026:0001"), advisory, "{rule}: {ids:?}");
        made.push(dst);
    }
    cleanup(&c, &[&made[0], &made[1], &src, &remote]).await;
}

/// Scan through the TUI. Pulp refuses a repository without `osv_config`; with Rocky Linux 9 the task queries
/// osv.dev once per fixture package (the server needs internet access) and finds no vulnerabilities.
#[tokio::test]
async fn rpm_version_scan_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, name) = (client(), unique("it-scan"));
    let (repo, remote) = synced_repo(&c, RepoType::Rpm, &name, "rpm/", "additive").await;
    let v1 = latest(&c, &repo).await;
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Versions(Some((repo.clone(), name.clone())));
    let item = json!({ "pulp_href": v1, "number": 1 });
    run(&mut app, Act::Scan, item.clone());
    settle(&mut app, &mut rx).await;
    if !app.conn.as_ref().unwrap().caps.has_operation(rpm::SCAN, "post") {
        // An older pulp_rpm: the TUI refuses before any request.
        assert_eq!(texts(&app).last().unwrap(), &format!("not supported by this server: vulnerability scan ({})", rpm::SCAN));
        return cleanup(&c, &[&repo, &remote]).await;
    }
    assert!(texts(&app).last().unwrap().contains("osv.rpm.config"), "{:?}", texts(&app));
    let m = c.patch(&repo, &json!({ "osv_config": [{ "name": "Rocky Linux", "releases": ["9"] }] })).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    run(&mut app, Act::Scan, item);
    settle(&mut app, &mut rx).await;
    let Some(Modal::Text { title, lines, .. }) = app.modal.take() else { panic!("no result: {:?}", texts(&app)) };
    assert_eq!(title, "Vulnerability scan of version 1");
    // A report belongs to a package, which other tests share: a rerun updates it instead of making one.
    assert!(lines[0].starts_with("3 package queries sent to osv.dev;"), "{lines:?}");
    assert_eq!(lines[1..], ["0 of 3 packages have known vulnerabilities."]);
    cleanup(&c, &[&repo, &remote]).await;
}

/// The OSV config through the TUI (audit F28). pulp_rpm keeps it in the label `osv.rpm.config`, which the Labels
/// field does not show: a label edit that removes a label keeps the config, an emptied OSV config field removes it.
#[tokio::test]
async fn rpm_osv_config_survives_a_label_edit_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, name) = (client(), unique("it-osv"));
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Repos(RepoType::Rpm, false);
    run(&mut app, Act::Create, Value::Null);
    if !app.conn.as_ref().unwrap().caps.has_field(RepoType::Rpm.repository_schema(), "osv_config") {
        // An older pulp_rpm: the form has no OSV config field.
        let Some(Modal::Form { form, .. }) = app.modal.take() else { panic!("no form: {:?}", texts(&app)) };
        assert!(!form.fields.iter().any(|f| f.key == "osv_config"), "{:?}", form.fields.iter().map(|f| &f.key).collect::<Vec<_>>());
        return;
    }
    let rocky = json!([{ "name": "Rocky Linux", "releases": ["9"] }]);
    fill(&mut app, &[("name", &name), ("labels", "a=1,b=2"), ("osv_config", &rocky.to_string())]);
    settle(&mut app, &mut rx).await;
    let found: Option<Value> = ops::find_by_name(&c, &c.api(RepoType::Rpm.repositories()), &name).await.unwrap();
    let r = found.unwrap_or_else(|| panic!("not created: {:?}", texts(&app)));
    let href = r["pulp_href"].as_str().unwrap().to_string();
    assert_eq!(r["osv_config"], rocky);
    assert!(r["pulp_labels"].get("osv.rpm.config").is_some(), "{r}");

    run(&mut app, Act::Edit, r);
    let Some(Modal::Form { form, .. }) = &app.modal else { panic!("no form: {:?}", texts(&app)) };
    assert_eq!(form.get("labels"), "a=1,b=2");
    fill(&mut app, &[("labels", "a=1")]);
    settle(&mut app, &mut rx).await;
    settle_submitted(&c, &app).await;
    let r: Value = c.get(&href, &[]).await.unwrap();
    assert_eq!((&r["osv_config"], r["pulp_labels"].get("b")), (&rocky, None), "{r}");

    run(&mut app, Act::Edit, r);
    fill(&mut app, &[("osv_config", "")]);
    settle(&mut app, &mut rx).await;
    settle_submitted(&c, &app).await;
    let r: Value = c.get(&href, &[]).await.unwrap();
    assert_eq!((&r["osv_config"], &r["pulp_labels"]), (&Value::Null, &json!({ "a": "1" })));
    cleanup(&c, &[&href]).await;
}

/// Upload with Overwrite off (audit F28): other content with the same relative path fails the task and the
/// repository keeps its file; the default (on) replaces it in a new version.
#[tokio::test]
async fn file_upload_without_overwrite_keeps_the_file_of_the_same_path() {
    if !live_enabled() {
        return;
    }
    let (c, t, name) = (client(), RepoType::File, unique("it-overwrite"));
    if !Capabilities::detect(&c).await.unwrap().has_field("file.FileContent", "overwrite") {
        // The upload form hides the option on such a server (an older pulpcore answers 400 "Unexpected field").
        eprintln!("skipping: this server's File uploads have no `overwrite` field (older pulpcore)");
        return;
    }
    let repo = create(&c, t.repositories(), json!({ "name": name }), "/repositories/").await;
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = (dir.path().join("old.txt"), dir.path().join("new.txt"));
    // Unique bytes, so a rerun does not find the content units of an earlier run.
    std::fs::write(&old, format!("{name} old\n")).unwrap();
    std::fs::write(&new, format!("{name} new\n")).unwrap();
    let up = |path, overwrite| ops::upload(&c, t, path, Some("same.txt"), Some(&repo), overwrite, &|_, _| {}, &no_progress);
    let first = up(&old, false).await.unwrap().content_href;
    let e = up(&new, false).await.unwrap_err();
    assert!(e.to_string().contains("overwrite"), "{e}");
    let content = async |v: String| {
        let q = [("repository_version", v), ("fields", "pulp_href".to_string())];
        c.list::<Value>(&c.api(t.content()), &q).await.unwrap().results.iter().map(|x| x["pulp_href"].clone()).collect::<Vec<_>>()
    };
    assert_eq!(content(latest(&c, &repo).await).await, [json!(first)], "the old file stays");
    let second = up(&new, true).await.unwrap().content_href;
    assert_eq!(content(latest(&c, &repo).await).await, [json!(second)], "replaced");
    cleanup(&c, &[&repo]).await;
}

/// Edit (audit F28): an emptied Retain package versions removes the limit (pulp_rpm: 0, not null).
#[tokio::test]
async fn rpm_retain_package_versions_cleared_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, name) = (client(), unique("it-retain"));
    let repo = create(&c, RepoType::Rpm.repositories(), json!({ "name": name, "retain_package_versions": 3 }), "/repositories/").await;
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Repos(RepoType::Rpm, false);
    run(&mut app, Act::Edit, c.get(&repo, &[]).await.unwrap());
    fill(&mut app, &[("retain_package_versions", "")]);
    settle(&mut app, &mut rx).await;
    settle_submitted(&c, &app).await;
    let r: Value = c.get(&repo, &[]).await.unwrap();
    assert_eq!(r["retain_package_versions"], 0, "{:?}", texts(&app));
    cleanup(&c, &[&repo]).await;
}

/// Remote timeouts in decimal seconds through the TUI (audit F28): Pulp stores floats, and the edit form shows
/// them as stored.
#[tokio::test]
async fn remote_timeouts_keep_decimal_seconds_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, name) = (client(), unique("it-timeout"));
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Remotes(RepoType::File, false);
    run(&mut app, Act::Create, Value::Null);
    let url = format!("{}/file/PULP_MANIFEST", upstream());
    fill(&mut app, &[("name", &name), ("url", &url), ("connect_timeout", "2.5"), ("sock_read_timeout", "0.1")]);
    settle(&mut app, &mut rx).await;
    let found: Option<Value> = ops::find_by_name(&c, &c.api(RepoType::File.remotes()), &name).await.unwrap();
    let r = found.unwrap_or_else(|| panic!("not created: {:?}", texts(&app)));
    assert_eq!((&r["connect_timeout"], &r["sock_read_timeout"]), (&json!(2.5), &json!(0.1)));
    run(&mut app, Act::Edit, r.clone());
    let Some(Modal::Form { form, .. }) = &app.modal else { panic!("no form: {:?}", texts(&app)) };
    assert_eq!((form.get("connect_timeout"), form.get("sock_read_timeout")), ("2.5", "0.1"), "as stored");
    app.modal = None;
    cleanup(&c, &[r["pulp_href"].as_str().unwrap()]).await;
}

/// A Content View snapshot repository of another database (labels set with the API) is not deleted from the
/// Repositories screen, alone or marked (audit F02): the refusal names the view and the database, and says to use the
/// Content View actions of that database (audit F51, T04); nothing is sent.
#[tokio::test]
async fn snapshot_repository_of_another_database_is_not_deleted_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, name) = (client(), unique("it-snap"));
    let labels = json!({ "pulp_tui_cv": "foreignview", "pulp_tui_db": "another-database-id" });
    let repo = create(&c, RepoType::File.repositories(), json!({ "name": name, "pulp_labels": labels }), "/repositories/").await;
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Repos(RepoType::File, false);
    let item: Value = c.get(&repo, &[]).await.unwrap();
    let refused = format!(
        "refused: {name} is a snapshot repository of Content View foreignview (database another-database-id); use the Content View actions of that database"
    );
    run(&mut app, Act::Delete, item.clone());
    settle(&mut app, &mut rx).await;
    assert!(app.modal.is_none() && texts(&app).last().unwrap().starts_with(&refused), "{:?}", texts(&app));
    app.list.marked = [repo.clone()].into();
    run(&mut app, Act::Delete, item);
    settle(&mut app, &mut rx).await;
    assert!(app.modal.is_none() && texts(&app).last().unwrap().contains(&refused), "{:?}", texts(&app));
    assert!(c.get::<Value>(&repo, &[]).await.is_ok(), "the repository stays");
    cleanup(&c, &[&repo]).await;
}

/// The versions and publications of a Content View snapshot repository of another database are not deleted from the
/// Versions screens (one repository, all versions) or the Publications screen (audit F25), and its content is not
/// removed on the Content screen (audit F34): each is refused before any dialog, nothing is sent, and the versions and
/// the publication stay. The refusal ends "use the Content View actions of that database" (audit F51, T04).
#[tokio::test]
async fn snapshot_versions_publications_and_content_of_another_database_are_not_changed_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, t, name) = (client(), RepoType::File, unique("it-snapver"));
    let (repo, remote, [v1, v2]) = two_versions(&c, t, &name, "file/PULP_MANIFEST").await;
    let labels = json!({ "pulp_tui_cv": "foreignview", "pulp_tui_db": "another-database-id" });
    ops::settle(&c, c.patch(&repo, &json!({ "pulp_labels": labels })).await.unwrap(), &no_progress).await.unwrap();
    let publication = ops::create_publication(&c, t, &v1, &json!({}), &no_progress).await.unwrap();
    let mark = pulp_tui::session_log::last_id();
    let (mut app, mut rx) = app().await;
    let refused = format!(
        "refused: {name} is a snapshot repository of Content View foreignview (database another-database-id); use the Content View actions of that database"
    );
    let p: Value = c.get(&publication, &[]).await.unwrap();
    for (kind, item) in [
        (Kind::Versions(Some((repo.clone(), name.clone()))), c.get::<Value>(&v1, &[]).await.unwrap()),
        (Kind::Versions(None), c.get::<Value>(&v2, &[]).await.unwrap()),
        (Kind::Publications(t), p),
    ] {
        app.list.kind = kind.clone();
        run(&mut app, Act::Delete, item);
        settle(&mut app, &mut rx).await;
        assert!(app.modal.is_none() && texts(&app).last().unwrap().contains(&refused), "{kind:?}: {:?}", texts(&app));
    }
    // Remove (x) of a unit on the Content screen of the latest version.
    open_list(&mut app, &mut rx, Kind::Content { version: v2.clone(), ctype: 0 }, "").await;
    let unit = app.list.items.first().cloned().unwrap_or_else(|| panic!("no content: {:?}", texts(&app)));
    run(&mut app, Act::RemoveContent, unit);
    settle(&mut app, &mut rx).await;
    assert!(app.modal.is_none() && texts(&app).last().unwrap().contains(&refused), "Remove: {:?}", texts(&app));
    assert_eq!(latest(&c, &repo).await, v2, "no new version");
    // The session log (Logs screen): since the TUI connected, every request was a read.
    let rows = pulp_tui::session_log::rows(true);
    let sent: Vec<&Value> = rows.iter().filter(|l| l["id"].as_u64() > Some(mark) && l["kind"] == "request").collect();
    assert!(!sent.is_empty() && sent.iter().all(|l| s(l, "summary").starts_with("GET ")), "only reads: {sent:?}");
    for href in [&v1, &v2, &publication] {
        assert!(c.get::<Value>(href, &[]).await.is_ok(), "{href} stays");
    }
    cleanup(&c, &[&publication, &repo, &remote]).await;
}

/// A repository of type `t` synced from `path` (version 1), and version 2 without one unit of version 1.
async fn two_versions(c: &PulpClient, t: RepoType, name: &str, path: &str) -> (String, String, [String; 2]) {
    let (repo, remote) = synced_repo(c, t, name, path, "additive").await;
    let v1 = latest(c, &repo).await;
    let q = [("repository_version", v1.clone()), ("limit", "1".to_string())];
    let unit = c.list::<Value>(&c.api(t.content()), &q).await.unwrap().results[0]["pulp_href"].clone();
    let m = c.post(&format!("{repo}modify/"), &json!({ "remove_content_units": [unit] })).await.unwrap();
    ops::settle(c, m, &no_progress).await.unwrap();
    let v2 = latest(c, &repo).await;
    assert!(v2.ends_with("/versions/2/"), "{v2}");
    (repo, remote, [v1, v2])
}

/// Verify (V) on distribution `row`: the last line of the result.
async fn tui_verify(app: &mut App, rx: &mut UnboundedReceiver<Msg>, row: Value) -> String {
    run(app, Act::Verify, row);
    settle(app, rx).await;
    let Some(Modal::Text { lines, .. }) = app.modal.take() else { panic!("no verify result: {:?}", texts(app)) };
    lines.last().cloned().unwrap_or_default()
}

/// The Publications and Distributions screens of type `t` through the TUI, on a repository with versions 1 and 2
/// (`path`: its fixture; `file`: a file that each publication serves):
/// * Publications Create makes a publication of version 1;
/// * Distributions Create pins one distribution to it (the form reuses it) and binds another to the repository;
/// * Edit of the content guard (set, change to another guard, clear) and Checkpoint (on, off) of the second one, each
///   after the typed name;
/// * Edit of the base path, then of the binding (version 2), each after the typed name; the old URL gets 404;
/// * Rollback (b) back to version 1: the preview names the existing publication, the job re-points and verifies;
/// * Publications Delete of the version 2 publication names the distribution that serves it and what it serves after;
/// * Distributions Delete: the Task Center tracks each task to its end and the list reloads without the row.
async fn publications_and_distributions_through_the_tui(t: RepoType, path: &str, file: &str) {
    let (c, name) = (client(), unique(&format!("it-dist-{}", t.component())));
    let (repo, remote, [v1, v2]) = two_versions(&c, t, &name, path).await;
    let (pinned, follow) = (format!("{name}-pin"), format!("{name}-follow"));
    let (mut app, mut rx) = app().await;

    open_list(&mut app, &mut rx, Kind::Publications(t), "").await;
    run(&mut app, Act::Create, Value::Null);
    fill(&mut app, &[("repo", &name), ("number", "1")]);
    settle(&mut app, &mut rx).await;
    let pubs = async |v: &str| -> Vec<String> {
        let all: Vec<Value> = c.list_all(&c.api(t.publications()), &[("repository_version", v.to_string())], 100).await.unwrap();
        all.iter().map(|p| p["pulp_href"].as_str().unwrap().to_string()).collect()
    };
    let p1 = pubs(&v1).await.pop().unwrap_or_else(|| panic!("no publication: {:?}", texts(&app)));
    assert!(texts(&app).contains(&format!("publication {} of {}", short(&p1), short(&v1))), "{:?}", texts(&app));
    assert!(row(&app, "pulp_href", &p1).is_some(), "the list reloads with it");

    // Create: pinned to version 1, and following the repository.
    open_list(&mut app, &mut rx, Kind::Distributions(t), &name).await;
    for (dist, mode) in [(&pinned, "publication of a repository version"), (&follow, "follow repository latest")] {
        run(&mut app, Act::Create, Value::Null);
        fill(&mut app, &[("name", dist), ("base_path", dist), ("mode", mode), ("repo", &name), ("number", "1")]);
        settle(&mut app, &mut rx).await;
        assert!(texts(&app).contains(&format!("create distribution {dist}: done")), "{:?}", texts(&app));
    }
    let d = row(&app, "name", &pinned).unwrap_or_else(|| panic!("the list reloads with it: {:?}", app.list.items));
    assert_eq!((d["publication"].as_str(), &d["repository"]), (Some(p1.as_str()), &Value::Null), "pinned to the reused publication");
    let f = row(&app, "name", &follow).unwrap_or_else(|| panic!("the list reloads with it: {:?}", app.list.items));
    assert_eq!((f["repository"].as_str(), &f["publication"]), (Some(repo.as_str()), &Value::Null));
    let cols = Kind::Distributions(t).columns();
    assert_eq!(((cols[2].get)(&d), (cols[2].get)(&f)), ("pinned".into(), "latest".into()));
    assert!(tui_verify(&mut app, &mut rx, d.clone()).await.starts_with("OK: "), "the pinned distribution serves version 1");

    // Edit the content guard (set, change A -> B, clear) and Checkpoint (audit F26): each after the typed name, with
    // the now / after line; a guard by its name (read, or from the picker options).
    let (a, b) = (format!("{name}-guard"), format!("{name}-guard-b"));
    let guard = create(&c, "contentguards/core/rbac/", json!({ "name": a }), "/contentguards/").await;
    let guard_b = create(&c, "contentguards/core/rbac/", json!({ "name": b }), "/contentguards/").await;
    for ((key, value), want, stored) in [
        (("content_guard", guard.as_str()), format!("  content guard: none -> {a}"), json!(guard)),
        (("content_guard", guard_b.as_str()), format!("  content guard: {a} -> {b}"), json!(guard_b)),
        (("content_guard", ""), format!("  content guard: {b} -> none"), Value::Null),
        (("checkpoint", "true"), "  checkpoint:  off -> on".into(), json!(true)),
        (("checkpoint", "false"), "  checkpoint:  on -> off".into(), json!(false)),
    ] {
        let f = row(&app, "name", &follow).unwrap();
        run(&mut app, Act::Edit, f);
        if !form_has(&app, key) {
            eprintln!("skipping {key}: this server's distributions have no `{key}` field (pulpcore 3.63)");
            app.modal = None;
            continue;
        }
        if value == guard_b {
            load_choices(&mut app, &mut rx, key).await; // the operator picks guard B
        }
        fill(&mut app, &[(key, value)]);
        settle(&mut app, &mut rx).await;
        let (lines, typed) = yes(&mut app);
        assert_eq!(typed.as_deref(), Some(follow.as_str()));
        assert!(lines.contains(&want), "{lines:?}");
        settle(&mut app, &mut rx).await;
        assert_eq!(row(&app, "name", &follow).unwrap()[key], stored, "the list reloads with it: {:?}", texts(&app));
    }

    // Edit the base path: the typed name, then the new path serves and the old one is gone.
    let moved = format!("{pinned}-moved");
    run(&mut app, Act::Edit, d.clone());
    fill(&mut app, &[("base_path", &moved)]);
    settle(&mut app, &mut rx).await;
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed.as_deref(), Some(pinned.as_str()));
    assert!(lines.contains(&format!("  base path:   {pinned} -> {moved}")), "{lines:?}");
    settle(&mut app, &mut rx).await;
    assert!(texts(&app).contains(&format!("update distribution {pinned}: done")), "{:?}", texts(&app));
    let d = row(&app, "name", &pinned).unwrap();
    assert_eq!(d["base_path"], moved.as_str(), "the list reloads with the new path");
    let url = s(&d, "base_url");
    assert_eq!(served(&c, &format!("{}{file}", url.replace(&moved, &pinned))).await.0, 404, "the old URL");
    assert_eq!(served(&c, &format!("{url}{file}")).await.0, 200, "the new URL");

    // Edit the binding: version 2, after the typed name.
    run(&mut app, Act::Edit, d);
    fill(&mut app, &[("repo", &name), ("number", "2")]);
    settle(&mut app, &mut rx).await;
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed.as_deref(), Some(pinned.as_str()));
    assert!(lines.contains(&format!("  will serve:  a publication of version 2 of repository {name} (pinned)")), "{lines:?}");
    settle(&mut app, &mut rx).await;
    let p2 = pubs(&v2).await.pop().unwrap_or_else(|| panic!("no publication of version 2: {:?}", texts(&app)));
    let d = row(&app, "name", &pinned).unwrap();
    assert_eq!(d["publication"].as_str(), Some(p2.as_str()), "{:?}", texts(&app));
    assert!(tui_verify(&mut app, &mut rx, d.clone()).await.starts_with("OK: "), "the pinned distribution serves version 2");

    // Rollback (b) to version 1: the preview, then the job re-points and verifies what is served.
    run(&mut app, Act::Rollback, d);
    fill(&mut app, &[("repo", &name), ("number", "1")]);
    settle(&mut app, &mut rx).await;
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed, None, "a plain confirmation on a profile that is not production");
    assert!(lines[1].starts_with(&format!("  now serves:  {} (", short(&v2))), "{lines:?}");
    assert_eq!(lines[2], format!("  will serve:  {} (pinned)", short(&v1)));
    assert!(lines[3].starts_with(&format!("  publication: existing {} (", short(&p1))), "{lines:?}");
    settle(&mut app, &mut rx).await;
    let done = texts(&app).into_iter().find(|x| x.starts_with(&format!("{pinned} now serves {}: ", short(&v1))));
    assert!(done.is_some(), "re-pointed and verified: {:?}", texts(&app));
    assert_eq!(row(&app, "name", &pinned).unwrap()["publication"].as_str(), Some(p1.as_str()), "the list reloads");

    // Delete the version 2 publication: the distribution that follows the repository then serves version 1.
    open_list(&mut app, &mut rx, Kind::Publications(t), "").await;
    let p = row(&app, "pulp_href", &p2).unwrap_or_else(|| panic!("{p2} not on the first page"));
    run(&mut app, Act::Delete, p);
    settle(&mut app, &mut rx).await;
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed, Some(short(&p2)));
    let after = format!("then serves publication {} (version 1)", short(&p1));
    assert!(lines[1] == "WARNING: 1 distribution(s) serve this publication now:" && lines[2].ends_with(&after), "{lines:?}");
    settle(&mut app, &mut rx).await;
    assert!(texts(&app).contains(&"delete publication: done".to_string()), "{:?}", texts(&app));
    assert!(row(&app, "pulp_href", &p2).is_none(), "the list reloads without it");
    assert!(c.get::<Value>(&p2, &[]).await.unwrap_err().is_not_found());
    open_list(&mut app, &mut rx, Kind::Distributions(t), &name).await;
    let f = row(&app, "name", &follow).unwrap();
    assert!(tui_verify(&mut app, &mut rx, f).await.starts_with("OK: "), "the following distribution serves version 1");

    // Delete both distributions: each a task that the Task Center follows to its end.
    for dist in [&follow, &pinned] {
        let d = row(&app, "name", dist).unwrap();
        run(&mut app, Act::Delete, d.clone());
        let (lines, typed) = yes(&mut app);
        assert_eq!((typed.as_deref(), lines[0].clone()), (Some(dist.as_str()), format!("Delete distribution {dist}.")));
        settle_tasks(&mut app, &mut rx).await;
        assert!(texts(&app).contains(&format!("task completed: delete distribution {dist}")), "{:?}", texts(&app));
        assert!(row(&app, "name", dist).is_none(), "the list reloads without it");
        assert!(c.get::<Value>(&s(&d, "pulp_href"), &[]).await.unwrap_err().is_not_found());
    }
    cleanup(&c, &[&p1, &repo, &remote, &guard, &guard_b]).await;
}

#[tokio::test]
async fn file_publications_and_distributions_through_the_tui() {
    if live_enabled() {
        publications_and_distributions_through_the_tui(RepoType::File, "file/PULP_MANIFEST", "PULP_MANIFEST").await;
    }
}

#[tokio::test]
async fn rpm_publications_and_distributions_through_the_tui() {
    if live_enabled() {
        publications_and_distributions_through_the_tui(RepoType::Rpm, "rpm/", "repodata/repomd.xml").await;
    }
}

#[tokio::test]
async fn deb_publications_and_distributions_through_the_tui() {
    if live_enabled() {
        publications_and_distributions_through_the_tui(RepoType::Deb, "deb/", "dists/fixture/Release").await;
    }
}

/// Repositories Delete through the TUI for File and RPM: one repository (the impact names the distribution that
/// follows it and the Content View that uses it as a source; Pulp then unbinds the distribution), then two marked
/// ones with one typed confirmation; the Task Center follows each task, the marks go, the list reloads without them.
#[tokio::test]
async fn repository_delete_single_and_marked_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let (mut app, mut rx) = app().await;
    let (st, scope) = (app.cv_store.clone().unwrap(), app.conn.as_ref().unwrap().scope.clone());
    for t in [RepoType::File, RepoType::Rpm] {
        let name = unique(&format!("it-repodel-{}", t.component()));
        let mut repos = vec![];
        for n in ["a", "b", "c"] {
            repos.push(create(&c, t.repositories(), json!({ "name": format!("{name}-{n}") }), "/repositories/").await);
        }
        let (a, b, cc) = (format!("{name}-a"), format!("{name}-b"), format!("{name}-c"));
        let dist = create(&c, t.distributions(), json!({ "name": a, "base_path": a, "repository": repos[0] }), "/distributions/").await;
        let cv = st.create_cv(&scope, &name, "").await.unwrap();
        st.set_cv_repo(cv, &pulp_tui::store::CvRepo { repo_href: repos[0].clone(), repo_name: a.clone(), repo_type: t, filters: vec![] })
            .await
            .unwrap();
        open_list(&mut app, &mut rx, Kind::Repos(t, false), &name).await;
        assert_eq!(app.list.items.len(), 3, "{:?}", app.list.error);

        let r = row(&app, "name", &a).unwrap();
        run(&mut app, Act::Delete, r);
        settle(&mut app, &mut rx).await; // the impact is read before the confirmation
        let (lines, typed) = yes(&mut app);
        assert_eq!(typed.as_deref(), Some(a.as_str()));
        assert_eq!(lines[0], format!("Deletes repository {a} with all 1 versions and 0 publications."));
        assert!(lines[1] == "WARNING: 1 distribution(s) serve it and stop serving:" && lines[2].contains(&a), "{lines:?}");
        assert_eq!(lines.last().unwrap(), &format!("WARNING: Content View(s) {name} use it as a source; their next publish fails."));
        settle_tasks(&mut app, &mut rx).await;
        shown(&app, &format!("task completed: delete repository {a}"));
        assert!(row(&app, "name", &a).is_none(), "the list reloads without it");
        assert!(c.get::<Value>(&repos[0], &[]).await.unwrap_err().is_not_found());
        assert_eq!(c.get::<Value>(&dist, &[]).await.unwrap()["repository"], Value::Null, "Pulp unbinds the distribution");

        app.list.marked = [repos[1].clone(), repos[2].clone()].into();
        let r = row(&app, "name", &b).unwrap();
        run(&mut app, Act::Delete, r);
        settle(&mut app, &mut rx).await;
        let (lines, typed) = yes(&mut app);
        assert_eq!(typed.as_deref(), Some("delete 2 repositories"));
        for n in [&b, &cc] {
            assert!(lines.contains(&format!("Deletes repository {n} with all 1 versions and 0 publications.")), "{lines:?}");
        }
        settle_tasks(&mut app, &mut rx).await;
        for (n, href) in [(&b, &repos[1]), (&cc, &repos[2])] {
            shown(&app, &format!("task completed: delete {n}"));
            assert!(c.get::<Value>(href, &[]).await.unwrap_err().is_not_found());
        }
        assert!(app.list.marked.is_empty() && app.list.items.is_empty(), "unmarked, and the list reloads without them");
        cleanup(&c, &[&dist]).await;
    }
}

/// Sync history (H) of a repository synced twice: the newest task first, and the version that each sync made.
#[tokio::test]
async fn sync_history_lists_the_syncs_of_a_repository() {
    if !live_enabled() {
        return;
    }
    let (c, t, name) = (client(), RepoType::File, unique("it-history"));
    let (repo, remote) = synced_repo(&c, t, &name, "file/PULP_MANIFEST", "additive").await;
    ops::settle(&c, ops::sync(&c, t, &repo, None, "additive").await.unwrap(), &no_progress).await.unwrap();
    let (mut app, mut rx) = app().await;
    open_list(&mut app, &mut rx, Kind::Repos(t, false), &name).await;
    let r = row(&app, "name", &name).unwrap();
    run(&mut app, Act::SyncHistory, r);
    settle(&mut app, &mut rx).await;
    let Some(Modal::Text { title, lines, .. }) = app.modal.take() else { panic!("no history: {:?}", texts(&app)) };
    assert_eq!((title, lines.len()), (format!("Sync history of {name} (2 tasks)"), 2), "{lines:?}");
    assert!(lines.iter().all(|l| l[19..].starts_with("  completed ")), "{lines:?}");
    let v1 = short(&format!("{repo}versions/1/"));
    assert!(!lines[0].contains(" -> ") && lines[1].ends_with(&format!(" -> {v1}")), "the second sync made no version: {lines:?}");
    cleanup(&c, &[&repo, &remote]).await;
}

/// The Remotes screens through the TUI for File, RPM, Deb and File Git: Create, Edit (a task: labels, and the
/// policy where the type has one), Delete: the impact names the repository that uses the remote as its default,
/// the typed name, the task; Pulp then unsets the repository's remote.
#[tokio::test]
async fn remotes_create_edit_and_delete_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let (mut app, mut rx) = app().await;
    for (t, git, path) in [
        (RepoType::File, false, "file/PULP_MANIFEST"),
        (RepoType::Rpm, false, "rpm/"),
        (RepoType::Deb, false, "deb/"),
        (RepoType::File, true, "git/files.git"),
    ] {
        let kind = Kind::Remotes(t, git);
        if let Some(e) = kind.unsupported(&app.conn.as_ref().unwrap().caps) {
            eprintln!("skipping: {e}");
            continue;
        }
        let name = unique(&format!("it-remote-{}{}", t.component(), if git { "-git" } else { "" }));
        open_list(&mut app, &mut rx, kind, &name).await;
        run(&mut app, Act::Create, Value::Null);
        let url = format!("{}/{path}", upstream());
        let mut values = vec![("name", name.as_str()), ("url", url.as_str())];
        if t == RepoType::Deb {
            values.push(("distributions", "fixture"));
        }
        fill(&mut app, &values);
        settle(&mut app, &mut rx).await;
        shown(&app, &format!("create remote {name}: done"));
        let r = row(&app, "name", &name).unwrap_or_else(|| panic!("the list reloads with it: {:?}", app.list.items));
        assert_eq!(r["url"].as_str(), Some(url.as_str()));
        let href = s(&r, "pulp_href");
        let repo = create(&c, t.repositories(), json!({ "name": name, "remote": href }), "/repositories/").await;

        run(&mut app, Act::Edit, r);
        let mut values = vec![("labels", "a=1")];
        if !git {
            values.push(("policy", "on_demand"));
        }
        fill(&mut app, &values);
        settle_tasks(&mut app, &mut rx).await;
        shown(&app, &format!("task completed: update remote {name}"));
        let r = row(&app, "name", &name).unwrap();
        assert_eq!(r["pulp_labels"], json!({ "a": "1" }), "the list reloads");
        assert_eq!(r["policy"].as_str(), if git { None } else { Some("on_demand") });

        run(&mut app, Act::Delete, r);
        settle(&mut app, &mut rx).await; // the impact is read before the confirmation
        let (lines, typed) = yes(&mut app);
        assert_eq!(typed.as_deref(), Some(name.as_str()));
        assert_eq!(lines[1], format!("1 repository(ies) use it as default remote: {name}"), "{lines:?}");
        settle_tasks(&mut app, &mut rx).await;
        shown(&app, &format!("task completed: delete remote {name}"));
        assert!(row(&app, "name", &name).is_none(), "the list reloads without it");
        assert!(c.get::<Value>(&href, &[]).await.unwrap_err().is_not_found());
        assert_eq!(c.get::<Value>(&repo, &[]).await.unwrap()["remote"], Value::Null, "Pulp unsets the default remote");
        cleanup(&c, &[&repo]).await;
    }
}

/// Row `number` of the open Versions list.
fn version_row(app: &App, number: i64) -> Value {
    app.list.items.iter().find(|v| v["number"] == number).cloned().unwrap_or_else(|| panic!("no version {number}: {:?}", app.list.items))
}

/// How many units of `version` have their stored files, of how many (synced units of an `immediate` remote have them).
async fn stored(c: &PulpClient, t: RepoType, version: &str) -> (usize, usize) {
    let q = [("repository_version", version.to_string()), ("fields", "artifact".into())];
    let units: Vec<Value> = c.list_all(&c.api(t.content()), &q, 1000).await.unwrap();
    (units.iter().filter(|u| u["artifact"].is_string()).count(), units.len())
}

/// The Versions screen of type `t` through the TUI, on a repository with versions 1 and 2 (`two_versions`; `path`: its
/// fixture), then Reclaim space on Repositories:
/// * Compare (m) of the marked version 1 with version 2 lists the removed unit;
/// * Restore (R) of version 1: the preview, then version 3 with the content of version 1; the list reloads with it;
/// * Repair (F) of version 1: the confirmation, the task, its counts;
/// * Delete (d) of the marked version 2: the typed id, the Task Center, the mark goes, the list reloads without it;
/// * Reclaim space (R): the impact (versions, remote), the typed name, the task; the files it reports are gone.
async fn version_actions_through_the_tui(t: RepoType, path: &str) {
    let (c, name) = (client(), unique(&format!("it-ver-{}", t.component())));
    let (repo, remote, [v1, v2]) = two_versions(&c, t, &name, path).await;
    let (mut app, mut rx) = app().await;
    open_list(&mut app, &mut rx, Kind::Versions(Some((repo.clone(), name.clone()))), "").await;

    // Compare (m): version 1 marked, version 2 selected.
    app.list.marked = [v1.clone()].into();
    let r = version_row(&app, 2);
    run(&mut app, Act::Compare, r);
    settle(&mut app, &mut rx).await;
    let Some(Modal::Text { title, lines, .. }) = app.modal.take() else { panic!("no comparison: {:?}", texts(&app)) };
    let removed = lines.iter().filter(|l| l.starts_with("- ")).count();
    let head = format!("{} -> {}: +0 added, -{removed} removed", short(&v1), short(&v2));
    assert_eq!((title.as_str(), lines[0].as_str()), ("Version comparison", head.as_str()), "{lines:?}");
    // pulp_deb also drops the Release files and indices from every new version.
    assert!(removed == 1 || (t == RepoType::Deb && removed > 1), "{lines:?}");
    app.list.marked.clear();

    // Restore (R) version 1 as latest: version 3 has its content.
    let r = version_row(&app, 1);
    run(&mut app, Act::Restore, r);
    settle(&mut app, &mut rx).await; // the preview is read before the confirmation
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed, None, "a plain confirmation on a profile that is not production");
    assert_eq!(lines[0], format!("Make a new latest version of {name} with the content of version 1; newer versions are kept."));
    assert!(lines.iter().any(|l| l.starts_with(&format!("latest {} -> new (= v1): +", short(&v2)))), "{lines:?}");
    settle_tasks(&mut app, &mut rx).await;
    shown(&app, &format!("restore {name} v1 as latest: new version 3"));
    let v3 = latest(&c, &repo).await;
    assert!(v3.ends_with("/versions/3/"), "{v3}");
    let keys = async |v: &str| ops::content_keys(&c, t, Some(v)).await.unwrap();
    assert_eq!(keys(&v3).await, keys(&v1).await);
    assert_eq!(version_row(&app, 3)["pulp_href"], v3.as_str(), "the list reloads with it");

    // Repair (F) version 1: a task that counts nothing to repair.
    let r = version_row(&app, 1);
    run(&mut app, Act::Repair, r);
    fill(&mut app, &[("verify_checksums", "true")]);
    settle(&mut app, &mut rx).await; // the version is read before the confirmation
    let (lines, typed) = yes(&mut app);
    let first = format!("Repair version 1 ({}): Pulp checks the files of its ", short(&v1));
    assert!(typed.is_none() && lines[0].starts_with(&first), "{lines:?}");
    settle(&mut app, &mut rx).await;
    shown(&app, "repair version 1: 0 missing, 0 corrupt, 0 repaired");
    let task: Task = c.get(&app.tasks.items[0].href, &[]).await.unwrap();
    assert!(task.name.ends_with("repair_version") && task.state == "completed", "{task:?}");

    // Delete (d) the marked version 2: the typed id, the task; the mark goes.
    app.list.marked = [v2.clone()].into();
    let r = version_row(&app, 2);
    run(&mut app, Act::Delete, r);
    settle(&mut app, &mut rx).await; // the impact is read before the confirmation
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed, Some(short(&v2)));
    assert_eq!(lines[0], format!("Delete repository version 2 ({}) and its 0 publication(s).", short(&v2)));
    settle_tasks(&mut app, &mut rx).await;
    shown(&app, "task completed: delete version 2");
    assert!(app.list.marked.is_empty(), "the mark goes when the delete completed");
    assert!(app.list.items.iter().all(|v| v["number"] != 2), "the list reloads without it");
    assert!(c.get::<Value>(&v2, &[]).await.unwrap_err().is_not_found());

    // Reclaim space (R on Repositories): the files that only this repository has go; the content stays.
    let (before, units) = stored(&c, t, &v3).await;
    open_list(&mut app, &mut rx, Kind::Repos(t, false), &name).await;
    let r = row(&app, "name", &name).unwrap();
    run(&mut app, Act::Reclaim, r);
    fill(&mut app, &[("keep", "")]);
    settle(&mut app, &mut rx).await; // the impact is read before the confirmation
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed.as_deref(), Some(name.as_str()));
    assert!(lines[1].starts_with("The content stays in all 3 versions: "), "{lines:?}");
    assert_eq!(lines[3..], [format!("Default remote: {name} (policy immediate)."), "No version keeps its files.".into()]);
    settle(&mut app, &mut rx).await;
    let done = texts(&app).into_iter().find_map(|n| n.strip_prefix(&format!("reclaim space of {name}: deleted ")).map(String::from));
    let deleted: usize = done.unwrap_or_else(|| panic!("no result: {:?}", texts(&app))).trim_end_matches(" file(s)").parse().unwrap();
    let (after, still) = stored(&c, t, &v3).await;
    assert_eq!(still, units, "the content stays in the version");
    // Units that another repository of the server also has keep their file (other tests sync the fixtures too).
    assert_eq!(before - after, deleted, "{before} with a file before, {after} after");
    let task: Task = c.get(&app.tasks.items[0].href, &[]).await.unwrap();
    assert!(task.name.ends_with("reclaim_space") && task.state == "completed", "{task:?}");
    cleanup(&c, &[&repo, &remote]).await;
}

#[tokio::test]
async fn file_version_actions_through_the_tui() {
    if live_enabled() {
        version_actions_through_the_tui(RepoType::File, "file/PULP_MANIFEST").await;
    }
}

#[tokio::test]
async fn rpm_version_actions_through_the_tui() {
    if live_enabled() {
        version_actions_through_the_tui(RepoType::Rpm, "rpm/").await;
    }
}

#[tokio::test]
async fn deb_version_actions_through_the_tui() {
    if live_enabled() {
        version_actions_through_the_tui(RepoType::Deb, "deb/").await;
    }
}

/// Prune orphans (O on Versions) with an age that no content of a running test has (a year): the confirmation shows
/// Pulp's count for that age and asks for the profile name; the notice gives what the cleanup task deleted.
#[tokio::test]
async fn prune_orphans_through_the_tui() {
    if !live_enabled() {
        return;
    }
    const YEAR: &str = "525600";
    let c = client();
    let (mut app, mut rx) = app().await;
    open_list(&mut app, &mut rx, Kind::Versions(None), "").await;
    run(&mut app, Act::PruneOrphans, Value::Null);
    fill(&mut app, &[("minutes", YEAR)]);
    settle(&mut app, &mut rx).await; // the orphans are counted before the confirmation
    let n = c.list::<Value>(&c.api("content/"), &[("orphaned_for", YEAR.into()), ("limit", "1".into())]).await.unwrap().count;
    let (lines, typed) = yes(&mut app);
    assert_eq!(
        (typed.as_deref(), lines[1].as_str()),
        (Some("pulp1"), format!("{n} content unit(s) orphaned for {YEAR} minutes or more.").as_str())
    );
    settle(&mut app, &mut rx).await;
    let task: Task = c.get(&app.tasks.items[0].href, &[]).await.unwrap();
    assert!(task.name.ends_with("orphan_cleanup") && task.state == "completed", "{task:?}");
    let done = |code: &str| task.progress_reports.iter().find(|r| r.code == code).and_then(|r| r.done).unwrap_or(0);
    let (units, files) = (done("clean-up.content"), done("clean-up.artifacts"));
    shown(&app, &format!("prune orphans on pulp1: deleted {units} content unit(s) and {files} artifact(s)"));
}

/// Upload (u) through the TUI into an empty repository of each type: a File at its relative path, an RPM package,
/// and a .deb into a release (distribution and component). Each makes version 1 with the unit, and the list reloads.
#[tokio::test]
async fn uploads_of_each_type_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, name, dir) = (client(), unique("it-upload"), tempfile::tempdir().unwrap());
    let txt = dir.path().join("u.txt");
    std::fs::write(&txt, format!("{name}\n")).unwrap(); // unique bytes: a rerun makes new content
    let (rel, release) = (format!("docs/{name}.txt"), format!("release component {name}/main"));
    let rpm = fixtures().join("rpms/fixture-upload-3.0-1.noarch.rpm");
    let deb = fixtures().join("upstream/deb/pool/main/f/fixture-alpha/fixture-alpha_1.0-1_amd64.deb");
    let (mut app, mut rx) = app().await;
    for (t, file, values, key) in [
        (RepoType::File, txt, vec![("relative_path", rel.as_str())], format!("{rel}  (")),
        (RepoType::Rpm, rpm, vec![], "fixture-upload-3.0-1.noarch".into()),
        (RepoType::Deb, deb, vec![("distribution", name.as_str()), ("component", "main")], "fixture-alpha_1.0-1_amd64".into()),
    ] {
        let repo_name = format!("{name}-{}", t.component());
        let repo = create(&c, t.repositories(), json!({ "name": repo_name }), "/repositories/").await;
        open_list(&mut app, &mut rx, Kind::Repos(t, false), &repo_name).await;
        let r = row(&app, "name", &repo_name).unwrap();
        run(&mut app, Act::Upload, r);
        let path = file.display().to_string();
        fill(&mut app, &[&[("path", path.as_str())][..], &values].concat());
        settle(&mut app, &mut rx).await;
        let done = texts(&app).into_iter().find(|n| n.starts_with(&format!("uploaded {path} (sha256 ")));
        assert!(done.is_some_and(|d| d.ends_with(": new version 1")), "{:?}", texts(&app));
        let v1 = latest(&c, &repo).await;
        let keys = ops::content_keys(&c, t, Some(&v1)).await.unwrap();
        assert!(keys.len() == 1 && keys.iter().all(|k| k.starts_with(&key)), "{keys:?}");
        assert_eq!(row(&app, "name", &repo_name).unwrap()["latest_version_href"], v1.as_str(), "the list reloads");
        if t == RepoType::Deb {
            let all = ops::all_content_keys(&c, &Capabilities::default(), t, &v1).await.unwrap();
            assert!(all.iter().any(|k| k.starts_with(&release)), "the release of the form: {all:?}");
        }
        cleanup(&c, &[&repo]).await;
    }
}

/// Copy content (C) through the TUI into an empty repository of each type: File with a path rule (`modify/`), RPM
/// without rules (`rpm/copy/` of all content), Deb with a name rule (structured `deb/copy/`). The confirmation says
/// what goes; the Task Center follows the task; the target has exactly the chosen units.
#[tokio::test]
async fn copy_content_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let (mut app, mut rx) = app().await;
    for (t, path, rules, left_out) in [
        (RepoType::File, "file/PULP_MANIFEST", "exclude path=isos/*", Some("isos/")),
        (RepoType::Rpm, "rpm/", "", None),
        (RepoType::Deb, "deb/", "exclude name=*-dbg", Some("fixture-beta-dbg_")),
    ] {
        let name = unique(&format!("it-copy-{}", t.component()));
        let (src, remote) = synced_repo(&c, t, &name, path, "additive").await;
        let sv = latest(&c, &src).await;
        let dst_name = format!("{name}-dst");
        let dst = create(&c, t.repositories(), json!({ "name": dst_name }), "/repositories/").await;

        open_list(&mut app, &mut rx, Kind::Repos(t, false), &dst_name).await;
        let r = row(&app, "name", &dst_name).unwrap();
        run(&mut app, Act::CopyContent, r);
        let mut values = vec![("source", name.as_str()), ("filters", rules)];
        if t == RepoType::Rpm {
            values.push(("deps", "false"));
        }
        fill(&mut app, &values);
        settle(&mut app, &mut rx).await; // the preview reads the source
        let want: std::collections::BTreeSet<String> =
            ops::content_keys(&c, t, Some(&sv)).await.unwrap().into_iter().filter(|k| left_out.is_none_or(|p| !k.starts_with(p))).collect();
        let (lines, typed) = yes(&mut app);
        let what = if rules.is_empty() { "all content".to_string() } else { format!("{} unit(s)", want.len()) };
        assert_eq!((typed, lines[0].clone()), (None, format!("Copy {what} from {} into {dst_name}.", short(&sv))));
        settle_tasks(&mut app, &mut rx).await;
        shown(&app, &format!("task completed: copy {name} -> {dst_name}"));
        let got = ops::content_keys(&c, t, Some(&latest(&c, &dst).await)).await.unwrap();
        assert!(!want.is_empty() && got == want, "{rules}: {got:?}");
        assert_eq!(row(&app, "name", &dst_name).unwrap()["latest_version_href"], latest(&c, &dst).await.as_str(), "the list reloads");
        cleanup(&c, &[&dst, &src, &remote]).await;
    }
}

/// The repository version that distribution `name` of type `t` serves through its pinned publication.
async fn pinned_version(c: &PulpClient, t: RepoType, name: &str) -> (String, String) {
    let d: Value = ops::find_by_name(c, &c.api(t.distributions()), name).await.unwrap().unwrap_or_else(|| panic!("no distribution {name}"));
    let p: Value = c.get(&s(&d, "publication"), &[]).await.unwrap();
    (s(&p, "pulp_href"), s(&p, "repository_version"))
}

/// Serving from the Versions and Repositories screens through the TUI, on a repository with versions 1 and 2
/// (`path`: its fixture; `file`: a file each publication serves):
/// * Publish (P) on version 1 makes its publication;
/// * Distribute (D) on version 2 makes a distribution pinned to a publication of version 2;
/// * Rollback (b) on version 1 points that distribution to the publication of version 1 (the preview names it);
/// * Distribute (D) on Repositories with "follow repository latest" binds a distribution to the repository;
/// * the Logs screen lists the actions and the requests; Inspect shows a request.
async fn serve_from_versions_through_the_tui(t: RepoType, path: &str, file: &str) {
    let (c, name) = (client(), unique(&format!("it-serve-{}", t.component())));
    let (repo, remote, [v1, v2]) = two_versions(&c, t, &name, path).await;
    let (pinned, follow) = (format!("{name}-pin"), format!("{name}-follow"));
    let (mut app, mut rx) = app().await;
    open_list(&mut app, &mut rx, Kind::Versions(Some((repo.clone(), name.clone()))), "").await;

    // Publish (P) version 1 with the default options.
    let r = version_row(&app, 1);
    run(&mut app, Act::Publish, r);
    if matches!(app.modal, Some(Modal::Form { .. })) {
        fill(&mut app, &[]);
    }
    settle(&mut app, &mut rx).await;
    let pubs: Vec<Value> = c.list_all(&c.api(t.publications()), &[("repository_version", v1.clone())], 10).await.unwrap();
    let p1 = pubs.first().map(|p| s(p, "pulp_href")).unwrap_or_else(|| panic!("no publication: {:?}", texts(&app)));
    shown(&app, &format!("publication {} for {}", short(&p1), short(&v1)));

    // Distribute (D) version 2: pinned.
    let r = version_row(&app, 2);
    run(&mut app, Act::Distribute, r.clone());
    settle(&mut app, &mut rx).await; // the repository name is read before the form
    fill(&mut app, &[("dist", &pinned), ("base_path", &pinned)]);
    settle(&mut app, &mut rx).await;
    let d: Value = ops::find_by_name(&c, &c.api(t.distributions()), &pinned).await.unwrap().unwrap_or_else(|| panic!("{:?}", texts(&app)));
    shown(&app, &format!("distribution {pinned} serves {}", s(&d, "base_url")));
    assert_eq!(pinned_version(&c, t, &pinned).await.1, v2);
    assert_eq!(served(&c, &format!("{}{file}", s(&d, "base_url"))).await.0, 200);

    // Rollback (b) on version 1: the distribution then serves the publication of version 1.
    let r1 = version_row(&app, 1);
    run(&mut app, Act::Rollback, r1);
    fill(&mut app, &[("dist", &pinned)]);
    settle(&mut app, &mut rx).await; // the preview
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed, None, "a plain confirmation on a profile that is not production");
    assert_eq!(lines[2], format!("  will serve:  {} (pinned)", short(&v1)));
    assert!(lines[3].starts_with(&format!("  publication: existing {} (", short(&p1))), "{lines:?}");
    settle(&mut app, &mut rx).await;
    assert!(texts(&app).iter().any(|n| n.starts_with(&format!("{pinned} now serves {}: ", short(&v1)))), "{:?}", texts(&app));
    assert_eq!(pinned_version(&c, t, &pinned).await, (p1.clone(), v1.clone()));

    // Distribute (D) on Repositories: follow the repository.
    open_list(&mut app, &mut rx, Kind::Repos(t, false), &name).await;
    let rr = row(&app, "name", &name).unwrap();
    run(&mut app, Act::Distribute, rr);
    fill(&mut app, &[("dist", &follow), ("base_path", &follow), ("mode", "follow repository latest")]);
    settle(&mut app, &mut rx).await;
    let f: Value = ops::find_by_name(&c, &c.api(t.distributions()), &follow).await.unwrap().unwrap_or_else(|| panic!("{:?}", texts(&app)));
    shown(&app, &format!("distribution {follow} serves {}", s(&f, "base_url")));
    assert_eq!((s(&f, "repository"), &f["publication"]), (repo.clone(), &Value::Null));
    assert_eq!(served(&c, &format!("{}{file}", s(&f, "base_url"))).await.0, 200, "the newest publication");

    // Logs: the actions and the requests of this session; Inspect of the request that made the distribution.
    open_list(&mut app, &mut rx, Kind::Logs { polls: false }, "").await;
    let has = |kind: &str, summary: &dyn Fn(&str) -> bool| {
        app.list.items.iter().find(|l| l["kind"] == kind && summary(&s(l, "summary"))).cloned()
    };
    let action = format!("{} on {}", Act::Distribute.label(), short(&v2));
    assert!(has("action", &|x| x == action).is_some(), "{:?}", app.list.items.iter().take(20).collect::<Vec<_>>());
    let made = has("request", &|x| x.starts_with("POST ") && x.contains(t.distributions()) && x.contains(" 202 "));
    let made = made.unwrap_or_else(|| panic!("no distribution create: {:?}", app.list.items.iter().take(30).collect::<Vec<_>>()));
    run(&mut app, Act::Inspect, made);
    let Some(Modal::Text { lines, .. }) = app.modal.take() else { panic!("no detail: {:?}", texts(&app)) };
    assert!(lines[0].starts_with("POST http") && lines.iter().any(|l| l.contains(&follow)), "{lines:?}");
    cleanup(&c, &[s(&f, "pulp_href").as_str(), s(&d, "pulp_href").as_str(), &repo, &remote]).await;
}

#[tokio::test]
async fn file_serve_from_versions_through_the_tui() {
    if live_enabled() {
        serve_from_versions_through_the_tui(RepoType::File, "file/PULP_MANIFEST", "PULP_MANIFEST").await;
    }
}

#[tokio::test]
async fn rpm_serve_from_versions_through_the_tui() {
    if live_enabled() {
        serve_from_versions_through_the_tui(RepoType::Rpm, "rpm/", "repodata/repomd.xml").await;
    }
}

/// Distribute (D) with the form defaults through the TUI (audit F54), on a repository of type `t` with versions 1 and
/// 2 (`path`: its fixture): D on version 2 makes the distribution of the repository name with no confirm. D on version
/// 1 hits that distribution: the confirm shows what it serves now and after and asks for its name. After Esc it still
/// serves version 2; after the typed name it serves version 1.
async fn distribute_re_points_after_the_typed_name_through_the_tui(t: RepoType, path: &str) {
    let (c, name) = (client(), unique(&format!("it-redist-{}", t.component())));
    let (repo, remote, [v1, v2]) = two_versions(&c, t, &name, path).await;
    let (mut app, mut rx) = app().await;
    open_list(&mut app, &mut rx, Kind::Versions(Some((repo.clone(), name.clone()))), "").await;
    let distribute = async |app: &mut App, rx: &mut UnboundedReceiver<Msg>, number: i64| {
        run(app, Act::Distribute, version_row(app, number));
        settle(app, rx).await; // the repository name
        fill(app, &[]);
        settle(app, rx).await; // the distribution of that name, and what it serves
    };
    distribute(&mut app, &mut rx, 2).await;
    assert!(app.modal.is_none(), "a new distribution: no confirm");
    assert_eq!(pinned_version(&c, t, &name).await.1, v2, "{:?}", texts(&app));
    for confirm in [false, true] {
        distribute(&mut app, &mut rx, 1).await;
        let Some(Modal::Confirm { lines, typed, .. }) = &app.modal else { panic!("no confirm: {:?}", texts(&app)) };
        assert_eq!(typed.as_deref(), Some(name.as_str()));
        assert!(lines[1].starts_with(&format!("  now serves:  {} (pinned to publication ", short(&v2))), "{lines:?}");
        assert_eq!(lines[2], format!("  will serve:  a publication of version 1 of repository {name} (pinned)"));
        if confirm {
            yes(&mut app);
            settle(&mut app, &mut rx).await;
        } else {
            app.modal = None; // Esc
            assert_eq!(pinned_version(&c, t, &name).await.1, v2, "nothing changed");
        }
    }
    assert_eq!(pinned_version(&c, t, &name).await.1, v1, "re-pointed: {:?}", texts(&app));
    let d: Value = ops::find_by_name(&c, &c.api(t.distributions()), &name).await.unwrap().unwrap();
    cleanup(&c, &[s(&d, "pulp_href").as_str(), &repo, &remote]).await;
}

#[tokio::test]
async fn file_distribute_re_points_after_the_typed_name_through_the_tui() {
    if live_enabled() {
        distribute_re_points_after_the_typed_name_through_the_tui(RepoType::File, "file/PULP_MANIFEST").await;
    }
}

#[tokio::test]
async fn deb_distribute_re_points_after_the_typed_name_through_the_tui() {
    if live_enabled() {
        distribute_re_points_after_the_typed_name_through_the_tui(RepoType::Deb, "deb/").await;
    }
}

/// Sync (s) of type `t` with the mirror `policy` through the TUI (audit T02), on a repository whose version 2 lacks one
/// unit of the upstream (`path`): the mirror confirm says that the new version matches the upstream (mirror_complete:
/// and is published), then the Task Center follows the sync; version 3 has the upstream content again.
async fn mirror_sync_through_the_tui(t: RepoType, path: &str, policy: &str) {
    let (c, name) = (client(), unique(&format!("it-mirror-{}", t.component())));
    let (repo, remote, [v1, _]) = two_versions(&c, t, &name, path).await;
    let (mut app, mut rx) = app().await;
    open_list(&mut app, &mut rx, Kind::Repos(t, false), &name).await;
    let r = row(&app, "name", &name).unwrap_or_else(|| panic!("no row: {:?}", app.list.items));
    run(&mut app, Act::Sync, r);
    settle(&mut app, &mut rx).await; // the name of the default remote
    // pulp_file 3.95 has no Optimize: its sync is always full.
    let (values, n) = ([("policy", policy), ("optimize", "false")], 1 + usize::from(form_has(&app, "optimize")));
    fill(&mut app, &values[..n]);
    settle(&mut app, &mut rx).await; // the retention of the repository
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed, None, "the retention keeps the current version: {lines:?}");
    assert_eq!(lines[0], "Mirror sync makes the NEW repository version match the upstream exactly:");
    assert_eq!(lines.iter().any(|l| l.starts_with("mirror_complete also publishes")), policy == "mirror_complete", "{lines:?}");
    let file = format!("File: the mirror sync of {name} also publishes the new version");
    assert_eq!(lines.iter().any(|l| l.starts_with(&file)), t == RepoType::File, "{lines:?}");
    settle_tasks(&mut app, &mut rx).await;
    shown(&app, &format!("task completed: sync {name} ({policy})"));
    let v3 = latest(&c, &repo).await;
    assert!(v3.ends_with("/versions/3/"), "{v3}");
    let keys = async |v: &str| ops::content_keys(&c, t, Some(v)).await.unwrap();
    assert_eq!(keys(&v3).await, keys(&v1).await, "the upstream content again");
    // pulp_file 3.123 also publishes a mirror sync (docs/COMPATIBILITY.md), as the confirm says.
    let pubs: Vec<Value> = c.list_all(&c.api(t.publications()), &[("repository_version", v3.clone())], 10).await.unwrap();
    assert_eq!(pubs.len(), usize::from(policy == "mirror_complete" || t == RepoType::File), "{pubs:?}");
    // The repository delete takes its publications along.
    cleanup(&c, &[&repo, &remote]).await;
}

#[tokio::test]
async fn rpm_mirror_sync_through_the_tui() {
    if live_enabled() {
        mirror_sync_through_the_tui(RepoType::Rpm, "rpm/", "mirror_complete").await;
    }
}

#[tokio::test]
async fn file_mirror_sync_through_the_tui() {
    if live_enabled() {
        mirror_sync_through_the_tui(RepoType::File, "file/PULP_MANIFEST", "mirror").await;
    }
}

#[tokio::test]
async fn deb_mirror_sync_through_the_tui() {
    if live_enabled() {
        mirror_sync_through_the_tui(RepoType::Deb, "deb/", "mirror").await;
    }
}

/// RPM Sync (s) with `mirror_content_only`, Optimize off and Skip srpm through the TUI (audit T03), on a repository
/// whose version 2 lacks one unit of the upstream: the sync request has the policy and both options, the new version
/// has the upstream content again, and no publication is made (only `mirror_complete` publishes). The fixture has no
/// source package, so the skip type shows only in the request. A second sync of the same form is not skipped by
/// Pulp: Optimize off reaches the task. A server without a field (older pulp_rpm) gets the request without it.
#[tokio::test]
async fn rpm_mirror_content_only_sync_with_options_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, t, name) = (client(), RepoType::Rpm, unique("it-mirror-co"));
    let (repo, remote, [v1, _]) = two_versions(&c, t, &name, "rpm/").await;
    let (mut app, mut rx) = app().await;
    open_list(&mut app, &mut rx, Kind::Repos(t, false), &name).await;
    // The sync through the form: the request body that the TUI sent, the one it should send, and the finished task.
    let sync = async |app: &mut App, rx: &mut UnboundedReceiver<Msg>| -> (Value, Value, Task) {
        let mark = pulp_tui::session_log::last_id();
        run(app, Act::Sync, row(app, "name", &name).unwrap_or_else(|| panic!("no row: {:?}", app.list.items)));
        settle(app, rx).await; // the name of the default remote
        let mut want = json!({ "remote": remote, "sync_policy": "mirror_content_only" });
        let mut values = vec![("policy", "mirror_content_only")];
        if form_has(app, "optimize") {
            (want["optimize"], values) = (json!(false), [values, vec![("optimize", "false")]].concat());
        }
        if form_has(app, "skip_srpm") {
            (want["skip_types"], values) = (json!(["srpm"]), [values, vec![("skip_srpm", "true")]].concat());
        }
        fill(app, &values);
        settle(app, rx).await; // the retention of the repository
        let (lines, typed) = yes(app);
        assert_eq!(typed, None, "{lines:?}");
        assert_eq!(lines[0], "Mirror sync makes the NEW repository version match the upstream exactly:");
        assert!(!lines.iter().any(|l| l.contains("publishes")), "mirror_content_only makes no publication: {lines:?}");
        settle_tasks(app, rx).await;
        shown(app, &format!("task completed: sync {name} (mirror_content_only)"));
        let rows = pulp_tui::session_log::rows(true);
        let sent = format!("POST {repo}sync/ ");
        let post = rows.iter().find(|l| l["id"].as_u64() > Some(mark) && s(l, "summary").starts_with(&sent));
        let id = post.unwrap_or_else(|| panic!("no {sent}: {rows:?}"))["id"].as_u64().unwrap();
        let detail = pulp_tui::session_log::detail(id).unwrap();
        let body = detail.iter().skip_while(|l| *l != "── request body ──").skip(1).take_while(|l| *l != "── response body ──");
        let body: Value = serde_json::from_str(&body.cloned().collect::<Vec<_>>().join("\n")).unwrap();
        let task: Task = c.get(&app.tasks.items.first().unwrap().href, &[]).await.unwrap();
        (body, want, task)
    };
    let (body, want, _) = sync(&mut app, &mut rx).await;
    assert_eq!(body, want);
    let v3 = latest(&c, &repo).await;
    assert!(v3.ends_with("/versions/3/"), "{v3}");
    let keys = async |v: &str| ops::content_keys(&c, t, Some(v)).await.unwrap();
    assert_eq!(keys(&v3).await, keys(&v1).await, "the upstream content again");
    let pubs: Vec<Value> = c.list_all(&c.api(t.publications()), &[("repository_version", v3.clone())], 10).await.unwrap();
    assert!(pubs.is_empty(), "{pubs:?}");

    let (_, want, task) = sync(&mut app, &mut rx).await;
    if want.get("optimize").is_some() {
        let codes: Vec<&str> = task.progress_reports.iter().map(|p| p.code.as_str()).collect();
        assert!(!codes.is_empty() && !codes.contains(&"sync.was_skipped"), "a full sync: {codes:?}");
    }
    assert_eq!(latest(&c, &repo).await, v3, "the upstream did not change");
    cleanup(&c, &[&repo, &remote]).await;
}

/// Upload (u) of a File with Overwrite off through the TUI (audit T02): other content at the same relative path fails
/// the task and the repository keeps its file; with Overwrite on it is replaced in a new version.
#[tokio::test]
async fn file_upload_without_overwrite_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, t, name) = (client(), RepoType::File, unique("it-tui-overwrite"));
    let (mut app, mut rx) = app().await;
    if !app.conn.as_ref().unwrap().caps.has_field("file.FileContent", "overwrite") {
        eprintln!("skipping: this server's File uploads have no `overwrite` field (older pulpcore)");
        return;
    }
    let repo = create(&c, t.repositories(), json!({ "name": name }), "/repositories/").await;
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = (dir.path().join("old.txt"), dir.path().join("new.txt"));
    // Unique bytes, so a rerun does not find the content units of an earlier run.
    std::fs::write(&old, format!("{name} old\n")).unwrap();
    std::fs::write(&new, format!("{name} new\n")).unwrap();
    // The digests of the files of the latest version.
    let files = async || -> Vec<String> {
        let q = [("repository_version", latest(&c, &repo).await), ("fields", "sha256".to_string())];
        c.list::<Value>(&c.api(t.content()), &q).await.unwrap().results.iter().map(|x| s(x, "sha256")).collect()
    };
    let sha = |p: &std::path::Path| hex::encode(<sha2::Sha256 as sha2::Digest>::digest(std::fs::read(p).unwrap()));
    open_list(&mut app, &mut rx, Kind::Repos(t, false), &name).await;
    for (file, overwrite, kept) in [(&old, "false", &old), (&new, "false", &old), (&new, "true", &new)] {
        let r = row(&app, "name", &name).unwrap();
        run(&mut app, Act::Upload, r);
        let path = file.display().to_string();
        fill(&mut app, &[("path", path.as_str()), ("relative_path", "same.txt"), ("overwrite", overwrite)]);
        settle(&mut app, &mut rx).await;
        let last = texts(&app).pop().unwrap_or_default();
        assert_eq!(last.starts_with(&format!("uploaded {path} ")), file == kept, "{overwrite}: {last}");
        if file != kept {
            assert!(last.contains("overwrite"), "Pulp says why: {last}");
        }
        assert_eq!(files().await, [sha(kept)], "{overwrite}: the file at same.txt");
    }
    cleanup(&c, &[&repo]).await;
}

/// Repository Edit (e) of a File repository through the TUI (audit T02): description, retained versions, manifest
/// file name, default remote and labels are saved; a second edit clears them.
#[tokio::test]
async fn file_repository_edit_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, t, name) = (client(), RepoType::File, unique("it-file-edit"));
    let remote = create(&c, t.remotes(), json!({ "name": name, "url": format!("{}/file/PULP_MANIFEST", upstream()) }), "/remotes/").await;
    let repo = create(&c, t.repositories(), json!({ "name": name }), "/repositories/").await;
    let (mut app, mut rx) = app().await;
    open_list(&mut app, &mut rx, Kind::Repos(t, false), &name).await;
    let keys = ["description", "retain_repo_versions", "manifest", "remote", "labels"];
    let set = ["docs", "3", "MANIFEST2", name.as_str(), "team=ops"];
    let stored = json!(["docs", 3, "MANIFEST2", remote, { "team": "ops" }]);
    for (values, want) in [(set, stored), (["", "", "PULP_MANIFEST", "", ""], json!([null, null, "PULP_MANIFEST", null, {}]))] {
        let r: Value = c.get(&repo, &[]).await.unwrap();
        run(&mut app, Act::Edit, r);
        settle(&mut app, &mut rx).await; // the name of the default remote, when there is one
        fill(&mut app, &keys.into_iter().zip(values).collect::<Vec<_>>());
        settle_tasks(&mut app, &mut rx).await;
        shown(&app, &format!("task completed: update repository {name}"));
        let r: Value = c.get(&repo, &[]).await.unwrap();
        let got = keys.map(|k| if k == "labels" { r["pulp_labels"].clone() } else { r[k].clone() });
        assert_eq!(json!(got), want, "{:?}", texts(&app));
    }
    cleanup(&c, &[&repo, &remote]).await;
}

/// The version numbers of repository `repo`, newest first.
async fn version_numbers(c: &PulpClient, repo: &str) -> Vec<u64> {
    let all: Vec<Value> = c.list_all(&format!("{repo}versions/"), &[("fields", "number".into())], 1000).await.unwrap();
    all.iter().filter_map(|v| v["number"].as_u64()).collect()
}

/// Repository Edit (e) of a lower Retain versions through the TUI (audit F53), on a repository with versions 0, 1 and
/// 2: the confirm names versions 1 and 0 (Pulp keeps the newest one) and asks for the repository name. After Esc the
/// versions stay; after the typed name Pulp deletes exactly the named versions.
#[tokio::test]
async fn a_lower_retain_versions_deletes_versions_only_after_the_typed_name_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let (c, t, name) = (client(), RepoType::File, unique("it-retain"));
    let (repo, remote, _) = two_versions(&c, t, &name, "file/PULP_MANIFEST").await;
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Repos(t, false);
    for confirm in [false, true] {
        run(&mut app, Act::Edit, c.get(&repo, &[]).await.unwrap());
        settle(&mut app, &mut rx).await; // the name of the default remote
        fill(&mut app, &[("retain_repo_versions", "1")]);
        settle(&mut app, &mut rx).await; // the impact
        let Some(Modal::Confirm { lines, typed, .. }) = &app.modal else { panic!("no confirm: {:?}", texts(&app)) };
        assert_eq!(typed.as_deref(), Some(name.as_str()));
        assert_eq!(lines[1], "  1, 0", "{lines:?}");
        if confirm {
            yes(&mut app);
            settle_tasks(&mut app, &mut rx).await;
            shown(&app, &format!("task completed: update repository {name}"));
        } else {
            app.modal = None; // Esc
            assert_eq!(version_numbers(&c, &repo).await, [2, 1, 0], "nothing sent");
            assert_eq!(c.get::<Value>(&repo, &[]).await.unwrap()["retain_repo_versions"], Value::Null);
        }
    }
    assert_eq!(version_numbers(&c, &repo).await, [2], "the named versions are gone");
    cleanup(&c, &[&repo, &remote]).await;
}
