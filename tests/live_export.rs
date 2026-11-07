//! Disconnected export and import on one live Pulp: a Pulp export of a File repository, its
//! import into another repository through an importer mapping, and a filesystem export of a
//! File publication, done with the requests of the Exporters and Importers screens. Each run
//! uses its own directories below the export path.

mod common;

use common::*;
use crossterm::event::{KeyCode, KeyEvent};
use pulp_tui::pulp::client::Page;
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{PulpClient, RepoType};
use pulp_tui::tui::actions::run;
use pulp_tui::tui::app::{App, Modal, Msg};
use pulp_tui::tui::kinds::{Act, Kind, short};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

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

/// Import (I) of the `.tar` of an export without chunks (audit F28): the check and the import take it as `path`.
#[tokio::test]
async fn pulp_import_of_the_export_tar_by_its_path() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let name = unique("it-importtar");
    let (repo, remote) = synced_repo(&c, RepoType::File, &name, "file/PULP_MANIFEST", "mirror").await;
    let body = json!({ "name": name, "path": format!("{EXPORTS}/{name}"), "repositories": [repo] });
    let exporter = create(&c, "exporters/core/pulp/", body, "/exporters/").await;
    let export = ops::create(&c, &format!("{exporter}exports/"), &json!({}), "/exports/", &no_progress).await.unwrap();
    let e: Value = c.get(&export, &[]).await.unwrap();
    let tar = e["output_file_info"].as_object().unwrap().keys().find(|f| f.ends_with(".tar")).unwrap().clone();
    let copy = format!("{name}-copy");
    let dst = create(&c, RepoType::File.repositories(), json!({ "name": copy }), "/repositories/").await;
    let mapping = json!({ name.clone(): copy });
    let importer = create(&c, "importers/core/pulp/", json!({ "name": name, "repo_mapping": mapping }), "/importers/").await;
    let (ok, lines) = ops::import_check(&c, &tar, &mapping).await.unwrap();
    assert!(ok && lines[0] == "path: valid", "{lines:?}");
    let (group, lines) = ops::pulp_import(&c, &importer, &tar, &|_| {}).await.unwrap();
    assert!(group.is_final() && group.outcome() == "completed", "{lines:?}");
    let files = async |r: &str| {
        c.get::<Value>(&latest(&c, r).await, &[]).await.unwrap()["content_summary"]["present"]["file.file"]["count"].clone()
    };
    assert_eq!((files(&dst).await, files(&repo).await), (json!(3), json!(3)), "the copy has the 3 exported files");
    let imports: Page<Value> = c.list(&format!("{importer}imports/"), &[]).await.unwrap();
    assert_eq!(imports.results[0]["params"]["path"].as_str(), Some(tar.as_str()));
    cleanup(&c, &[&importer, &exporter, &dst, &repo, &remote]).await;
}

/// A Content View snapshot (a repository with `pulp_tui_cv`) is offered, marked, by the exporter's repository picker,
/// and the exporter made with it in the TUI exports it.
#[tokio::test]
async fn a_content_view_snapshot_can_be_exported() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let name = unique("it-export-snap");
    let (repo, remote) = synced_repo(&c, RepoType::File, &name, "file/PULP_MANIFEST", "mirror").await;
    let m = c.patch(&repo, &json!({ "pulp_labels": { "pulp_tui_cv": "snapview", "pulp_tui_db": "it" } })).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();

    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::PulpExporters;
    run(&mut app, Act::Create, Value::Null);
    let Some(Modal::Form { form, .. }) = &mut app.modal else { panic!("no form: {:?}", texts(&app)) };
    form.focus = form.fields.iter().position(|f| f.key == "repositories").unwrap();
    app.handle_key(KeyEvent::from(KeyCode::Enter));
    settle(&mut app, &mut rx).await;
    let Some(Modal::Form { form, .. }) = &app.modal else { panic!("no form: {:?}", texts(&app)) };
    let offered = form.visible().iter().find(|i| i.value == repo).map(|i| i.label.clone());
    assert_eq!(offered, Some(format!("{name} (snapshot of Content View snapview)")));

    let dir = format!("{EXPORTS}/{name}");
    fill(&mut app, &[("name", &name), ("path", &dir), ("repositories", &repo)]);
    settle(&mut app, &mut rx).await;
    let exporter: Value = ops::find_by_name(&c, &c.api("exporters/core/pulp/"), &name).await.unwrap().expect("exporter made");
    let exporter = exporter["pulp_href"].as_str().unwrap().to_string();
    let export = ops::create(&c, &format!("{exporter}exports/"), &json!({}), "/exports/", &no_progress).await.unwrap();
    let e: Value = c.get(&export, &[]).await.unwrap();
    assert_eq!(e["exported_resources"], json!([latest(&c, &repo).await]));
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

