//! RPM plugin operations: package listing, serving checks, the copy API, comps and advisory uploads, prune.

use super::client::{Mutation, Page, PulpClient, PulpError, Result};
use super::models::*;
use super::ops::CONTENT_PAGE;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Upload endpoints (POST) of comps.xml and of one advisory.
pub const COMPS: &str = "rpm/comps/";
pub const ADVISORIES: &str = "content/rpm/advisories/";

/// POST the file at `path` to `endpoint` with `fields` (multipart, field `file`); a task that makes a new version.
async fn upload_file(c: &PulpClient, endpoint: &str, path: &Path, fields: &[(&str, String)]) -> Result<Mutation> {
    let bytes = tokio::fs::read(path).await.map_err(|e| PulpError::Config(format!("{}: {e}", path.display())))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("upload").to_string();
    c.post_form(&c.api(endpoint), fields, Some((name, bytes))).await
}

/// Add the groups, categories, environments and langpacks of a comps.xml to `repo`; `replace` first removes the ones it has.
pub async fn upload_comps(c: &PulpClient, path: &Path, repo: &str, replace: bool) -> Result<Mutation> {
    upload_file(c, COMPS, path, &[("repository", repo.into()), ("replace", replace.to_string())]).await
}

/// Add one advisory to `repo`: a JSON file with the fields of an updateinfo record (`id`, `issued`, `updated` required).
pub async fn upload_advisory(c: &PulpClient, path: &Path, repo: &str) -> Result<Mutation> {
    upload_file(c, ADVISORIES, path, &[("repository", repo.into())]).await
}

/// Prune old packages (POST, pulp_rpm 3.27+): a task group with the `prune_packages` task and one task per repository.
pub const PRUNE: &str = "rpm/prune/";

/// What the prune task of one repository reports: the packages that go (dry run) or went, and the new version.
/// `not_removed`: packages that the new version has as removed but still lists (pulp_rpm 3.34.0 on pulpcore 3.95.3).
#[derive(Debug, Clone, PartialEq)]
pub struct Pruned {
    pub repo: String,
    pub packages: u64,
    pub version: Option<String>,
    pub not_removed: u64,
}

/// Remove from the latest version of each of `repos` (a new version) the packages that have a newer one of the same
/// name and arch and came in more than `keep_days` days ago (0 = keep only the newest). A dry run changes nothing and
/// reports the counts; Pulp names the packages only in its debug log. Waits for the group: a failed task fails it.
pub async fn prune(
    c: &PulpClient,
    repos: &[String],
    keep_days: u64,
    dry_run: bool,
    on: &(dyn Fn(&TaskGroup) + Send + Sync),
) -> Result<Vec<Pruned>> {
    let body = json!({ "repo_hrefs": repos, "keep_days": keep_days, "dry_run": dry_run });
    let Mutation::TaskGroup(g) = c.post(&c.api(PRUNE), &body).await? else {
        return Err(PulpError::Decode(format!("{PRUNE} did not answer with a task group")));
    };
    let (group, lines) = super::ops::group_report(c, &g, on).await?;
    if group.outcome() != "completed" {
        return Err(PulpError::TaskFailed { href: g, state: group.outcome().into(), message: lines.join("\n") });
    }
    let repo_tasks = group.tasks.iter().filter(|t| t["name"].as_str().is_some_and(|n| n.ends_with(".prune_repo_packages")));
    let mut out = vec![];
    for h in repo_tasks.filter_map(|t| t["pulp_href"].as_str()) {
        let t: Task = c.get(h, &[]).await?;
        if let Some(r) = t.progress_reports.iter().find(|r| r.code == "rpm.package.prune.repository") {
            let repo = r.message.trim_start_matches("Pruning ").to_string();
            let version = t.created_matching("/versions/");
            let mut not_removed = 0;
            if let Some(v) = &version {
                // A pulp_rpm without the fix (3.35.0; 3.32.15, 3.29.13, 3.27.14) on a pulpcore with version
                // content_ids (seen: 3.34.0 on 3.95.3) marks the packages removed, but the version still lists them.
                let q = [("repository_version", v.clone()), ("repository_version_removed", v.clone()), ("limit", "1".into())];
                not_removed = c.list::<Value>(&c.api(RepoType::Rpm.content()), &q).await?.count;
            }
            out.push(Pruned { repo, packages: r.total.unwrap_or(0), version, not_removed });
        }
    }
    out.sort_by(|a, b| a.repo.cmp(&b.repo));
    Ok(out)
}

