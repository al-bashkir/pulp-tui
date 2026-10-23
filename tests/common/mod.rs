//! Shared helpers for live tests against a running Pulp (see tests/env/pulp-env.sh).
//! Live tests are skipped unless `PULP_TUI_LIVE=1`.
#![allow(dead_code)]

use pulp_tui::config::{Profile, Settings};
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{PulpClient, RepoType};
use serde_json::{Value, json};
use std::path::PathBuf;

pub fn live_enabled() -> bool {
    if std::env::var("PULP_TUI_LIVE").as_deref() == Ok("1") {
        true
    } else {
        eprintln!("skipping live test: set PULP_TUI_LIVE=1 (and start tests/env/pulp-env.sh up)");
        false
    }
}

/// Client for a test instance. The password is passed through a temporary secret file,
/// the same mechanism used in containers.
pub fn client_for(url: &str, name: &str) -> PulpClient {
    let dir = std::env::temp_dir().join(format!("pulp-tui-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let secret = dir.join(name);
    std::fs::write(&secret, std::env::var("PULP_TUI_TEST_PASSWORD").unwrap_or_else(|_| "password".into())).unwrap();
    let p = Profile {
        url: url.into(),
        username: Some("admin".into()),
        secret: Some(format!("file:{}", secret.display())),
        ..Default::default()
    };
    let s = Settings { poll_min_ms: 300, poll_max_ms: 2000, upload_chunk_mb: 1, ..Default::default() };
    PulpClient::new(name, &p, &s).unwrap()
}

pub fn client() -> PulpClient {
    client_for(&std::env::var("PULP_TUI_TEST_URL").unwrap_or_else(|_| "http://localhost:8080".into()), "pulp1")
}

/// Upstream fixture repositories as seen from inside the Pulp container.
pub fn upstream() -> String {
    std::env::var("PULP_TUI_TEST_UPSTREAM").unwrap_or_else(|_| "http://host.containers.internal:8000".into())
}

pub fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/fixtures")
}

pub fn unique(prefix: &str) -> String {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    format!("{prefix}-{}", n % 1_000_000_000_000)
}

pub async fn create(c: &PulpClient, path: &str, body: Value, needle: &str) -> String {
    ops::create(c, &c.api(path), &body, needle, &no_progress).await.unwrap()
}

pub async fn synced_repo(c: &PulpClient, t: RepoType, name: &str, upstream_path: &str, policy: &str) -> (String, String) {
    let url = format!("{}/{upstream_path}", upstream());
    let remote = create(c, t.remotes(), json!({ "name": name, "url": url, "policy": "immediate" }), "/remotes/").await;
    let repo = create(c, t.repositories(), json!({ "name": name, "remote": remote }), "/repositories/").await;
    let m = ops::sync(c, t, &repo, None, policy).await.unwrap();
    ops::settle(c, m, &no_progress).await.unwrap();
    (repo, remote)
}

pub async fn latest(c: &PulpClient, repo: &str) -> String {
    let r: Value = c.get(repo, &[]).await.unwrap();
    r["latest_version_href"].as_str().unwrap().to_string()
}

pub async fn served(c: &PulpClient, url: &str) -> (u16, Vec<u8>) {
    c.fetch_content(url, 64 << 20).await.unwrap()
}

/// Best-effort removal; tests use unique names so leftovers never collide.
pub async fn cleanup(c: &PulpClient, hrefs: &[&str]) {
    for h in hrefs {
        if let Ok(m) = c.delete(h).await {
            let _ = ops::settle(c, m, &no_progress).await;
        }
    }
}
