//! File plugin operations: file listing and serving checks.

use super::client::{PulpClient, PulpError, Result};
use super::models::*;
use super::ops::CONTENT_PAGE;
use std::collections::BTreeMap;

pub async fn files_in(c: &PulpClient, version: &str) -> Result<Vec<FileContent>> {
    let q = [("repository_version", version.into()), ("fields", "pulp_href,relative_path,sha256".into()), ("limit", CONTENT_PAGE.into())];
    c.list_all(&c.api(RepoType::File.content()), &q, 2_000_000).await
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
    let served: BTreeMap<String, String> = String::from_utf8_lossy(&body)
        .lines()
        .filter_map(|l| {
            // pulp_file writes `path,sha256,size` unquoted and reads it from the right.
            let mut it = l.rsplitn(3, ',');
            let (_size, digest) = (it.next()?, it.next()?);
            Some((it.next()?.to_string(), digest.to_string()))
        })
        .collect();
    let want = files_in(c, version).await?;
    // `sha256` comes from the stored file: `null` for an on_demand file until a client downloads it.
    let differs = want.iter().any(|f| served.get(&f.relative_path).is_none_or(|d| f.sha256.as_ref().is_some_and(|s| s != d)));
    if differs || served.len() != want.len() {
        return fail(format!("served manifest differs from snapshot ({} served, {} expected)", served.len(), want.len()));
    }
    Ok(format!("PULP_MANIFEST matches {} files", want.len()))
}
