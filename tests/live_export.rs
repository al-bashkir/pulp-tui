//! Disconnected export and import on one live Pulp: a Pulp export of a File repository, its
//! import into another repository through an importer mapping, and a filesystem export of a
//! File publication, done with the requests of the Exporters and Importers screens. Each run
//! uses its own directories below the export path.

mod common;

use common::*;
use pulp_tui::pulp::RepoType;
use pulp_tui::pulp::client::Page;
use pulp_tui::pulp::ops::{self, no_progress};
use serde_json::{Value, json};

/// In `ALLOWED_EXPORT_PATHS` and `ALLOWED_IMPORT_PATHS` of the test containers (tests/env/pulp-env.sh).
const EXPORTS: &str = "/var/lib/pulp/exports";

#[tokio::test]
async fn pulp_export_and_import_of_a_file_repository() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let name = unique("it-export");
    let (repo, remote) = synced_repo(&c, RepoType::File, &name, "file/PULP_MANIFEST", "mirror").await;
    let dir = format!("{EXPORTS}/{name}");
    let body = json!({ "name": name, "path": dir, "repositories": [repo] });
    let exporter = create(&c, "exporters/core/pulp/", body, "/exporters/").await;
    let exports = format!("{exporter}exports/");

    // An incremental export needs a previous export: Pulp answers 400 (the form refuses it first).
    let e = c.post(&exports, &json!({ "full": false })).await.unwrap_err();
    assert!(e.to_string().contains("Incremental export can only be requested when there is a previous export"), "{e}");

    // Export (E): a full export of the latest version, awaited; the export names its TOC file.
    let export = ops::create(&c, &exports, &json!({}), "/exports/", &no_progress).await.unwrap();
    let e: Value = c.get(&export, &[]).await.unwrap();
    let toc = e["toc_info"]["file"].as_str().unwrap().to_string();
    assert!(toc.starts_with(&format!("{dir}/export-")) && toc.ends_with("-toc.json"), "{toc}");
    let first = latest(&c, &repo).await;
    assert_eq!(e["exported_resources"], json!([first]), "the latest version");
    let files = e["output_file_info"].as_object().unwrap();
    assert!(files.contains_key(&toc) && files.keys().any(|f| f.ends_with(".tar")), "{files:?}");
    let ex: Value = c.get(&exporter, &[]).await.unwrap();
    assert_eq!(ex["last_export"].as_str(), Some(export.as_str()));

    // Import (I): the import check is the preview, then the import into another repository through the mapping.
    let copy = format!("{name}-copy");
    let dst = create(&c, RepoType::File.repositories(), json!({ "name": copy }), "/repositories/").await;
    let mapping = json!({ name.clone(): copy });
    let importer = create(&c, "importers/core/pulp/", json!({ "name": name, "repo_mapping": mapping }), "/importers/").await;
    let (ok, lines) = ops::import_check(&c, &toc, &mapping).await.unwrap();
    assert!(ok, "{lines:?}");
    let (ok, lines) = ops::import_check(&c, &format!("{dir}/missing-toc.json"), &mapping).await.unwrap();
    assert!(!ok && lines[1].contains("does not exist"), "{lines:?}");
    let (group, lines) = ops::pulp_import(&c, &importer, &toc, &|_| {}).await.unwrap();
    assert!(group.is_final() && group.outcome() == "completed", "{lines:?}");
    let repo_tasks = group.tasks.iter().filter(|t| t["name"].as_str().is_some_and(|n| n.ends_with("import_repository_version")));
    assert_eq!(repo_tasks.count(), 1, "{lines:?}");
    let present = async |r: &str| {
        let v: Value = c.get(&latest(&c, r).await, &[]).await.unwrap();
        v["content_summary"]["present"].as_object().unwrap().values().map(|x| x["count"].as_u64().unwrap()).sum::<u64>()
    };
    let n = present(&repo).await;
    assert!(n > 0 && present(&dst).await == n, "the copy has the {n} units of the exported version");

    // Incremental export (E, Incremental): a new file in the repository, then only what the last export lacks.
    let tmp = tempfile::tempdir().unwrap();
    let new_file = tmp.path().join("new.txt");
    std::fs::write(&new_file, format!("{name} new\n")).unwrap();
    ops::upload(&c, RepoType::File, &new_file, Some("new.txt"), Some(&repo), true, &|_, _| {}, &no_progress).await.unwrap();
    let inc = ops::create(&c, &exports, &json!({ "full": false }), "/exports/", &no_progress).await.unwrap();
    let e: Value = c.get(&inc, &[]).await.unwrap();
    assert_eq!(e["params"], json!({ "full": false }));
    assert_eq!(e["exported_resources"], json!([latest(&c, &repo).await]), "the latest version");
    let inc_toc = e["toc_info"]["file"].as_str().unwrap().to_string();
    assert_ne!(inc_toc, toc);
    // The incremental export imports on a server that has the earlier content: the copy gets the new file too.
    let (group, lines) = ops::pulp_import(&c, &importer, &inc_toc, &|_| {}).await.unwrap();
    assert!(group.is_final() && group.outcome() == "completed", "{lines:?}");
    assert_eq!(present(&dst).await, n + 1, "the copy has the new unit");

    // Versions (E, `name=number`): an older version instead of the latest one.
    let older = ops::create(&c, &exports, &json!({ "versions": [first] }), "/exports/", &no_progress).await.unwrap();
    let e: Value = c.get(&older, &[]).await.unwrap();
    assert_eq!(e["exported_resources"], json!([first]), "the chosen version");

    // Start versions (E, Incremental, `name=number`): Pulp starts there instead of at the last export.
    let body = json!({ "full": false, "start_versions": [first] });
    let since = ops::create(&c, &exports, &body, "/exports/", &no_progress).await.unwrap();
    let e: Value = c.get(&since, &[]).await.unwrap();
    assert_eq!((&e["params"], &e["exported_resources"]), (&body, &json!([latest(&c, &repo).await])));
    // Start versions without Incremental: 400 (the form refuses it first).
    let e = c.post(&exports, &json!({ "start_versions": [first] })).await.unwrap_err();
    assert!(e.to_string().contains("start_versions is only valid for incremental exports"), "{e}");

    // Imports (o) lists them with the TOC.
    let imports: Page<Value> = c.list(&format!("{importer}imports/"), &[("ordering", "pulp_created".into())]).await.unwrap();
    assert_eq!((imports.count, imports.results[0]["params"]["path"].as_str()), (2, Some(toc.as_str())));
    assert!(matches!(c.delete(&importer).await.unwrap(), pulp_tui::pulp::Mutation::Empty), "204, no task");

    // Exports (o) lists them; Delete removes the record only.
    let list: Page<Value> = c.list(&exports, &[("ordering", "pulp_created".into())]).await.unwrap();
    assert_eq!((list.count, list.results[0]["pulp_href"].as_str()), (4, Some(export.as_str())));
    for e in [&export, &inc, &older, &since] {
        assert!(matches!(c.delete(e).await.unwrap(), pulp_tui::pulp::Mutation::Empty), "204, no task");
    }
    assert_eq!(c.list::<Value>(&exports, &[]).await.unwrap().count, 0);

    // Deleting the exporter is a task.
    let m = c.delete(&exporter).await.unwrap();
    assert!(matches!(m, pulp_tui::pulp::Mutation::Task(_)));
    ops::settle(&c, m, &no_progress).await.unwrap();
    cleanup(&c, &[&dst, &repo, &remote]).await;
}

