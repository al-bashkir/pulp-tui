//! Deb repositories against a live Pulp with pulp_deb: mirror sync of the Debian fixture,
//! filtered structured copy, publication, distribution and served-content checks, and a Deb
//! Content View with each signing choice.

mod common;

use common::*;
use pulp_tui::cv::{CvService, Mode, base_path};
use pulp_tui::filters::{Selector, parse_rules};
use pulp_tui::pulp::client::Page;
use pulp_tui::pulp::models::{DebPackage, RepoVersion};
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{Capabilities, PulpClient, RepoType, deb};
use pulp_tui::store::{CvRepo, Store};
use pulp_tui::tui::actions::run;
use pulp_tui::tui::app::Modal;
use pulp_tui::tui::kinds::{Act, Kind};
use serde_json::{Value, json};
use std::sync::Arc;

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
    // Versions -> Compare lists every unit that Pulp counts: also the release records, indices and the Release
    // file (and asks for the types that this sync does not make, such as source packages).
    let all = ops::all_content_keys(&c, &Capabilities::default(), t, &v1).await.unwrap();
    let rv: RepoVersion = c.get(&v1, &[]).await.unwrap();
    assert_eq!(all.len() as u64, rv.content_summary.present_count(), "{all:?}");
    // Without the unit id that some keys end with.
    let records: Vec<&str> = all.iter().filter(|k| k.starts_with("release")).map(|k| k.split("  (").next().unwrap()).collect();
    let release = "release fixture (codename fixture, suite fixture)";
    assert_eq!(
        records,
        ["release architecture fixture/amd64", "release component fixture/main", "release file dists/fixture/Release", release]
    );
    assert!(all.contains("package fixture-beta-dbg_2.0-1_amd64"), "{all:?}");
    assert!(all.iter().any(|k| k.starts_with("package index dists/fixture/main/binary-amd64/Packages  (")), "{all:?}");

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
    let publication = ops::ensure_publication(&c, t, &v2, &json!({}), &no_progress).await.unwrap();
    let dist = ops::ensure_distribution(&c, t, &name, &name, Some(&publication), &json!({}), None, &no_progress).await.unwrap();
    let d: Value = c.get(&dist, &[]).await.unwrap();
    let base = d["base_url"].as_str().unwrap().to_string();
    let msg = ops::verify_served(&c, t, &base, &v2, &Value::Null).await.unwrap();
    assert!(msg.contains("matching digest"), "{msg}");
    let (status, index) = served(&c, &format!("{base}dists/fixture/main/binary-amd64/Packages")).await;
    let index = String::from_utf8_lossy(&index);
    assert_eq!(status, 200, "{index}");
    assert!(index.contains("Package: fixture-beta\n"), "{index}");
    assert!(!index.contains("fixture-beta-dbg"), "the filtered package is not served");

    // A publication made with the Deb test signing service is reused only for that service, and
    // clients get the detached signature next to the Release file.
    let service: Value =
        ops::find_by_name(&c, &c.api("signing-services/"), "pulp-tui-deb-test").await.unwrap().expect("Deb test signing service");
    let service = service["pulp_href"].as_str().unwrap();
    let signed = ops::ensure_publication(&c, t, &v1, &json!({ "signing_service": service }), &no_progress).await.unwrap();
    assert_eq!(
        ops::ensure_publication(&c, t, &v1, &json!({ "signing_service": service }), &no_progress).await.unwrap(),
        signed,
        "signed publication is reused"
    );
    let plain = ops::ensure_publication(&c, t, &v1, &json!({ "signing_service": null }), &no_progress).await.unwrap();
    assert_ne!(plain, signed, "a signed publication is not reused for the repository default");
    let signed_name = format!("{name}-signed");
    let signed_dist =
        ops::ensure_distribution(&c, t, &signed_name, &signed_name, Some(&signed), &json!({}), None, &no_progress).await.unwrap();
    let d: Value = c.get(&signed_dist, &[]).await.unwrap();
    let base = d["base_url"].as_str().unwrap();
    for file in ["Release", "Release.gpg", "InRelease"] {
        assert_eq!(served(&c, &format!("{base}dists/fixture/{file}")).await.0, 200, "{file} is served");
    }

    cleanup(&c, &[&dist, &signed_dist, &repo, &copy, &remote]).await;
}

