//! Deb plugin operations: package listing and upload, serving checks and the copy API.

use super::client::{Mutation, Page, PulpClient, PulpError, Result};
use super::models::*;
use super::ops::{self, CONTENT_PAGE, OnTask, UploadResult};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;

const FIELDS: &str = "pulp_href,package,version,architecture,relative_path,sha256";

/// Publications that serve the synced upstream metadata as is; pulp-tui does not create them.
pub const VERBATIM_PUBLICATIONS: &str = "publications/deb/verbatim/";

/// Content types that pulp_deb removes from every new version (`AptRepository.initialize_new_version`);
/// only a sync adds them again.
pub const NEW_VERSION_DROPS: [&str; 3] =
    ["content/deb/release_files/", "content/deb/package_indices/", "content/deb/installer_file_indices/"];

pub async fn packages_in(c: &PulpClient, version: &str) -> Result<Vec<DebPackage>> {
    let q = [("repository_version", version.into()), ("fields", FIELDS.into()), ("limit", CONTENT_PAGE.into())];
    c.list_all(&c.api("content/deb/packages/"), &q, 2_000_000).await
}

/// `Filename:` and `SHA256:` of the stanza of `p` in a `Packages` index.
fn served_entry(index: &str, p: &DebPackage) -> Option<(String, Option<String>)> {
    let field =
        |stanza: &str, name: &str| stanza.lines().find_map(|l| l.strip_prefix(name)?.strip_prefix(':')).map(|v| v.trim().to_string());
    index.split("\n\n").find_map(|s| {
        let is_p = field(s, "Package").as_deref() == Some(&p.package)
            && field(s, "Version").as_deref() == Some(&p.version)
            && field(s, "Architecture").as_deref() == Some(&p.architecture);
        if is_p { Some((field(s, "Filename")?, field(s, "SHA256"))) } else { None }
    })
}

/// The Release files of the modes of `publication` must be served and the smallest package must be
/// listed in a `Packages` index and downloadable with the expected digest. `base` ends with `/`.
/// A structured publication (the default) serves `dists/<release>/`; a simple one `dists/default/all/`.
pub async fn verify(c: &PulpClient, base: &str, version: &str, publication: &Value) -> Result<String> {
    let (releases, indices) = layout(c, version, publication).await?;
    for dir in releases {
        served(c, base, &format!("dists/{dir}/Release")).await?;
    }
    let q = [
        ("repository_version", version.to_string()),
        ("fields", FIELDS.into()),
        ("ordering", "installed_size".into()),
        ("limit", "1".into()),
    ];
    let pkgs: Page<DebPackage> = c.list(&c.api("content/deb/packages/"), &q).await?;
    let Some(p) = pkgs.results.first() else { return Ok("Release served (no packages)".into()) };
    let mut tried = vec![];
    // The smallest package of the version can be in any of its distributions.
    for (d, comp) in indices {
        let index = format!("dists/{d}/{comp}/binary-{}/Packages", p.architecture);
        match served_package(c, base, p, &index).await? {
            Some(path) => return Ok(format!("Release and {path} served with matching digest ({} packages)", pkgs.count)),
            None => tried.push(index),
        }
    }
    Err(PulpError::Mismatch(format!("package {} not listed in any Packages index (tried {})", p.key(), tried.join(", "))))
}

/// An error unless `base` serves `path` (HTTP 200).
async fn served(c: &PulpClient, base: &str, path: &str) -> Result<()> {
    let (status, _) = c.fetch_content(&format!("{base}{path}"), 64 << 20).await?;
    if status != 200 {
        return Err(PulpError::Mismatch(format!("{path} returned HTTP {status}")));
    }
    Ok(())
}

/// What a publication of `version` serves, as pulp_deb publishes it (`tasks/publishing.py`): the `dists/`
/// directories with a Release file, and each (directory, component) with package indices. Structured (the
/// default): each distribution of the release components of the version, also one without a Release record
/// (pulp_deb then makes the Release file), under `dists/<distribution without outer slashes, "/" is
/// flat-repo>/<last part of the component>/`; without release components, `default` with component `empty`
/// (pulp_deb 3.11.1; 3.5.4 and 3.8.1 publish no structured release then). Simple: `default` with component
/// `all`, which replaces a structured `default`.
async fn layout(c: &PulpClient, version: &str, publication: &Value) -> Result<(Vec<String>, Vec<(String, String)>)> {
    let simple = publication["simple"] == true;
    let (mut releases, mut indices) = (vec![], vec![]);
    if publication["structured"] != false {
        let q = [("repository_version", version.to_string()), ("fields", "plain_component,distribution".into())];
        let comps: Vec<Value> = c.list_all(&c.api("content/deb/release_components/"), &q, 10_000).await?;
        let mut pairs: Vec<(&str, &str)> =
            comps.iter().filter_map(|r| Some((r["distribution"].as_str()?, r["plain_component"].as_str()?))).collect();
        if pairs.is_empty() && !simple {
            pairs.push(("default", "empty"));
        }
        for (d, comp) in pairs.into_iter().filter(|(d, _)| !simple || *d != "default") {
            let dir = if d == "/" { "flat-repo".to_string() } else { d.trim_matches('/').to_string() };
            if !releases.contains(&dir) {
                releases.push(dir.clone());
            }
            indices.push((dir, comp.to_string()));
        }
    }
    if simple {
        releases.push("default".into());
        indices.push(("default".into(), "all".into()));
    }
    Ok((releases, indices))
}

