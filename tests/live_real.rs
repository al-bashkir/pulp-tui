//! Live tests against real upstream repositories, synced on demand: Debian main (Deb) and Rocky 9
//! BaseOS and AppStream (RPM). Packages stream from upstream when Pulp serves them. They need
//! internet from the Pulp container and run only with `PULP_TUI_LIVE=1` and `PULP_TUI_REAL=1`
//! (`tests/run-live.sh real`). The Debian main sync takes many minutes.

mod common;

use common::*;
use pulp_tui::cv::CvService;
use pulp_tui::filters::parse_rules;
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{PulpClient, RepoType, deb, rpm};
use pulp_tui::store::{CvRepo, Store};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Instant;

/// The fields that `rpm::packages_in` sends.
const RPM_FIELDS: &str = "pulp_href,name,epoch,version,release,arch,location_href,sha256";

fn real_enabled() -> bool {
    if !live_enabled() {
        return false;
    }
    if std::env::var("PULP_TUI_REAL").as_deref() == Ok("1") {
        true
    } else {
        eprintln!("skipping real-upstream test: set PULP_TUI_REAL=1 (the Pulp container needs internet)");
        false
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.into())
}

fn quiet(_: String) {}

/// Creates the remote (body `remote`) and a repository, and syncs it additively.
/// Returns (repository, remote, latest version).
async fn synced(c: &PulpClient, t: RepoType, name: &str, remote: Value) -> (String, String, String) {
    let remote = create(c, t.remotes(), remote, "/remotes/").await;
    let repo = create(c, t.repositories(), json!({ "name": name, "remote": remote }), "/repositories/").await;
    let started = Instant::now();
    let m = ops::sync(c, t, &repo, None, "additive").await.unwrap();
    ops::settle(c, m, &no_progress).await.unwrap();
    eprintln!("{name}: synced in {:.0} s", started.elapsed().as_secs_f64());
    let v = latest(c, &repo).await;
    (repo, remote, v)
}

/// A Content View with one repository, its service locked as the TUI does, and the Library id.
async fn content_view(c: &PulpClient, name: &str, repo: &CvRepo) -> (CvService, i64, i64) {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let profile = c.profile().to_string();
    let cv = store.create_cv(&profile, name, "").await.unwrap();
    store.set_cv_repo(cv, repo).await.unwrap();
    let lib = store.ensure_library(&profile).await.unwrap();
    let svc = CvService { client: c.clone(), store, scope: profile, actor: "test@live".into(), lock: None };
    (svc.locked(cv, "live test").await.unwrap(), cv, lib)
}

/// What the distribution of `cv` in `env` serves at (ends with `/`).
async fn served_base(svc: &CvService, env: i64, cv: i64) -> String {
    let d: Value = svc.client.get(&svc.store.binding_repos(env, cv).await.unwrap().remove(0).distribution, &[]).await.unwrap();
    d["base_url"].as_str().unwrap().to_string()
}

async fn served_text(c: &PulpClient, url: &str) -> (u16, String) {
    let (status, body) = served(c, url).await;
    (status, String::from_utf8_lossy(&body).into_owned())
}

async fn advisories(c: &PulpClient, version: &str) -> u64 {
    let q = [("repository_version", version.to_string()), ("limit", "1".into())];
    c.list::<Value>(&c.api("content/rpm/advisories/"), &q).await.unwrap().count
}

