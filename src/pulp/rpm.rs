//! RPM plugin operations: package listing, serving checks and the copy API.

use super::client::{Mutation, Page, PulpClient, PulpError, Result};
use super::models::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub async fn packages_in(c: &PulpClient, version: &str) -> Result<Vec<Package>> {
    let fields = "pulp_href,name,epoch,version,release,arch,location_href,sha256";
    c.list_all(&c.api(RepoType::Rpm.content()), &[("repository_version", version.into()), ("fields", fields.into())], 2_000_000).await
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

/// RPM metadata must be served and the smallest package must be downloadable with the
/// expected digest. `base` ends with `/`.
pub async fn verify(c: &PulpClient, base: &str, version: &str, layout: &str) -> Result<String> {
    let fail = |m: String| Err(PulpError::Unsupported(m));
    let probe = "repodata/repomd.xml";
    let (status, _) = c.fetch_content(&format!("{base}{probe}"), 64 << 20).await?;
    if status != 200 {
        return fail(format!("{probe} returned HTTP {status}"));
    }
    let q = [
        ("repository_version", version.to_string()),
        ("fields", "location_href,sha256,pkgId,checksum_type".into()),
        ("ordering", "size_package".into()),
        ("limit", "1".into()),
    ];
    let pkgs: Page<Value> = c.list(&c.api(RepoType::Rpm.content()), &q).await?;
    let Some(p) = pkgs.results.first() else { return Ok("repomd.xml served (no packages)".into()) };
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
            return Ok(format!("repomd.xml and {path} served with matching digest ({} packages)", pkgs.count));
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