/// A package removed together with its release records is not in the next publication.
#[tokio::test]
async fn a_removed_deb_package_is_no_longer_served() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::Deb;
    let name = unique("it-deb-rm");
    let url = format!("{}/deb/", upstream());
    let body = json!({ "name": name, "url": url, "policy": "immediate", "distributions": "fixture" });
    let remote = create(&c, t.remotes(), body, "/remotes/").await;
    let repo = create(&c, t.repositories(), json!({ "name": name, "remote": remote }), "/repositories/").await;
    let m = ops::sync(&c, t, &repo, None, "mirror").await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let v1 = latest(&c, &repo).await;
    let pkgs = deb::packages_in(&c, &v1).await.unwrap();
    let href = pkgs.into_iter().find(|p| p.package == "fixture-beta-dbg").expect("fixture-beta-dbg").pulp_href;
    let units = deb::with_release_records(&c, &v1, vec![href]).await.unwrap();
    let m = ops::remove_content(&c, &repo, units).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();

    let publication = ops::ensure_publication(&c, t, &latest(&c, &repo).await, &json!({}), &no_progress).await.unwrap();
    let dist = ops::ensure_distribution(&c, t, &name, &name, Some(&publication), &json!({}), None, &no_progress).await.unwrap();
    let d: Value = c.get(&dist, &[]).await.unwrap();
    let base = d["base_url"].as_str().unwrap();
    let (status, index) = served(&c, &format!("{base}dists/fixture/main/binary-amd64/Packages")).await;
    let index = String::from_utf8_lossy(&index);
    assert_eq!(status, 200, "{index}");
    assert!(index.contains("Package: fixture-beta\n"), "{index}");
    assert!(!index.contains("fixture-beta-dbg"), "the removed package is not served: {index}");

    // Restore v1 as latest (pulp_deb takes `base_version` too): with its release records, the package is served again.
    let m = ops::restore_version(&c, &repo, &v1).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let v3 = latest(&c, &repo).await;
    // pulp_deb removes the upstream metadata units from every new version; only a sync adds them.
    let synced = ops::all_content_keys(&c, &Capabilities::default(), t, &v1).await.unwrap();
    assert!(synced.iter().any(|k| k.starts_with("package index ")), "the sync stored the index: {synced:?}");
    let want = ops::content_keys_except(&c, &Capabilities::default(), t, &v1, &deb::NEW_VERSION_DROPS).await.unwrap();
    assert_eq!(ops::all_content_keys(&c, &Capabilities::default(), t, &v3).await.unwrap(), want);
    let publication = ops::create_publication(&c, t, &v3, &json!({}), &no_progress).await.unwrap();
    ops::ensure_distribution(&c, t, &name, &name, Some(&publication), &json!({}), None, &no_progress).await.unwrap();
    let index = served(&c, &format!("{base}dists/fixture/main/binary-amd64/Packages")).await.1;
    assert!(String::from_utf8_lossy(&index).contains("Package: fixture-beta-dbg\n"), "restored");
    cleanup(&c, &[&dist, &repo, &remote]).await;
}

