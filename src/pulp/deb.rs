//! Deb plugin operations: package listing and upload, serving checks and the copy API.

use super::client::{Mutation, Page, PulpClient, PulpError, Result};
use super::models::*;
use super::ops::{self, CONTENT_PAGE, OnTask, UploadResult};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;

const FIELDS: &str = "pulp_href,package,version,architecture,relative_path,sha256";

/// Publications that serve the files of a version as they are (the synced upstream Release files and indices).
pub const VERBATIM_PUBLICATIONS: &str = "publications/deb/verbatim/";
/// The key of the publication mode (`ops::publication_mode`) of a verbatim publication; not a Pulp field.
pub const VERBATIM: &str = "verbatim";

pub fn is_verbatim(publication: &str) -> bool {
    publication.contains("/publications/deb/verbatim/")
}

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
/// A structured publication (the default) serves `dists/<release>/`; a simple one `dists/default/all/`; a verbatim
/// one of a flat repository `Release` and `Packages` in its directory.
pub async fn verify(c: &PulpClient, base: &str, version: &str, publication: &Value) -> Result<String> {
    let (releases, indices) = layout(c, version, publication).await?;
    for d in releases {
        served(c, base, &format!("{}Release", release_dir(&d))).await?;
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
        let index = if d.ends_with('/') {
            format!("{}Packages", release_dir(&d))
        } else {
            format!("dists/{d}/{comp}/binary-{}/Packages", p.architecture)
        };
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
/// `all`, which replaces a structured `default`. Verbatim: `verbatim_layout`. A release is named as a pulp_deb remote
/// names it: `<d>` is `dists/<d>/`, `<dir>/` a flat repository there (`/`: the root, `release_dir`).
pub async fn layout(c: &PulpClient, version: &str, publication: &Value) -> Result<(Vec<String>, Vec<(String, String)>)> {
    if is_verbatim(publication["pulp_href"].as_str().unwrap_or_default()) {
        return verbatim_layout(c, version).await;
    }
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

/// What a verbatim publication of `version` serves: the synced files at their upstream paths, one release per
/// ReleaseFile (pulp_deb 3.11.1 `_handle_distribution`): `dists/<d>` with the components of its release, or the
/// directory of a flat repository (synced as `<dir>/`, stored as `flat-repo`) with its one `Packages` index. A
/// version that no sync made has no ReleaseFile, so no release.
async fn verbatim_layout(c: &PulpClient, version: &str) -> Result<(Vec<String>, Vec<(String, String)>)> {
    let q = |f: &str| [("repository_version", version.to_string()), ("fields", f.to_string())];
    let files: Vec<Value> = c.list_all(&c.api("content/deb/release_files/"), &q("distribution,relative_path"), 10_000).await?;
    let comps: Vec<Value> = c.list_all(&c.api("content/deb/release_components/"), &q("plain_component,distribution"), 10_000).await?;
    let (mut releases, mut indices) = (vec![], vec![]);
    for f in &files {
        // The path of its Release file (seen live): `dists/<d>/Release`, or `Release` of a flat repository at the root.
        let dir = f["relative_path"].as_str().unwrap_or_default().rsplit_once('/').map_or("", |(dir, _)| dir);
        let Some(d) = dir.strip_prefix("dists/") else {
            let d = format!("{dir}/");
            indices.push((d.clone(), String::new()));
            releases.push(d);
            continue;
        };
        let comps = comps.iter().filter(|r| r["distribution"] == f["distribution"]).filter_map(|r| r["plain_component"].as_str());
        indices.extend(comps.map(|comp| (d.to_string(), comp.to_string())));
        releases.push(d.to_string());
    }
    Ok((releases, indices))
}

/// The served directory of release `d` of `layout`: `dists/<d>/`, or the directory of a flat repository (`""` at the root).
fn release_dir(d: &str) -> String {
    if d.ends_with('/') { d.trim_start_matches('/').to_string() } else { format!("dists/{d}/") }
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

/// What a local file goes to pulp_deb as; the upload form decides it once (from its Kind or the file name).
#[derive(Debug, Clone, PartialEq)]
pub enum Upload {
    /// A `.deb`. In a repository, pulp_deb lists it in component `component` of distribution `distribution` and adds
    /// the release records; each `None` is the server default (`upload`, `pulp`). Pulp sets the pool path.
    Package { distribution: Option<String>, component: Option<String> },
    /// A `.udeb` (section `debian-installer`): no release records.
    Installer,
    /// A `.dsc` and the files that it lists (`upload_source`).
    Source,
    /// Any file, served at `relative_path` of a publication.
    Generic { relative_path: String },
}

impl Upload {
    pub fn endpoint(&self) -> &'static str {
        match self {
            Upload::Package { .. } => "content/deb/packages/",
            Upload::Installer => "content/deb/installer_packages/",
            Upload::Source => "content/deb/source_packages/",
            Upload::Generic { .. } => "content/deb/generic_contents/",
        }
    }
}

/// Upload `path` as `up` (`ops::upload_fields`), optionally into `repository`. `overwrite`: as `ops::upload`.
pub async fn upload(
    c: &PulpClient,
    up: &Upload,
    path: &Path,
    repository: Option<&str>,
    overwrite: bool,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
    on: OnTask<'_>,
) -> Result<UploadResult> {
    let (distribution, component, relative_path) = match up {
        Upload::Package { distribution, component } => (distribution.as_deref(), component.as_deref(), None),
        Upload::Installer => (None, None, None),
        Upload::Source => return upload_source(c, path, repository, overwrite, progress, on).await,
        Upload::Generic { relative_path } => (None, None, Some(relative_path.as_str())),
    };
    let overwrite = (!overwrite).then_some("false");
    let fields = [
        ("repository", repository),
        ("distribution", distribution),
        ("component", component),
        ("relative_path", relative_path),
        ("overwrite", overwrite),
    ];
    let fields = fields.into_iter().filter_map(|(k, v)| Some((k, v?.to_string()))).collect();
    ops::upload_fields(c, up.endpoint(), path, fields, progress, on).await
}

/// Upload the source package of the `.dsc` at `path`. pulp_deb reads the `.dsc` from an artifact and needs an artifact
/// of each file that it lists (`dsc_files`, read next to the `.dsc`), so these are made first (`artifact`).
async fn upload_source(
    c: &PulpClient,
    path: &Path,
    repository: Option<&str>,
    overwrite: bool,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
    on: OnTask<'_>,
) -> Result<UploadResult> {
    let dsc = tokio::fs::read(path).await.map_err(|e| PulpError::Config(format!("{}: {e}", path.display())))?;
    let (text, sha256) = (String::from_utf8_lossy(&dsc), hex::encode(Sha256::digest(&dsc)));
    if let Some(kept) = other_source(c, &text, &sha256).await? {
        return Err(PulpError::Config(format!(
            "refused: Pulp has source package {kept} of this Source and Version with another .dsc; pulp_deb keeps one per \
             Source and Version and would add that one: give a rebuilt .dsc a new Version"
        )));
    }
    let dir = path.parent().unwrap_or(Path::new(""));
    for (sha256, name) in dsc_files(&text)? {
        artifact(c, &dir.join(name), &sha256, progress, on).await?;
    }
    let dsc_artifact = artifact(c, path, &sha256, progress, on).await?;
    let (content_href, task) = create_source(c, dsc_artifact, repository, overwrite, on).await?;
    ops::check_digest(c, &content_href, &sha256).await.map_err(|e| match e {
        PulpError::Decode(m) => PulpError::Decode(format!(
            "{m}: pulp_deb keeps one source package per Source and Version (and added that one); give a rebuilt .dsc a new Version"
        )),
        e => e,
    })?;
    Ok(UploadResult { content_href, sha256, task })
}

/// The source package that Pulp keeps for the Source and Version of the `.dsc` text `dsc` when its digest is not
/// `sha256`. pulp_deb returns that package for any `.dsc` of the pair (`SourcePackageSerializer.retrieve`).
async fn other_source(c: &PulpClient, dsc: &str, sha256: &str) -> Result<Option<String>> {
    let field = |name: &str| dsc.lines().find_map(|l| l.strip_prefix(name)?.strip_prefix(':')).map(|v| v.trim().to_string());
    // Without them pulp_deb refuses the .dsc itself.
    let (Some(source), Some(version)) = (field("Source"), field("Version")) else { return Ok(None) };
    // pulp_deb 3.11 answers HTTP 500 to a source package read without `artifacts` (`ops::check_digest`).
    let q = [("source", source), ("version", version), ("fields", "pulp_href,sha256,artifacts".into())];
    let found: Page<Value> = c.list(&c.api(Upload::Source.endpoint()), &q).await?;
    Ok(found.results.iter().find(|p| p["sha256"].as_str() != Some(sha256)).and_then(|p| p["pulp_href"].as_str()).map(str::to_string))
}

/// The source package of the `.dsc` artifact `dsc`, optionally in `repository`, and the task that made it.
async fn create_source(
    c: &PulpClient,
    dsc: String,
    repository: Option<&str>,
    overwrite: bool,
    on: OnTask<'_>,
) -> Result<(String, Option<Task>)> {
    let mut body = json!({ "artifact": dsc });
    if let Some(r) = repository {
        body["repository"] = r.into();
    }
    if !overwrite {
        body["overwrite"] = false.into();
    }
    let task = ops::settle(c, c.post(&c.api(Upload::Source.endpoint()), &body).await?, on).await?;
    Ok((ops::created_href(None, task.as_ref(), "/content/deb/source_packages/")?, task))
}

/// The artifact of the local file `path`, whose sha256 must be `sha256`: the one Pulp has, else `path` uploaded.
async fn artifact(
    c: &PulpClient,
    path: &Path,
    sha256: &str,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
    on: OnTask<'_>,
) -> Result<String> {
    let q = [("sha256", sha256.to_string()), ("fields", "pulp_href".into())];
    let found: Page<Value> = c.list(&c.api(ops::ARTIFACTS), &q).await?;
    if let Some(h) = found.results.first().and_then(|a| a["pulp_href"].as_str()) {
        return Ok(h.to_string());
    }
    let up = ops::upload_fields(c, ops::ARTIFACTS, path, vec![], progress, on).await?;
    if up.sha256 != sha256 {
        return Err(PulpError::Mismatch(format!("{}: sha256 {} is not {sha256} of the .dsc", path.display(), up.sha256)));
    }
    Ok(up.content_href)
}

/// The files that a `.dsc` lists in `Checksums-Sha256`: (sha256, name). Each name must be a plain file name: it is
/// read next to the `.dsc`.
fn dsc_files(dsc: &str) -> Result<Vec<(String, String)>> {
    let bad = |m: String| Err(PulpError::Config(m));
    let mut files = vec![];
    for line in dsc.lines().skip_while(|l| !l.starts_with("Checksums-Sha256:")).skip(1).take_while(|l| l.starts_with(' ')) {
        let words: Vec<&str> = line.split_whitespace().collect();
        let [sha256, _, name] = words[..] else { return bad(format!("bad Checksums-Sha256 line in the .dsc: {line:?}")) };
        if Path::new(name).file_name().and_then(|n| n.to_str()) != Some(name) {
            return bad(format!("{name:?} is not a file name (Checksums-Sha256 of the .dsc)"));
        }
        files.push((sha256.to_string(), name.to_string()));
    }
    if files.is_empty() {
        return bad("the .dsc lists no files (Checksums-Sha256)".into());
    }
    Ok(files)
}

/// Hrefs of the release records (`PackageReleaseComponent`) in `version` whose package is in `packages`.
pub async fn release_records_of(c: &PulpClient, version: &str, packages: &HashSet<&str>) -> Result<HashSet<String>> {
    let q = [("repository_version", version.to_string()), ("fields", "pulp_href,package".into()), ("limit", CONTENT_PAGE.into())];
    let v: Vec<Value> = c.list_all(&c.api(PACKAGE_RELEASE_COMPONENTS), &q, 2_000_000).await?;
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

/// Package release component records: a package listed in a release component.
pub const PACKAGE_RELEASE_COMPONENTS: &str = "content/deb/package_release_components/";

/// The release records that the TUI creates in a repository (Create on the Content screen of the type).
pub const RECORDS: [&str; 4] =
    ["content/deb/releases/", "content/deb/release_components/", "content/deb/release_architectures/", PACKAGE_RELEASE_COMPONENTS];

/// The href of the package `key` of `version`: `name`, `name_version` or `name_version_arch` (`DebPackage::key`), when
/// exactly one package of the version matches.
pub async fn package_in(c: &PulpClient, version: &str, key: &str) -> Result<String> {
    let mut q = vec![("repository_version", version.to_string()), ("fields", "pulp_href".into()), ("limit", "2".into())];
    q.extend(["package", "version", "architecture"].into_iter().zip(key.splitn(3, '_').map(str::to_string)));
    let p: Page<Value> = c.list(&c.api("content/deb/packages/"), &q).await?;
    match (p.count, p.results.first().and_then(|r| r["pulp_href"].as_str())) {
        (1, Some(h)) => Ok(h.to_string()),
        (n, _) => Err(PulpError::Config(format!(
            "{n} packages of version {} match {key:?}: give name_version_arch",
            ops::version_number(version)
        ))),
    }
}

/// The record of `package` in `release_component`: an existing one (a POST makes a new record each time), else a new
/// one. It is in no repository until it is added.
pub async fn package_release_component(c: &PulpClient, package: &str, release_component: &str) -> Result<String> {
    let q = [("package", package.to_string()), ("release_component", release_component.to_string()), ("fields", "pulp_href".into())];
    let found: Page<Value> = c.list(&c.api(PACKAGE_RELEASE_COMPONENTS), &q).await?;
    if let Some(h) = found.results.first().and_then(|r| r["pulp_href"].as_str()) {
        return Ok(h.to_string());
    }
    let body = json!({ "package": package, "release_component": release_component });
    ops::create(c, &c.api(PACKAGE_RELEASE_COMPONENTS), &body, &format!("/{PACKAGE_RELEASE_COMPONENTS}"), &ops::no_progress).await
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The files of `Checksums-Sha256`, also in a signed `.dsc`; a name that is not a plain file name is refused, as
    /// pulp-tui reads each one next to the `.dsc`.
    #[test]
    fn dsc_files_are_the_plain_file_names_of_checksums_sha256() {
        let dsc = "-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA256\n\nFormat: 3.0 (quilt)\nSource: h\nChecksums-Sha1:\n 1111 10 \
                   h_1.0.orig.tar.gz\nChecksums-Sha256:\n aaaa 10 h_1.0.orig.tar.gz\n bbbb 12 h_1.0-1.debian.tar.xz\nFiles:\n cccc 10 \
                   h_1.0.orig.tar.gz\n\n-----BEGIN PGP SIGNATURE-----\n";
        let want = [("aaaa", "h_1.0.orig.tar.gz"), ("bbbb", "h_1.0-1.debian.tar.xz")].map(|(s, n)| (s.to_string(), n.to_string()));
        assert_eq!(dsc_files(dsc).unwrap(), want);
        for bad in ["../h.tar.gz", "/etc/passwd", "d/h.tar.gz", ".."] {
            let err = dsc_files(&format!("Checksums-Sha256:\n aaaa 10 {bad}\n")).unwrap_err().to_string();
            assert!(err.contains(&format!("{bad:?} is not a file name")), "{bad}: {err}");
        }
        assert!(dsc_files("Checksums-Sha256:\n aaaa 10\n").unwrap_err().to_string().contains("\" aaaa 10\""));
        assert!(dsc_files("Source: h\nFiles:\n cccc 10 h.tar.gz\n").unwrap_err().to_string().contains("lists no files"));
    }
}
