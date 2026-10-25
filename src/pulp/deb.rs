//! Deb plugin operations: package listing, serving checks and the copy API.

use super::client::{Mutation, Page, PulpClient, PulpError, Result};
use super::models::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const FIELDS: &str = "pulp_href,package,version,architecture,relative_path,sha256";

pub async fn packages_in(c: &PulpClient, version: &str) -> Result<Vec<DebPackage>> {
    c.list_all(&c.api("content/deb/packages/"), &[("repository_version", version.into()), ("fields", FIELDS.into())], 2_000_000).await
}

/// `Filename:` of the stanza of `p` in a `Packages` index.
fn served_path(index: &str, p: &DebPackage) -> Option<String> {
    let field =
        |stanza: &str, name: &str| stanza.lines().find_map(|l| l.strip_prefix(name)?.strip_prefix(':')).map(|v| v.trim().to_string());
    index.split("\n\n").find_map(|s| {
        let is_p = field(s, "Package").as_deref() == Some(&p.package)
            && field(s, "Version").as_deref() == Some(&p.version)
            && field(s, "Architecture").as_deref() == Some(&p.architecture);
        if is_p { field(s, "Filename") } else { None }
    })
}

/// The Release file must be served and the smallest package must be listed in a `Packages` index
/// and downloadable with the expected digest. `base` ends with `/`.
pub async fn verify(c: &PulpClient, base: &str, version: &str) -> Result<String> {
    let fail = |m: String| Err(PulpError::Unsupported(m));
    let q = [("repository_version", version.to_string()), ("fields", "distribution".into()), ("limit", "1".into())];
    let releases: Page<Value> = c.list(&c.api("content/deb/releases/"), &q).await?;
    let Some(dist) = releases.results.first().and_then(|r| r["distribution"].as_str()) else {
        return fail("the version has no Release".into());
    };
    // pulp_deb publishes under `dists/<distribution without slashes>/<last part of the component>/`.
    let dir = dist.trim_matches('/');
    let probe = format!("dists/{dir}/Release");
    let (status, _) = c.fetch_content(&format!("{base}{probe}"), 64 << 20).await?;
    if status != 200 {
        return fail(format!("{probe} returned HTTP {status}"));
    }
    let q = [
        ("repository_version", version.to_string()),
        ("fields", FIELDS.into()),
        ("ordering", "installed_size".into()),
        ("limit", "1".into()),
    ];
    let pkgs: Page<DebPackage> = c.list(&c.api("content/deb/packages/"), &q).await?;
    let Some(p) = pkgs.results.first() else { return Ok("Release served (no packages)".into()) };
    let comps: Vec<Value> = c
        .list_all(
            &c.api("content/deb/release_components/"),
            &[("repository_version", version.into()), ("fields", "plain_component,distribution".into())],
            10_000,
        )
        .await?;
    let mut tried = vec![];
    // The smallest package of the version can be in any of its distributions.
    let comps = comps.iter().filter_map(|r| Some((r["distribution"].as_str()?.trim_matches('/'), r["plain_component"].as_str()?)));
    for (d, comp) in comps {
        let index = format!("dists/{d}/{comp}/binary-{}/Packages", p.architecture);
        let (status, body) = c.fetch_content(&format!("{base}{index}"), 64 << 20).await?;
        let path = if status == 200 { served_path(&String::from_utf8_lossy(&body), p) } else { None };
        let Some(path) = path else {
            tried.push(index);
            continue;
        };
        let (status, body) = c.fetch_content(&format!("{base}{path}"), 64 << 20).await?;
        if status == 200 && p.sha256.as_deref() == Some(hex::encode(Sha256::digest(&body)).as_str()) {
            return Ok(format!("Release and {path} served with matching digest ({} packages)", pkgs.count));
        }
        return fail(format!("package {path} not served with the expected digest"));
    }
    fail(format!("package {} not listed in any Packages index (tried {})", p.key(), tried.join(", ")))
}

/// Django's default `DATA_UPLOAD_MAX_MEMORY_SIZE` in Pulp: a larger request body gets HTTP 400 (an HTML page).
const MAX_BODY: usize = 2_621_440;

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
    let n = content.as_ref().map_or(0, Vec::len);
    if let Some(u) = content {
        cfg["content"] = u.into();
    }
    // shortcut: one copy body holds every selected href; Pulp (Django DATA_UPLOAD_MAX_MEMORY_SIZE, default 2.5 MiB ≈ 34,900 hrefs) refuses larger bodies; batch when that limit matters.
    let body = json!({ "config": [cfg], "structured": true, "dependency_solving": false });
    let size = body.to_string().len();
    c.post(&c.api("deb/copy/"), &body).await.map_err(|e| match e {
        PulpError::Api { status: 400, message } if size > MAX_BODY => {
            let hint = format!(
                "request body {size} bytes, {n} hrefs; the server refuses request bodies above DATA_UPLOAD_MAX_MEMORY_SIZE \
                 (default 2.5 MiB): raise it in the Pulp settings, or narrow the selection with include rules"
            );
            PulpError::Api { status: 400, message: if message.is_empty() { hint } else { format!("{message}; {hint}") } }
        }
        e => e,
    })
}