/// A version without its Release units is served: pulp_deb publishes `dists/<distribution>/` for each
/// distribution of the release components and makes the Release file itself. Verify accepts it.
#[tokio::test]
async fn a_deb_version_without_release_units_is_served_and_verified() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::Deb;
    let name = unique("it-deb-norelease");
    let body = json!({ "name": name, "url": format!("{}/deb/", upstream()), "policy": "immediate", "distributions": "fixture" });
    let remote = create(&c, t.remotes(), body, "/remotes/").await;
    let repo = create(&c, t.repositories(), json!({ "name": name, "remote": remote }), "/repositories/").await;
    ops::settle(&c, ops::sync(&c, t, &repo, None, "mirror").await.unwrap(), &no_progress).await.unwrap();
    let units = release_units(&c, &latest(&c, &repo).await).await;
    assert_eq!(units.len(), 1, "{units:?}");
    ops::settle(&c, ops::remove_content(&c, &repo, units).await.unwrap(), &no_progress).await.unwrap();
    let v2 = latest(&c, &repo).await;
    assert_eq!(release_units(&c, &v2).await, Vec::<String>::new(), "{v2} has no Release");

    let publication = ops::create_publication(&c, t, &v2, &json!({ "structured": true }), &no_progress).await.unwrap();
    let dist = ops::ensure_distribution(&c, t, &name, &name, Some(&publication), &json!({}), None, &no_progress).await.unwrap();
    let d: Value = c.get(&dist, &[]).await.unwrap();
    let base = d["base_url"].as_str().unwrap().to_string();
    let now = ops::served_publication(&c, &Capabilities::default(), t, &d).await.unwrap().unwrap();
    let msg = ops::verify_served(&c, t, &base, &v2, &now).await.unwrap();
    assert!(msg.contains("matching digest"), "{msg}");
    let (status, release) = served(&c, &format!("{base}dists/fixture/Release")).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&release));
    cleanup(&c, &[&dist, &repo, &remote]).await;
}

/// Hrefs of the Release units (`content/deb/releases/`) of `version`.
async fn release_units(c: &PulpClient, version: &str) -> Vec<String> {
    let q = [("repository_version", version.to_string()), ("fields", "pulp_href".into())];
    let r: Vec<Value> = c.list_all(&c.api("content/deb/releases/"), &q, 1000).await.unwrap();
    r.into_iter().map(|r| r["pulp_href"].as_str().unwrap().to_string()).collect()
}

/// A distribution that serves a verbatim publication protects its version, as Pulp says on delete.
#[tokio::test]
async fn a_served_verbatim_publication_protects_its_version() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::Deb;
    let name = unique("it-deb-verbatim");
    let body = json!({ "name": name, "url": format!("{}/deb/", upstream()), "policy": "immediate", "distributions": "fixture" });
    let remote = create(&c, t.remotes(), body, "/remotes/").await;
    let repo = create(&c, t.repositories(), json!({ "name": name, "remote": remote }), "/repositories/").await;
    let m = ops::sync(&c, t, &repo, None, "mirror").await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let v1 = latest(&c, &repo).await;
    let verbatim = create(&c, deb::VERBATIM_PUBLICATIONS, json!({ "repository_version": v1 }), "/publications/").await;
    let dist = ops::ensure_distribution(&c, t, &name, &name, Some(&verbatim), &json!({}), None, &no_progress).await.unwrap();

    let caps = Capabilities::detect(&c).await.unwrap();
    let (pubs, by) = ops::version_protection(&c, &caps, t, &v1).await.unwrap();
    let by: Vec<_> = by.into_iter().map(|(d, why)| (d.name, why)).collect();
    assert_eq!((pubs.len(), by), (1, vec![(name.clone(), "pinned to a publication of this version")]));
    let err = c.delete(&v1).await.unwrap_err();
    assert!(err.status() == Some(400) && err.to_string().contains("currently being used to distribute content"), "{err}");
    cleanup(&c, &[&dist, &repo, &remote]).await;
}

