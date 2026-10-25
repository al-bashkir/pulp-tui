//! File plugin operations: file listing, serving checks and repository modify.

use super::client::{Mutation, PulpClient, PulpError, Result};
use super::models::*;
use serde_json::json;
use std::collections::BTreeSet;

pub async fn files_in(c: &PulpClient, version: &str) -> Result<Vec<FileContent>> {
    c.list_all(
        &c.api(RepoType::File.content()),
        &[("repository_version", version.into()), ("fields", "pulp_href,relative_path,sha256".into())],
        2_000_000,
    )
    .await
}

pub fn key(f: &FileContent) -> String {
    format!("{}  ({})", f.relative_path, f.sha256.as_deref().map(|s| &s[..s.len().min(12)]).unwrap_or("?"))
}

/// The served manifest must list exactly the version's files. `base` ends with `/`.
pub async fn verify(c: &PulpClient, base: &str, version: &str) -> Result<String> {
    let fail = |m: String| Err(PulpError::Unsupported(m));
    let probe = "PULP_MANIFEST";
    let (status, body) = c.fetch_content(&format!("{base}{probe}"), 64 << 20).await?;
    if status != 200 {
        return fail(format!("{probe} returned HTTP {status}"));
    }
    let served: BTreeSet<(String, String)> = String::from_utf8_lossy(&body)
        .lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, ',');
            Some((it.next()?.to_string(), it.next()?.to_string()))
        })
        .collect();
    let want: BTreeSet<(String, String)> =
        files_in(c, version).await?.into_iter().map(|f| (f.relative_path, f.sha256.unwrap_or_default())).collect();
    if served != want {
        return fail(format!("served manifest differs from snapshot ({} served, {} expected)", served.len(), want.len()));
    }
    Ok(format!("PULP_MANIFEST matches {} files", want.len()))
}

/// Add content units to a repository; `replace_all` first removes all existing units.
pub async fn modify(c: &PulpClient, repo: &str, replace_all: bool, add: Vec<String>) -> Result<Mutation> {
    let mut body = json!({ "add_content_units": add });
    if replace_all {
        body["remove_content_units"] = json!(["*"]);
    }
    c.post(&format!("{repo}modify/"), &body).await
}