pub async fn packages_in(c: &PulpClient, version: &str) -> Result<Vec<Package>> {
    let fields = "pulp_href,name,epoch,version,release,arch,location_href,sha256";
    let q = [("repository_version", version.into()), ("fields", fields.into()), ("limit", CONTENT_PAGE.into())];
    c.list_all(&c.api(RepoType::Rpm.content()), &q, 2_000_000).await
}

/// Path of a package inside a published RPM repository for the publication `layout`.
// shortcut: nested_by_digest paths are not derived, callers skip the package probe; add when needed.
pub fn package_path(layout: &str, location_href: &str) -> Option<String> {
    let file = location_href.rsplit('/').next().filter(|f| !f.is_empty())?;
    match layout {
        "flat" => Some(format!("Packages/{file}")),
        "nested_alphabetically" | "" => Some(format!("Packages/{}/{file}", file.chars().next()?.to_ascii_lowercase())),
        _ => None,
    }
}

/// Whether the repository of `version` has a metadata signing service (a server without the field: no).
pub async fn signs_metadata(c: &PulpClient, version: &str) -> Result<bool> {
    let repo: Value = c.get(&super::ops::repo_of_version(version), &[("fields", "metadata_signing_service".into())]).await?;
    Ok(repo["metadata_signing_service"].is_string())
}

/// The metadata files that `base` serves (`repomd.xml`, and `repomd.xml.asc` when the repository of
/// `version` has a metadata signing service); an error when one is missing. A publication made before the
/// repository got its signing service has no signature.
async fn served_metadata(c: &PulpClient, base: &str, version: &str) -> Result<&'static str> {
    let signed = signs_metadata(c, version).await?;
    let files: &[&str] = if signed { &["repomd.xml", "repomd.xml.asc"] } else { &["repomd.xml"] };
    for f in files {
        let (status, _) = c.fetch_content(&format!("{base}repodata/{f}"), 64 << 20).await?;
        if status != 200 {
            let why = if f.ends_with(".asc") { " (the repository signs its metadata)" } else { "" };
            return Err(PulpError::Mismatch(format!("repodata/{f} returned HTTP {status}{why}")));
        }
    }
    Ok(if signed { "repomd.xml, repomd.xml.asc" } else { "repomd.xml" })
}

/// RPM metadata must be served (and signed, when the repository has a metadata signing service) and
/// the smallest package must be downloadable with the expected digest. `base` ends with `/`.
pub async fn verify(c: &PulpClient, base: &str, version: &str, layout: &str) -> Result<String> {
    let fail = |m: String| Err(PulpError::Mismatch(m));
    let metadata = served_metadata(c, base, version).await?;
    let q = [
        ("repository_version", version.to_string()),
        ("fields", "location_href,sha256,pkgId,checksum_type".into()),
        ("ordering", "size_package".into()),
        ("limit", "1".into()),
    ];
    let pkgs: Page<Value> = c.list(&c.api(RepoType::Rpm.content()), &q).await?;
    let Some(p) = pkgs.results.first() else { return Ok(format!("{metadata} served (no packages)")) };
    let loc = p["location_href"].as_str().unwrap_or_default();
    // `sha256` comes from the stored file: `null` for an on_demand package until a client downloads it.
    let Some(want) = p["sha256"].as_str().or_else(|| p["pkgId"].as_str().filter(|_| p["checksum_type"] == "sha256")) else {
        return fail(format!("package {loc} has no SHA256 digest in Pulp"));
    };
    // Publications use the layout; mirror_complete publications keep upstream paths.
    let candidates: Vec<String> = package_path(layout, loc).into_iter().chain([loc.to_string()]).collect();
    for path in &candidates {
        let (status, body) = c.fetch_content(&format!("{base}{path}"), 64 << 20).await?;
        if status == 200 && hex::encode(Sha256::digest(&body)) == want {
            return Ok(format!("{metadata} and {path} served with matching digest ({} packages)", pkgs.count));
        }
    }
    fail(format!("package {loc} not served with the expected digest (tried {})", candidates.join(", ")))
}

/// Copy content of a repository version into `dest_repo` (RPM copy API). `content` limits the
/// copy to those hrefs; `dest_base_version` starts the new version from that base (0 = empty).
pub async fn copy(
    c: &PulpClient,
    source_version: &str,
    dest_repo: &str,
    dest_base_version: Option<u64>,
    content: Option<Vec<String>>,
    dependency_solving: bool,
) -> Result<Mutation> {
    let mut cfg = json!({ "source_repo_version": source_version, "dest_repo": dest_repo });
    if let Some(b) = dest_base_version {
        cfg["dest_base_version"] = b.into();
    }
    if let Some(u) = content {
        cfg["content"] = u.into();
    }
    c.post(&c.api("rpm/copy/"), &json!({ "config": [cfg], "dependency_solving": dependency_solving })).await
}