/// A verbatim publication (P or c, kind verbatim) of a synced version serves the upstream Release as it is;
/// Distribute pins it, Verify passes, and its delete leaves the distribution serving nothing, as the impact says.
#[tokio::test]
async fn a_verbatim_publication_is_made_distributed_verified_and_deleted() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::Deb;
    let name = unique("it-deb-verbatim-pub");
    let body = json!({ "name": name, "url": format!("{}/deb/", upstream()), "policy": "immediate", "distributions": "fixture" });
    let remote = create(&c, t.remotes(), body, "/remotes/").await;
    let repo = create(&c, t.repositories(), json!({ "name": name, "remote": remote }), "/repositories/").await;
    let m = ops::sync(&c, t, &repo, None, "mirror").await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let v1 = latest(&c, &repo).await;

    let verbatim = json!({ "verbatim": true });
    let p = ops::create_publication(&c, t, &v1, &verbatim, &no_progress).await.unwrap();
    assert!(p.contains("/publications/deb/verbatim/"), "{p}");
    let pv: Value = c.get(&p, &[]).await.unwrap();
    assert_eq!(ops::publication_mode(t, &pv), verbatim, "a rollback keeps it verbatim");
    assert_eq!(ops::ensure_publication(&c, t, &v1, &verbatim, &no_progress).await.unwrap(), p, "reused");
    let dist = ops::ensure_distribution(&c, t, &name, &name, Some(&p), &json!({}), None, &no_progress).await.unwrap();
    let d: Value = c.get(&dist, &[]).await.unwrap();
    let base = d["base_url"].as_str().unwrap().to_string();
    let caps = Capabilities::detect(&c).await.unwrap();
    let now = ops::served_publication(&c, &caps, t, &d).await.unwrap().expect("a served publication");
    assert_eq!(now["pulp_href"], p.as_str());
    let msg = ops::verify_served(&c, t, &base, &v1, &now).await.unwrap();
    assert!(msg.contains("matching digest (4 packages)"), "{msg}");
    let upstream = std::fs::read(fixtures().join("upstream/deb/dists/fixture/Release")).unwrap();
    assert_eq!(served(&c, &format!("{base}dists/fixture/Release")).await, (200, upstream), "the upstream Release as it is");

    let pubs = ops::publications_where(&c, &caps, t, ("repository", &repo)).await.unwrap();
    let ds = ops::distributions(&c, t).await.unwrap();
    let ours = ds.iter().find(|x| x.name == name).unwrap();
    assert_eq!(ops::served_after_delete(ours, &p, &pubs), Some(None), "the delete impact: then serves nothing");
    let m = c.delete(&p).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let d: Value = c.get(&dist, &[]).await.unwrap();
    assert!(d["publication"].is_null(), "{d}");
    cleanup(&c, &[&dist, &repo, &remote]).await;
}

/// Upload (u) of a `.deb` into a release of an empty repository makes a version with the package and its
/// release records; the publication serves it under that release with the uploaded digest.
#[tokio::test]
async fn an_uploaded_deb_package_is_published_and_served() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::Deb;
    let name = unique("it-deb-upload");
    let repo = create(&c, t.repositories(), json!({ "name": name }), "/repositories/").await;
    let file = fixtures().join("upstream/deb/pool/main/f/fixture-alpha/fixture-alpha_1.0-1_amd64.deb");
    let release = deb::Upload::Package { distribution: Some("uploads".into()), component: Some("main".into()) };
    let up = deb::upload(&c, &release, &file, Some(&repo), true, &|_, _| {}, &no_progress).await.unwrap();
    assert!(up.content_href.contains("/content/deb/packages/"), "{}", up.content_href);
    let v1 = up.task.as_ref().and_then(|t| t.created_matching("/versions/")).expect("a new version");
    assert_eq!(v1, latest(&c, &repo).await);
    assert_eq!(ops::content_keys(&c, t, Some(&v1)).await.unwrap().into_iter().collect::<Vec<_>>(), ["fixture-alpha_1.0-1_amd64"]);
    let all = ops::all_content_keys(&c, &Capabilities::default(), t, &v1).await.unwrap();
    assert!(all.iter().any(|k| k.starts_with("release component uploads/main")), "{all:?}");

    let publication = ops::create_publication(&c, t, &v1, &json!({}), &no_progress).await.unwrap();
    let dist = ops::ensure_distribution(&c, t, &name, &name, Some(&publication), &json!({}), None, &no_progress).await.unwrap();
    let d: Value = c.get(&dist, &[]).await.unwrap();
    let base = d["base_url"].as_str().unwrap().to_string();
    let pv: Value = c.get(&publication, &[]).await.unwrap();
    let msg = ops::verify_served(&c, t, &base, &v1, &pv).await.unwrap();
    assert!(msg.contains("matching digest (1 packages)"), "{msg}");
    let (status, index) = served(&c, &format!("{base}dists/uploads/main/binary-amd64/Packages")).await;
    let index = String::from_utf8_lossy(&index);
    assert!(status == 200 && index.contains(&format!("SHA256: {}\n", up.sha256)), "{status} {index}");
    cleanup(&c, &[&dist, &repo]).await;
}