/// The export records of `exporter`.
async fn exports_of(c: &PulpClient, exporter: &str) -> Vec<Value> {
    c.list::<Value>(&format!("{exporter}exports/"), &[]).await.unwrap().results
}

/// Delete (d) of the exporter or importer row `r` of the open list: the impact (its first line), the typed name, then
/// the request; the object is gone in Pulp and from the reloaded list.
async fn tui_delete(app: &mut App, rx: &mut UnboundedReceiver<Msg>, r: Value, first: &str) {
    let (href, name) = (r["pulp_href"].as_str().unwrap().to_string(), r["name"].as_str().unwrap().to_string());
    run(app, Act::Delete, r);
    settle(app, rx).await; // the impact is read before the confirmation
    let (lines, typed) = yes(app);
    assert_eq!((typed.as_deref(), lines[0].as_str()), (Some(name.as_str()), first));
    settle_tasks(app, rx).await;
    assert!(row(app, "name", &name).is_none(), "the list reloads without it: {:?}", texts(app));
    assert!(client().get::<Value>(&href, &[]).await.unwrap_err().is_not_found(), "{:?}", texts(app));
}

/// The Pulp Exporters and Importers screens through the TUI, on one server: Create an exporter, Edit its directory
/// (a task), Export (E), list and Delete the export record; Create an importer that maps the repository to a
/// decoy, Edit the mapping to the copy, Import (I): the check, the confirmation, the task group; the copy gets the
/// exported files and the decoy nothing; list the imports, Delete the importer and the exporter.
#[tokio::test]
async fn pulp_exporter_and_importer_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let name = unique("it-tui-export");
    let (repo, remote) = synced_repo(&c, RepoType::File, &name, "file/PULP_MANIFEST", "mirror").await;
    let (copy, decoy) = (format!("{name}-copy"), format!("{name}-decoy"));
    let dst = create(&c, RepoType::File.repositories(), json!({ "name": copy }), "/repositories/").await;
    let other = create(&c, RepoType::File.repositories(), json!({ "name": decoy }), "/repositories/").await;
    let (dir, dir2) = (format!("{EXPORTS}/{name}"), format!("{EXPORTS}/{name}-2"));
    let (mut app, mut rx) = app().await;

    // Exporter: Create, then Edit of its directory (Pulp updates an exporter in a task).
    open_list(&mut app, &mut rx, Kind::PulpExporters, &name).await;
    run(&mut app, Act::Create, Value::Null);
    fill(&mut app, &[("name", &name), ("path", &dir), ("repositories", &repo)]);
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("create exporter {name}: done"));
    let r = row(&app, "name", &name).unwrap_or_else(|| panic!("the list reloads with it: {:?}", app.list.items));
    assert_eq!((r["path"].as_str(), &r["repositories"]), (Some(dir.as_str()), &json!([repo])));
    let exporter = r["pulp_href"].as_str().unwrap().to_string();
    run(&mut app, Act::Edit, r);
    fill(&mut app, &[("path", &dir2)]);
    settle_tasks(&mut app, &mut rx).await;
    shown(&app, &format!("task completed: update exporter {name}"));
    let r = row(&app, "name", &name).unwrap();
    assert_eq!((r["path"].as_str(), &r["repositories"]), (Some(dir2.as_str()), &json!([repo])), "the list reloads");

    // Export (E): a full export of the latest version; the result names the TOC file.
    run(&mut app, Act::Export, r.clone());
    settle(&mut app, &mut rx).await; // the repositories and the last export are read before the form
    fill(&mut app, &[]);
    settle(&mut app, &mut rx).await;
    let Some(Modal::Text { title, lines, .. }) = app.modal.take() else { panic!("no export result: {:?}", texts(&app)) };
    let export = exports_of(&c, &exporter).await.into_iter().next().unwrap_or_else(|| panic!("no export: {lines:?}"));
    let toc = export["toc_info"]["file"].as_str().unwrap().to_string();
    assert_eq!((title, lines[0].clone()), (format!("Export of {name}"), format!("TOC: {toc}")));
    assert!(toc.starts_with(&format!("{dir2}/")), "{toc}");
    assert_eq!(export["exported_resources"], json!([latest(&c, &repo).await]));

    // Importer: Create with the decoy mapping, then Edit the mapping to the copy.
    open_list(&mut app, &mut rx, Kind::Importers, &name).await;
    run(&mut app, Act::Create, Value::Null);
    fill(&mut app, &[("name", &name), ("repo_mapping", &format!("{name}={decoy}"))]);
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("create importer {name}: done"));
    let i = row(&app, "name", &name).unwrap_or_else(|| panic!("the list reloads with it: {:?}", app.list.items));
    assert_eq!(i["repo_mapping"], json!({ name.clone(): decoy }));
    run(&mut app, Act::Edit, i);
    fill(&mut app, &[("repo_mapping", &format!("{name}={copy}"))]);
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("update importer {name}: done"));
    let i = row(&app, "name", &name).unwrap();
    assert_eq!(i["repo_mapping"], json!({ name.clone(): copy }), "the list reloads");

    // Import (I): the check is the preview; the confirmation names the mapping that Pulp has.
    run(&mut app, Act::Import, i.clone());
    fill(&mut app, &[("toc", &toc)]);
    settle(&mut app, &mut rx).await;
    let (lines, typed) = yes(&mut app);
    assert_eq!(typed, None, "a plain confirmation on a profile that is not production");
    assert!(lines.contains(&format!("  {name} -> {copy}")), "{lines:?}");
    settle_tasks(&mut app, &mut rx).await;
    let Some(Modal::Text { title, lines, .. }) = app.modal.take() else { panic!("no import result: {:?}", texts(&app)) };
    assert_eq!(title, format!("Import of {toc}"), "{lines:?}");
    let files = async |r: &str| {
        let v: Value = c.get(&latest(&c, r).await, &[]).await.unwrap();
        v["content_summary"]["present"]["file.file"]["count"].clone()
    };
    assert_eq!((files(&dst).await, files(&other).await), (json!(3), Value::Null), "the copy has the 3 exported files");
    run(&mut app, Act::Imports, i.clone());
    settle(&mut app, &mut rx).await;
    assert_eq!(app.list.items.len(), 1, "Imports lists the import: {:?}", app.list.error);
    app.back();
    settle(&mut app, &mut rx).await;
    let i = row(&app, "name", &name).unwrap();
    tui_delete(&mut app, &mut rx, i, &format!("Delete importer {name} and the records of its 1 import(s).")).await;

    // Exports (o) lists the export; Delete removes its record (the files stay); then Delete the exporter (a task).
    open_list(&mut app, &mut rx, Kind::PulpExporters, &name).await;
    let r = row(&app, "name", &name).unwrap();
    run(&mut app, Act::Exports, r);
    settle(&mut app, &mut rx).await;
    let id = short(export["pulp_href"].as_str().unwrap());
    let e = row(&app, "pulp_href", export["pulp_href"].as_str().unwrap()).unwrap_or_else(|| panic!("{:?}", app.list.items));
    run(&mut app, Act::Delete, e);
    let (lines, typed) = yes(&mut app);
    assert_eq!((typed.as_deref(), lines[0].clone()), (Some(id.as_str()), format!("Delete the record of export {id} (TOC {toc}).")));
    settle(&mut app, &mut rx).await;
    assert!(app.list.items.is_empty() && exports_of(&c, &exporter).await.is_empty(), "{:?}", texts(&app));
    app.back();
    settle(&mut app, &mut rx).await;
    let r = row(&app, "name", &name).unwrap();
    tui_delete(&mut app, &mut rx, r, &format!("Delete exporter {name} and the records of its 0 export(s).")).await;
    shown(&app, &format!("task completed: delete exporter {name}"));
    cleanup(&c, &[&dst, &other, &repo, &remote]).await;
}

