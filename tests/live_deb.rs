//! Deb repositories against a live Pulp with pulp_deb: mirror sync of the Debian fixture,
//! filtered structured copy, publication, distribution and served-content checks, and a Deb
//! Content View with each signing choice.

mod common;

use common::*;
use pulp_tui::cv::{CvService, Mode, base_path};
use pulp_tui::filters::{Selector, parse_rules};
use pulp_tui::pulp::client::Page;
use pulp_tui::pulp::models::DebPackage;
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{PulpClient, RepoType, deb};
use pulp_tui::store::{CvRepo, Store};
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
    let publication = ops::ensure_publication(&c, t, &v2, None, &no_progress).await.unwrap();
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

    // A publication made with the Deb test signing service is reused only for that service, and
    // clients get the detached signature next to the Release file.
    let service: Value =
        ops::find_by_name(&c, &c.api("signing-services/"), "pulp-tui-deb-test").await.unwrap().expect("Deb test signing service");
    let service = service["pulp_href"].as_str().unwrap();
    let signed = ops::ensure_publication(&c, t, &v1, Some(service), &no_progress).await.unwrap();
    assert_eq!(ops::ensure_publication(&c, t, &v1, Some(service), &no_progress).await.unwrap(), signed, "signed publication is reused");
    let plain = ops::ensure_publication(&c, t, &v1, None, &no_progress).await.unwrap();
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
    let v1 = svc.publish(cv, "signed", &service, &quiet, &no_progress).await.unwrap();
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
    let v2 = svc.publish(cv, "unsigned", "none", &quiet, &no_progress).await.unwrap();
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
    let v3 = svc.resume(v3.id, &quiet, &no_progress).await.unwrap();
    assert_eq!(v3.state, "published");
    assert_eq!(svc.verify(cv, lib).await.unwrap(), Vec::<String>::new());
    assert!(signed(&c, &served_base(&c, &store, lib, cv).await).await, "the resumed version is signed");

    // A composite borrows the rows of the component version: nothing to sign or copy.
    let comp = store.create_composite(&profile, &format!("{id}-all"), "").await.unwrap();
    store.set_component(comp, cv, None).await.unwrap();
    let c1 = svc.publish(comp, "", "", &quiet, &no_progress).await.unwrap();
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