/// Upload (u) in the TUI: a `.udeb` (Kind auto) becomes an installer package, another file (Kind generic) generic
/// content at its relative path; each goes into a new version of the repository.
#[tokio::test]
async fn the_tui_uploads_a_udeb_and_generic_content_as_their_types() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::Deb;
    let name = unique("it-deb-kinds");
    let repo = create(&c, t.repositories(), json!({ "name": name }), "/repositories/").await;
    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Repos(t, false);
    let (udeb, txt) = (fixtures().join("deb-upload/fixture-installer_1.0_amd64.udeb"), fixtures().join("files/one.txt"));
    let rel = format!("docs/{name}.txt");
    let forms =
        [vec![("path", udeb.to_str().unwrap())], vec![("path", txt.to_str().unwrap()), ("kind", "generic"), ("relative_path", &rel)]];
    for values in forms {
        run(&mut app, Act::Upload, json!({ "pulp_href": repo, "name": name }));
        fill(&mut app, &values);
        settle(&mut app, &mut rx).await;
    }
    let uploaded: Vec<String> = texts(&app).into_iter().filter(|n| n.starts_with("uploaded ")).collect();
    assert!(uploaded.len() == 2 && uploaded[1].ends_with(": new version 2"), "{:?}", texts(&app));
    let all = ops::all_content_keys(&c, &Capabilities::default(), t, &latest(&c, &repo).await).await.unwrap();
    assert!(all.contains("installer package fixture-installer_1.0_amd64"), "{all:?}");
    assert!(all.iter().any(|k| k.starts_with(&format!("generic content {rel}  ("))), "{all:?}");
    cleanup(&c, &[&repo]).await;
}

/// An operator repository with its own signing service: a publication without one is signed by the
/// repository's service, and after the edit form clears it (`signing_service: null`) it is not.
#[tokio::test]
async fn deb_repository_signs_with_its_own_service() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::Deb;
    let name = unique("it-deb-sign");
    let service: Value =
        ops::find_by_name(&c, &c.api("signing-services/"), "pulp-tui-deb-test").await.unwrap().expect("Deb test signing service");
    let url = format!("{}/deb/", upstream());
    let body = json!({ "name": name, "url": url, "policy": "immediate", "distributions": "fixture" });
    let remote = create(&c, t.remotes(), body, "/remotes/").await;
    // The body of the create form with a signing service picked.
    let body = json!({ "name": name, "remote": remote, "signing_service": service["pulp_href"], "publish_upstream_release_fields": true });
    let repo = create(&c, t.repositories(), body, "/repositories/").await;
    let m = ops::sync(&c, t, &repo, None, "mirror").await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let v1 = latest(&c, &repo).await;

    // Publish (P) sends no signing service: the repository's own one signs.
    let mut dists = vec![];
    for (suffix, expect) in [("signed", true), ("plain", false)] {
        let publication = ops::create_publication(&c, t, &v1, &json!({}), &no_progress).await.unwrap();
        let n = format!("{name}-{suffix}");
        let dist = ops::ensure_distribution(&c, t, &n, &n, Some(&publication), &json!({}), None, &no_progress).await.unwrap();
        let d: Value = c.get(&dist, &[]).await.unwrap();
        assert_eq!(signed(&c, d["base_url"].as_str().unwrap()).await, expect, "{suffix}");
        dists.push(dist);
        // Edit cleared the signing service.
        let m = c.patch(&repo, &json!({ "signing_service": null })).await.unwrap();
        ops::settle(&c, m, &no_progress).await.unwrap();
    }
    cleanup(&c, &[&dists[0], &dists[1], &repo, &remote]).await;
}

fn quiet(_: String) {}

/// What the distribution of `cv` in `env` serves at (ends with `/`).
async fn served_base(c: &PulpClient, store: &Store, env: i64, cv: i64) -> String {
    let d: Value = c.get(&store.binding_repos(env, cv).await.unwrap().remove(0).distribution, &[]).await.unwrap();
    d["base_url"].as_str().unwrap().to_string()
}