/// The Filesystem Exporters screen through the TUI: Create (method write), Export (E) of the newest publication, Edit
/// of the directory (a task; Pulp needs a new one for each export), Export of the repository version, Delete.
#[tokio::test]
async fn filesystem_exporter_through_the_tui() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::File;
    let name = unique("it-tui-fsexport");
    let (repo, remote) = synced_repo(&c, t, &name, "file/PULP_MANIFEST", "mirror").await;
    let v = latest(&c, &repo).await;
    let publication = ops::create_publication(&c, t, &v, &json!({}), &no_progress).await.unwrap();
    let (dir, dir2) = (format!("{EXPORTS}/{name}"), format!("{EXPORTS}/{name}-2"));
    let (mut app, mut rx) = app().await;
    open_list(&mut app, &mut rx, Kind::FsExporters, &name).await;
    run(&mut app, Act::Create, Value::Null);
    fill(&mut app, &[("name", &name), ("path", &dir), ("method", "write")]);
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("create exporter {name}: done"));
    let r = row(&app, "name", &name).unwrap_or_else(|| panic!("the list reloads with it: {:?}", app.list.items));
    assert_eq!((r["path"].as_str(), r["method"].as_str()), (Some(dir.as_str()), Some("write")));
    let exporter = r["pulp_href"].as_str().unwrap().to_string();

    // Export (E) "publication", version empty: the newest publication of the latest version.
    run(&mut app, Act::Export, r.clone());
    fill(&mut app, &[("repo", &repo), ("what", "publication")]);
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("export {name}: publication written to {dir}"));
    let made = exports_of(&c, &exporter).await;
    assert_eq!(made.iter().map(|e| e["exported_resources"].clone()).collect::<Vec<_>>(), [json!([publication])]);

    // Edit the directory, then Export (E) "repository version".
    run(&mut app, Act::Edit, r);
    fill(&mut app, &[("path", &dir2)]);
    settle_tasks(&mut app, &mut rx).await;
    shown(&app, &format!("task completed: update exporter {name}"));
    let r = row(&app, "name", &name).unwrap();
    assert_eq!(r["path"].as_str(), Some(dir2.as_str()), "the list reloads");
    run(&mut app, Act::Export, r.clone());
    fill(&mut app, &[("repo", &repo), ("what", "repository version")]);
    settle(&mut app, &mut rx).await;
    shown(&app, &format!("export {name}: repository version written to {dir2}"));
    let mut made: Vec<Value> = exports_of(&c, &exporter).await.into_iter().map(|e| e["exported_resources"].clone()).collect();
    made.sort_by_key(Value::to_string);
    assert_eq!(made, [json!([publication]), json!([v])]);

    tui_delete(&mut app, &mut rx, r, &format!("Delete exporter {name} and the records of its 2 export(s).")).await;
    shown(&app, &format!("task completed: delete exporter {name}"));
    cleanup(&c, &[&publication, &repo, &remote]).await;
}
