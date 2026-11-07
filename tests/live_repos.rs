//! End-to-end RPM and File repository lifecycles against a live Pulp:
//! local repo + upload, remote + sync, versions, publication, distribution, served
//! content, updated content, rollback by re-pointing the distribution.

mod common;

use common::*;
use pulp_tui::pulp::models::{Package, RepoVersion};
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{Capabilities, Mutation, PulpClient, RepoType, file, rpm};
use pulp_tui::tui::actions::run;
use pulp_tui::tui::app::Modal;
use pulp_tui::tui::kinds::{Act, Kind};
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
/// Repositories screen, alone or marked (audit F02): the refusal names the view and the database, and nothing is sent.
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
    let refused = format!("refused: {name} is a snapshot repository of Content View foreignview (database another-database-id); ");
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