async fn signed(c: &PulpClient, base: &str) -> bool {
    let (gpg, inline) =
        (served(c, &format!("{base}dists/fixture/Release.gpg")).await.0, served(c, &format!("{base}dists/fixture/InRelease")).await.0);
    assert_eq!(served(c, &format!("{base}dists/fixture/Release")).await.0, 200);
    assert_eq!(gpg, inline, "Release.gpg and InRelease come together");
    gpg == 200
}

/// A Deb Content View: a filtered snapshot, each signing choice, a resumed publish that keeps its choice,
/// a composite that borrows the rows, and a per-release key of the source.
#[tokio::test]
async fn deb_content_view() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::Deb;
    let id = unique("cvdeb");
    let url = format!("{}/deb/", upstream());
    let body = json!({ "name": id, "url": url, "policy": "immediate", "distributions": "fixture" });
    let remote = create(&c, t.remotes(), body, "/remotes/").await;
    let repo = create(&c, t.repositories(), json!({ "name": id, "remote": remote }), "/repositories/").await;
    let m = ops::sync(&c, t, &repo, None, "mirror").await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();

    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let profile = c.profile().to_string();
    let svc = CvService { client: c.clone(), store: store.clone(), scope: profile.clone(), actor: "test@live".into(), lock: None };
    let cv = store.create_cv(&profile, &id, "").await.unwrap();
    let apt =
        CvRepo { repo_href: repo.clone(), repo_name: "apt".into(), repo_type: t, filters: parse_rules("exclude name=*-dbg", t).unwrap() };
    store.set_cv_repo(cv, &apt).await.unwrap();
    let preview = svc.preview(cv, None).await.unwrap().join("\n");
    assert!(preview.contains("+ fixture-alpha_1.0-1_amd64"), "{preview}");
    assert!(!preview.contains("fixture-beta-dbg"), "{preview}");

    // Signed with the test service: Library is bound and verified, then Dev serves the signature without the excluded package.
    let service: Value =
        ops::find_by_name(&c, &c.api("signing-services/"), "pulp-tui-deb-test").await.unwrap().expect("Deb test signing service");
    let service = service["pulp_href"].as_str().unwrap().to_string();
    let (v1, _) = svc.publish(cv, "signed", &service, &quiet, &no_progress).await.unwrap();
    let lib = store.ensure_library(&profile).await.unwrap();
    assert_eq!(svc.verify(cv, lib).await.unwrap(), Vec::<String>::new());
    let dev = store.create_env(&profile, "Dev", lib, false).await.unwrap();
    svc.promote(cv, v1.id, dev, Mode::Promote, &quiet, &no_progress).await.unwrap();
    assert_eq!(svc.verify(cv, dev).await.unwrap(), Vec::<String>::new());
    let dev_base = served_base(&c, &store, dev, cv).await;
    assert!(dev_base.contains(&base_path(&id, "dev", "apt")), "{dev_base}");
    assert!(signed(&c, &dev_base).await, "Dev serves the signed Release");
    let (status, index) = served(&c, &format!("{dev_base}dists/fixture/main/binary-amd64/Packages")).await;
    let index = String::from_utf8_lossy(&index);
    assert_eq!(status, 200, "{index}");
    assert!(index.contains("Package: fixture-alpha\n") && !index.contains("fixture-beta-dbg"), "{index}");

    // Unsigned: the derived repository loses its key, and Library serves no signature.
    let (v2, _) = svc.publish(cv, "unsigned", "none", &quiet, &no_progress).await.unwrap();
    assert_eq!(svc.verify(cv, lib).await.unwrap(), Vec::<String>::new());
    assert!(!signed(&c, &served_base(&c, &store, lib, cv).await).await, "v{} is unsigned", v2.number);
    assert!(signed(&c, &dev_base).await, "Dev still serves v1");

    // A publish that stopped after its checkpoint: Resume reads the signing choice from it.
    let v3 = store.begin_version(cv, "resume", &store.cv_repos(cv).await.unwrap()).await.unwrap();
    for mut vr in store.version_repos(v3.id).await.unwrap() {
        vr.source_version = Some(latest(&c, &vr.repo().repo_href).await);
        vr.signing = service.clone();
        store.save_version_repo(&vr).await.unwrap();
    }
    let (v3, _) = svc.resume(v3.id, &quiet, &no_progress).await.unwrap();
    assert_eq!(v3.state, "published");
    assert_eq!(svc.verify(cv, lib).await.unwrap(), Vec::<String>::new());
    assert!(signed(&c, &served_base(&c, &store, lib, cv).await).await, "the resumed version is signed");

    // A composite borrows the rows of the component version: nothing to sign or copy.
    let comp = store.create_composite(&profile, &format!("{id}-all"), "").await.unwrap();
    store.set_component(comp, cv, None).await.unwrap();
    let (c1, _) = svc.publish(comp, "", "", &quiet, &no_progress).await.unwrap();
    assert_eq!(c1.state, "published");
    assert_eq!(svc.verify(comp, lib).await.unwrap(), Vec::<String>::new());
    assert!(signed(&c, &served_base(&c, &store, lib, comp).await).await, "the composite serves the publication of v3");

    // A per-release key of the source: same as source copies it, and none removes it again
    // (pulp_deb changes only the per-release keys that a PATCH names).
    let m = c.patch(&repo, &json!({ "signing_service_release_overrides": { "fixture": service } })).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    svc.publish(cv, "release key", "", &quiet, &no_progress).await.unwrap();
    assert!(signed(&c, &served_base(&c, &store, lib, cv).await).await, "the per-release key of the source signs");
    svc.publish(cv, "unsigned again", "none", &quiet, &no_progress).await.unwrap();
    let base = served_base(&c, &store, lib, cv).await;
    assert_eq!(served(&c, &format!("{base}dists/fixture/Release.gpg")).await.0, 404, "none removes the per-release key");
    assert!(!signed(&c, &base).await);

    svc.delete_view(comp, &quiet, &no_progress).await.unwrap();
    svc.delete_view(cv, &quiet, &no_progress).await.unwrap();
    cleanup(&c, &[&repo, &remote]).await;
}