/// The path of package `p` when `index` lists it and the server serves it with the digest that apt checks;
/// `None` when `index` is not served or does not list it.
async fn served_package(c: &PulpClient, base: &str, p: &DebPackage, index: &str) -> Result<Option<String>> {
    let fail = |m: String| Err(PulpError::Mismatch(m));
    let (status, body) = c.fetch_content(&format!("{base}{index}"), 64 << 20).await?;
    let entry = if status == 200 { served_entry(&String::from_utf8_lossy(&body), p) } else { None };
    let Some((path, listed)) = entry else { return Ok(None) };
    // The API has no sha256 for an on_demand package until its file is downloaded; the index has the one apt checks.
    let Some(want) = listed.or_else(|| p.sha256.clone()) else { return fail(format!("package {path} has no SHA256 in {index}")) };
    if p.sha256.as_ref().is_some_and(|s| *s != want) {
        return fail(format!("package {path}: {index} lists another SHA256 than Pulp has"));
    }
    let (status, body) = c.fetch_content(&format!("{base}{path}"), 64 << 20).await?;
    if status == 200 && hex::encode(Sha256::digest(&body)) == want {
        return Ok(Some(path));
    }
    fail(format!("package {path} not served with the expected digest"))
}

/// Upload a `.deb` (`ops::upload_fields`), optionally into `repository`. There pulp_deb lists it in component
/// `component` of distribution `distribution` (it adds the release records); each one left out is the server
/// default (`pulp`, `upload`). Pulp sets the pool path from the package.
pub async fn upload(
    c: &PulpClient,
    path: &Path,
    repository: Option<&str>,
    distribution: Option<&str>,
    component: Option<&str>,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
    on: OnTask<'_>,
) -> Result<UploadResult> {
    let fields = [("repository", repository), ("distribution", distribution), ("component", component)];
    let fields = fields.into_iter().filter_map(|(k, v)| Some((k, v?.to_string()))).collect();
    ops::upload_fields(c, RepoType::Deb, path, fields, progress, on).await
}

/// Hrefs of the release records (`PackageReleaseComponent`) in `version` whose package is in `packages`.
pub async fn release_records_of(c: &PulpClient, version: &str, packages: &HashSet<&str>) -> Result<HashSet<String>> {
    let q = [("repository_version", version.to_string()), ("fields", "pulp_href,package".into()), ("limit", CONTENT_PAGE.into())];
    let v: Vec<Value> = c.list_all(&c.api("content/deb/package_release_components/"), &q, 2_000_000).await?;
    Ok(v.iter()
        .filter(|r| r["package"].as_str().is_some_and(|p| packages.contains(p)))
        .filter_map(|r| r["pulp_href"].as_str().map(str::to_string))
        .collect())
}

/// `units`, then the binary-package release records (PRCs) in `version` of the packages among them.
/// A structured publication publishes every PRC of a version, also one whose package is gone. This is a
/// fallback: pulp_deb 3.11.1 `modify/` removes these records itself (`_prepare_package_removals`); older
/// versions were not checked (COMPATIBILITY.md).
pub async fn with_release_records(c: &PulpClient, version: &str, units: Vec<String>) -> Result<Vec<String>> {
    let packages: HashSet<&str> = units.iter().map(String::as_str).collect();
    let mut records: Vec<String> =
        release_records_of(c, version, &packages).await?.into_iter().filter(|r| !packages.contains(r.as_str())).collect();
    // Sorted: the set has no order, and a request body should not change from run to run.
    records.sort();
    Ok(units.into_iter().chain(records).collect())
}

/// Release, release component and release architecture records: shared by all packages of a release.
pub fn is_release_record(href: &str) -> bool {
    ["/content/deb/releases/", "/content/deb/release_components/", "/content/deb/release_architectures/"].iter().any(|p| href.contains(p))
}

/// Copy content of a repository version into `dest_repo` (Deb copy API). `content` limits the
/// copy to those hrefs; `dest_base_version` starts the new version from that base (0 = empty).
/// `structured` also copies the release, component and architecture records of the packages.
pub async fn copy(
    c: &PulpClient,
    source_version: &str,
    dest_repo: &str,
    dest_base_version: Option<u64>,
    content: Option<Vec<String>>,
) -> Result<Mutation> {
    let mut cfg = json!({ "source_repo_version": source_version, "dest_repo": dest_repo });
    if let Some(b) = dest_base_version {
        cfg["dest_base_version"] = b.into();
    }
    if let Some(u) = content {
        cfg["content"] = u.into();
    }
    c.post(&c.api("deb/copy/"), &json!({ "config": [cfg], "structured": true, "dependency_solving": false })).await
}