/// Debian main: listing in large pages, a filtered Content View that later drops packages, and a
/// package removal with its release records.
#[tokio::test]
async fn debian_main_real() {
    if !real_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::Deb;
    let name = unique("real-deb");
    let url = env_or("PULP_TUI_REAL_DEBIAN", "https://deb.debian.org/debian/");
    let dist = env_or("PULP_TUI_REAL_DEBIAN_DIST", "trixie");
    let body = json!({
        "name": name, "url": url, "policy": "on_demand", "distributions": dist, "components": "main", "architectures": "amd64",
    });
    let (repo, remote, v) = synced(&c, t, &name, body).await;

    let started = Instant::now();
    let pkgs = deb::packages_in(&c, &v).await.unwrap();
    eprintln!("{name}: {} packages listed in {:.1} s", pkgs.len(), started.elapsed().as_secs_f64());
    assert!(pkgs.len() > 40_000, "{} packages", pkgs.len());
    let hello: Vec<String> = pkgs.iter().filter(|p| p.package == "hello").map(|p| p.pulp_href.clone()).collect();
    assert!(!hello.is_empty(), "hello is in {dist} main");

    // v1: hello and nginx*; verify fetches the smallest package, streamed from Debian.
    let filters = parse_rules("include name=hello; include name=nginx*", t).unwrap();
    let mut main = CvRepo { repo_href: repo.clone(), repo_name: "main".into(), repo_type: t, filters };
    let (svc, cv, lib) = content_view(&c, &name, &main).await;
    let preview = svc.preview(cv, None).await.unwrap().join("\n");
    assert!(preview.contains("+ hello_") && preview.contains("+ nginx"), "{preview}");
    svc.publish(cv, "v1", "", &quiet, &no_progress).await.unwrap();
    assert_eq!(svc.verify(cv, lib).await.unwrap(), Vec::<String>::new());
    let index_url = format!("{}dists/{dist}/main/binary-amd64/Packages", served_base(&svc, lib, cv).await);
    let (status, index) = served_text(&c, &index_url).await;
    assert_eq!(status, 200, "{index}");
    assert!(index.contains("Package: hello\n") && index.contains("Package: nginx"), "{index}");
    assert!(!index.contains("Package: bash\n"), "only the selected packages are served");

    // v2: hello only; the snapshot removes the nginx packages and their release records, then copies.
    main.filters = parse_rules("include name=hello", t).unwrap();
    svc.store.set_cv_repo(cv, &main).await.unwrap();
    svc.publish(cv, "v2", "", &quiet, &no_progress).await.unwrap();
    assert_eq!(svc.verify(cv, lib).await.unwrap(), Vec::<String>::new());
    let (status, index) = served_text(&c, &index_url).await;
    assert_eq!(status, 200, "{index}");
    assert!(index.contains("Package: hello\n") && !index.contains("Package: nginx"), "{index}");

    // A plain repository with hello; hello is removed with its release records and published again.
    let rm = format!("{name}-rm");
    let plain = create(&c, t.repositories(), json!({ "name": rm }), "/repositories/").await;
    let m = deb::copy(&c, &v, &plain, Some(0), Some(hello.clone())).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let copied = latest(&c, &plain).await;
    assert_eq!(deb::packages_in(&c, &copied).await.unwrap().len(), hello.len(), "the copy has hello");
    let units = deb::with_release_records(&c, &copied, hello).await.unwrap();
    let m = ops::remove_content(&c, &plain, units).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    let publication = ops::create_publication(&c, t, &latest(&c, &plain).await, None, &no_progress).await.unwrap();
    let rm_dist = ops::ensure_distribution(&c, t, &rm, &rm, Some(&publication), &json!({}), None, &no_progress).await.unwrap();
    let d: Value = c.get(&rm_dist, &[]).await.unwrap();
    let (status, index) = served_text(&c, &format!("{}dists/{dist}/main/binary-amd64/Packages", d["base_url"].as_str().unwrap())).await;

    svc.delete_view(cv, &quiet, &no_progress).await.unwrap();
    cleanup(&c, &[&rm_dist, &plain, &repo, &remote]).await;
    assert_eq!(status, 200, "{index}");
    assert!(!index.contains("Package: hello\n"), "the removed package is not served: {index}");
}