/// A rollback of a distribution that serves a simple publication (clients use `dists/default/`) gets a
/// simple publication too: a structured one of the target version is not reused, and verify probes the
/// simple paths.
#[tokio::test]
async fn deb_rollback_keeps_a_simple_publication() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::Deb;
    let name = unique("it-deb-simple");
    let url = format!("{}/deb/", upstream());
    let body = json!({ "name": name, "url": url, "policy": "immediate", "distributions": "fixture" });
    let remote = create(&c, t.remotes(), body, "/remotes/").await;
    let repo = create(&c, t.repositories(), json!({ "name": name, "remote": remote }), "/repositories/").await;
    ops::settle(&c, ops::sync(&c, t, &repo, None, "mirror").await.unwrap(), &no_progress).await.unwrap();
    let v1 = latest(&c, &repo).await;
    let dbg = deb::packages_in(&c, &v1).await.unwrap().into_iter().find(|p| p.package.ends_with("-dbg")).unwrap();
    ops::settle(&c, ops::remove_content(&c, &repo, vec![dbg.pulp_href]).await.unwrap(), &no_progress).await.unwrap();
    let v2 = latest(&c, &repo).await;

    // v1 has a structured publication; the distribution serves a simple-only one of v2.
    let structured = ops::create_publication(&c, t, &v1, &json!({}), &no_progress).await.unwrap();
    let simple = ops::create_publication(&c, t, &v2, &json!({ "simple": true, "structured": false }), &no_progress).await.unwrap();
    let dist = ops::ensure_distribution(&c, t, &name, &name, Some(&simple), &json!({}), None, &no_progress).await.unwrap();
    let d: Value = c.get(&dist, &[]).await.unwrap();
    let base = d["base_url"].as_str().unwrap().to_string();
    let now = ops::served_publication(&c, &Capabilities::default(), t, &d).await.unwrap().unwrap();
    let msg = ops::verify_served(&c, t, &base, &v2, &now).await.unwrap();
    assert!(msg.contains("pool/") && msg.contains("matching digest"), "{msg}");

    // Roll back to v1 in the served mode.
    let mode = ops::publication_mode(t, &now);
    assert_eq!((mode["simple"].clone(), mode["structured"].clone()), (json!(true), json!(false)), "{mode}");
    let p = ops::ensure_publication(&c, t, &v1, &mode, &no_progress).await.unwrap();
    assert_ne!(p, structured, "the structured publication of v1 is not reused");
    ops::ensure_distribution(&c, t, &name, &name, Some(&p), &json!({}), None, &no_progress).await.unwrap();
    let pv: Value = c.get(&p, &[]).await.unwrap();
    assert_eq!(pv["simple"], true, "{pv}");
    let msg = ops::verify_served(&c, t, &base, &v1, &pv).await.unwrap();
    assert!(msg.contains("matching digest"), "{msg}");
    let (status, _) = served(&c, &format!("{base}dists/default/Release")).await;
    assert_eq!(status, 200, "apt clients of the simple layout still get the Release file");
    // A structured-only check of this publication fails: it serves no dists/fixture/.
    let e = ops::verify_served(&c, t, &base, &v1, &json!({})).await.unwrap_err().to_string();
    assert!(e.contains("dists/fixture/Release returned HTTP 404"), "{e}");
    cleanup(&c, &[&dist, &repo, &remote]).await;
}