/// pulpcore 3.123.1 declares `dry_run` on a Pulp export but no code reads it: the task writes the export and
/// makes it the exporter's last export (the base of the next incremental one). So the Export form does not offer
/// it (audit F28). A pulpcore that implements it fails here: then offer it.
#[tokio::test]
async fn pulp_export_dry_run_still_writes_the_export() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let name = unique("it-dryrun");
    let (repo, remote) = synced_repo(&c, RepoType::File, &name, "file/PULP_MANIFEST", "mirror").await;
    let body = json!({ "name": name, "path": format!("{EXPORTS}/{name}"), "repositories": [repo] });
    let exporter = create(&c, "exporters/core/pulp/", body, "/exporters/").await;
    let export = ops::create(&c, &format!("{exporter}exports/"), &json!({ "dry_run": true }), "/exports/", &no_progress).await.unwrap();
    let e: Value = c.get(&export, &[]).await.unwrap();
    let files = e["output_file_info"].as_object().unwrap();
    assert!(files.keys().any(|f| f.ends_with(".tar")) && e["toc_info"]["file"].is_string(), "{e}");
    let ex: Value = c.get(&exporter, &[]).await.unwrap();
    assert_eq!(ex["last_export"].as_str(), Some(export.as_str()));
    cleanup(&c, &[&exporter, &repo, &remote]).await;
}

#[tokio::test]
async fn filesystem_export_of_a_file_publication() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::File;
    let name = unique("it-fsexport");
    let (repo, remote) = synced_repo(&c, t, &name, "file/PULP_MANIFEST", "mirror").await;
    let v = latest(&c, &repo).await;
    let publication = ops::create_publication(&c, t, &v, &json!({}), &no_progress).await.unwrap();
    let body = json!({ "name": name, "path": format!("{EXPORTS}/{name}"), "method": "write" });
    let exporter = create(&c, "exporters/core/filesystem/", body, "/exporters/").await;

    // Export (E) "publication" with an empty version number: the newest publication of the latest version.
    let q = [("repository_version", v.clone()), ("ordering", "-pulp_created".into()), ("limit", "1".into())];
    let newest: Page<Value> = c.list(&c.api(t.publications()), &q).await.unwrap();
    assert_eq!(newest.results[0]["pulp_href"].as_str(), Some(publication.as_str()));
    let exports = format!("{exporter}exports/");
    let export = ops::create(&c, &exports, &json!({ "publication": publication }), "/exports/", &no_progress).await.unwrap();
    let e: Value = c.get(&export, &[]).await.unwrap();
    assert_eq!(e["exported_resources"], json!([publication]));

    // The directory exists now: the task of a second export fails, so the exporter needs a new directory.
    for (body, error) in [(json!({ "publication": publication }), "existing data"), (json!({ "repository_version": v }), "File exists")] {
        let again = ops::create(&c, &exports, &body, "/exports/", &no_progress).await.unwrap_err();
        assert!(again.to_string().contains(error), "{again}");
    }
    let m = c.patch(&exporter, &json!({ "path": format!("{EXPORTS}/{name}-2") })).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    ops::create(&c, &exports, &json!({ "repository_version": v }), "/exports/", &no_progress).await.unwrap();

    cleanup(&c, &[&exporter, &publication, &repo, &remote]).await;
}