/// Rocky 9 BaseOS: listing, a kernel-only Content View, and the operator Publish after a settings change.
#[tokio::test]
async fn rocky9_baseos_real() {
    if !real_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::Rpm;
    let name = unique("real-rocky");
    let url = env_or("PULP_TUI_REAL_ROCKY", "https://dl.rockylinux.org/pub/rocky/9/BaseOS/x86_64/os/");
    let (repo, remote, v) = synced(&c, t, &name, json!({ "name": name, "url": url, "policy": "on_demand" })).await;

    let started = Instant::now();
    let pkgs = rpm::packages_in(&c, &v).await.unwrap();
    eprintln!("{name}: {} packages listed in {:.1} s", pkgs.len(), started.elapsed().as_secs_f64());
    assert!(pkgs.len() > 1_000, "{} packages", pkgs.len());

    // v1: kernel*; verify fetches the smallest selected package, streamed from Rocky.
    let baseos = CvRepo {
        repo_href: repo.clone(),
        repo_name: "baseos".into(),
        repo_type: t,
        filters: parse_rules("include name=kernel*", t).unwrap(),
    };
    let (svc, cv, lib) = content_view(&c, &name, &baseos).await;
    let preview = svc.preview(cv, None).await.unwrap().join("\n");
    assert!(preview.contains("+ kernel-") && !preview.contains("+ bash-"), "{preview}");
    let v1 = svc.publish(cv, "v1", "", &quiet, &no_progress).await.unwrap();
    assert_eq!(svc.verify(cv, lib).await.unwrap(), Vec::<String>::new());
    let snapshot = svc.store.version_repos(v1.id).await.unwrap().remove(0).derived_version.unwrap();
    eprintln!("{name}: {} advisories in the snapshot, {} in the source", advisories(&c, &snapshot).await, advisories(&c, &v).await);

    // Operator Publish after a settings change: a reused publication keeps the settings of its time.
    let p1 = ops::ensure_publication(&c, t, &v, None, &no_progress).await.unwrap();
    let m = c.patch(&repo, &json!({ "checksum_type": "sha512" })).await.unwrap();
    ops::settle(&c, m, &no_progress).await.unwrap();
    assert_eq!(ops::ensure_publication(&c, t, &v, None, &no_progress).await.unwrap(), p1, "ensure_publication reuses p1");
    let p2 = ops::create_publication(&c, t, &v, None, &no_progress).await.unwrap();
    assert_ne!(p2, p1, "create_publication makes a new one");
    let (old, new): (Value, Value) = (c.get(&p1, &[]).await.unwrap(), c.get(&p2, &[]).await.unwrap());

    svc.delete_view(cv, &quiet, &no_progress).await.unwrap();
    cleanup(&c, &[&repo, &remote]).await;
    if new.get("checksum_type").is_none() {
        eprintln!("{name}: the publication has no checksum_type field; settings not compared");
        return;
    }
    assert_eq!(new["checksum_type"], "sha512", "{new}");
    assert_ne!(old["checksum_type"], "sha512", "{old}");
}

/// Measurement for the RPM page size: pulp_rpm loads full package rows also with `fields=`.
#[tokio::test]
async fn rocky9_appstream_page_cost() {
    if !real_enabled() {
        return;
    }
    let c = client();
    let t = RepoType::Rpm;
    let name = unique("real-rocky");
    let url = env_or("PULP_TUI_REAL_ROCKY_APPSTREAM", "https://dl.rockylinux.org/pub/rocky/9/AppStream/x86_64/os/");
    let (repo, remote, v) = synced(&c, t, &name, json!({ "name": name, "url": url, "policy": "on_demand" })).await;

    for limit in ["500", "5000"] {
        let q = [("repository_version", v.clone()), ("fields", RPM_FIELDS.into()), ("limit", limit.into())];
        let started = Instant::now();
        let page = c.list::<Value>(&c.api(t.content()), &q).await;
        let rows = page.map_or_else(|e| e.to_string(), |p| format!("{} rows of {}", p.results.len(), p.count));
        eprintln!("{name}: one page, limit={limit}: {:.1} s ({rows})", started.elapsed().as_secs_f64());
    }
    let started = Instant::now();
    let all = rpm::packages_in(&c, &v).await;
    let secs = started.elapsed().as_secs_f64();
    cleanup(&c, &[&repo, &remote]).await;
    let all = all.unwrap();
    eprintln!("{name}: rpm::packages_in: {} packages in {secs:.1} s", all.len());
    assert!(all.len() > 4_000, "{} packages", all.len());
}