/// Repository Delete through the TUI (audit F05): the impact counts the APT and the verbatim publication and names
/// both distributions; after the delete, Pulp has deleted both publications and neither distribution serves.
#[tokio::test]
async fn deb_repository_delete_impact_names_the_distribution_of_a_verbatim_publication() {
    if !live_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::Deb;
    let name = unique("it-deb-del");
    let body = json!({ "name": name, "url": format!("{}/deb/", upstream()), "policy": "immediate", "distributions": "fixture" });
    let remote = create(&c, t.remotes(), body, "/remotes/").await;
    let repo = create(&c, t.repositories(), json!({ "name": name, "remote": remote }), "/repositories/").await;
    ops::settle(&c, ops::sync(&c, t, &repo, None, "mirror").await.unwrap(), &no_progress).await.unwrap();
    let v1 = latest(&c, &repo).await;
    let apt = ops::create_publication(&c, t, &v1, &json!({}), &no_progress).await.unwrap();
    let verbatim = ops::create_publication(&c, t, &v1, &json!({ "verbatim": true }), &no_progress).await.unwrap();
    let (apt_dist, verbatim_dist) = (format!("{name}-apt"), format!("{name}-verbatim"));
    let d1 = ops::ensure_distribution(&c, t, &apt_dist, &apt_dist, Some(&apt), &json!({}), None, &no_progress).await.unwrap();
    let d2 =
        ops::ensure_distribution(&c, t, &verbatim_dist, &verbatim_dist, Some(&verbatim), &json!({}), None, &no_progress).await.unwrap();

    let (mut app, mut rx) = app().await;
    app.list.kind = Kind::Repos(t, false);
    run(&mut app, Act::Delete, json!({ "pulp_href": repo, "name": name }));
    settle(&mut app, &mut rx).await;
    let Some(Modal::Confirm { lines, typed, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm: {:?}", texts(&app)) };
    assert_eq!(typed.as_deref(), Some(name.as_str()));
    assert_eq!(lines[0], format!("Deletes repository {name} with all 2 versions and 2 publications."), "{lines:?}");
    assert!(lines.contains(&"WARNING: 2 distribution(s) serve it and stop serving:".to_string()), "{lines:?}");
    assert!(lines.iter().any(|l| l.starts_with(&format!("  {verbatim_dist} ("))), "{lines:?}");
    yes(&mut app);
    settle(&mut app, &mut rx).await;
    settle_submitted(&c, &app).await;
    for d in [&d1, &d2] {
        let d: Value = c.get(d, &[]).await.unwrap();
        assert!(d["publication"].is_null(), "Pulp deleted the publication with the repository: {d}");
    }
    assert!(c.get::<Value>(&verbatim, &[]).await.is_err(), "the verbatim publication is gone");
    cleanup(&c, &[&d1, &d2, &remote]).await;
}
