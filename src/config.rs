//! Configuration: named Pulp server profiles and application settings.
//!
//! Precedence for the config file: `--config` > `PULP_TUI_CONFIG` > `$XDG_CONFIG_HOME/pulp-tui/config.toml`.
//! Precedence for the active profile: `--profile` > `PULP_TUI_PROFILE` > `default_profile` > first profile.
//! Secrets are never stored in the config file: a profile holds a *reference* (`env:VAR`,
//! `file:/path` or `keyring`) that is resolved when the client connects.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const KEYRING_SERVICE: &str = "pulp-tui";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Upper bound of concurrent HTTP requests per server.
    pub max_concurrent_requests: usize,
    /// Task polling interval bounds (adaptive backoff between them).
    pub poll_min_ms: u64,
    pub poll_max_ms: u64,
    /// Responses larger than this are rejected.
    pub max_response_mb: u64,
    /// Upload chunk size for the Pulp chunked upload API.
    pub upload_chunk_mb: u64,
    /// First path segment of Content View distribution base paths.
    pub cv_base_path_prefix: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_concurrent_requests: 6,
            poll_min_ms: 1000,
            poll_max_ms: 10_000,
            max_response_mb: 64,
            upload_chunk_mb: 8,
            cv_base_path_prefix: "cv".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    #[default]
    Basic,
    Bearer,
    /// No credentials (anonymous, or identity provided by the mTLS client certificate).
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Scheme + host (+ port), e.g. `https://pulp.example.com`.
    pub url: String,
    #[serde(default = "default_api_root")]
    pub api_root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default)]
    pub auth: AuthMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// `env:VAR`, `file:/path` or `keyring`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub read_only: bool,
    /// Marks the server as production: the UI shows a red banner.
    #[serde(default)]
    pub production: bool,
}

fn default_api_root() -> String {
    "/pulp/".into()
}
fn default_timeout() -> u64 {
    30
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            url: String::new(),
            api_root: default_api_root(),
            domain: None,
            auth: AuthMethod::Basic,
            username: None,
            secret: None,
            ca_cert: None,
            client_cert: None,
            client_key: None,
            proxy: None,
            timeout_secs: default_timeout(),
            read_only: false,
            production: false,
        }
    }
}

impl Profile {
    pub fn validate(&self) -> Result<()> {
        let url = url::Url::parse(&self.url).with_context(|| format!("invalid url {:?}", self.url))?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("url must use http or https");
        }
        if url.path() != "/" || url.query().is_some() {
            bail!("url must be scheme://host[:port] only; put the path into api_root");
        }
        if !self.api_root.starts_with('/') || !self.api_root.ends_with('/') {
            bail!("api_root must start and end with '/' (e.g. /pulp/)");
        }
        if let Some(d) = &self.domain
            && (d.is_empty() || !d.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)))
        {
            bail!("domain must be a plain Pulp domain name");
        }
        if let Some(s) = &self.secret {
            parse_secret_ref(s)?;
        }
        if self.auth != AuthMethod::None && self.secret.is_none() {
            bail!("auth {:?} needs a secret reference (env:VAR, file:/path or keyring)", self.auth);
        }
        if self.auth == AuthMethod::Basic && self.username.as_deref().unwrap_or("").is_empty() {
            bail!("basic auth needs a username");
        }
        if self.client_cert.is_some() != self.client_key.is_some() {
            bail!("client_cert and client_key must be set together");
        }
        Ok(())
    }

    /// `scheme://host:port` used to compare servers (self-replication guard).
    pub fn origin(&self) -> String {
        url::Url::parse(&self.url).map(|u| u.origin().ascii_serialization()).unwrap_or_else(|_| self.url.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretRef {
    Env(String),
    File(PathBuf),
    Keyring,
}

pub fn parse_secret_ref(s: &str) -> Result<SecretRef> {
    if let Some(v) = s.strip_prefix("env:") {
        if v.is_empty() {
            bail!("empty env var name in secret reference");
        }
        return Ok(SecretRef::Env(v.into()));
    }
    if let Some(p) = s.strip_prefix("file:") {
        if p.is_empty() {
            bail!("empty path in secret reference");
        }
        return Ok(SecretRef::File(p.into()));
    }
    if s == "keyring" {
        return Ok(SecretRef::Keyring);
    }
    bail!("secret must be env:VAR, file:/path or keyring (plaintext secrets are not allowed)")
}

/// Resolve a profile secret. The value is only ever passed to the HTTP client.
pub fn resolve_secret(profile_name: &str, r: &SecretRef) -> Result<String> {
    match r {
        SecretRef::Env(var) => std::env::var(var).with_context(|| format!("environment variable {var} is not set")),
        SecretRef::File(path) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(m) = std::fs::metadata(path)
                    && m.permissions().mode() & 0o077 != 0
                {
                    tracing::warn!(path = %path.display(), "secret file is readable by group/others");
                }
            }
            let s = std::fs::read_to_string(path).with_context(|| format!("cannot read secret file {}", path.display()))?;
            Ok(s.trim_end_matches(['\r', '\n']).to_string())
        }
        SecretRef::Keyring => keyring_get(profile_name),
    }
}

#[cfg(feature = "keyring")]
fn keyring_get(profile: &str) -> Result<String> {
    keyring::Entry::new(KEYRING_SERVICE, profile)
        .and_then(|e| e.get_password())
        .with_context(|| format!("no keyring secret for profile {profile}"))
}

#[cfg(not(feature = "keyring"))]
fn keyring_get(_profile: &str) -> Result<String> {
    bail!("this build has no system keyring support; use env:VAR or file:/path")
}

#[cfg(feature = "keyring")]
pub fn keyring_set(profile: &str, secret: &str) -> Result<()> {
    keyring::Entry::new(KEYRING_SERVICE, profile).and_then(|e| e.set_password(secret)).context("cannot store secret in the system keyring")
}

#[cfg(not(feature = "keyring"))]
pub fn keyring_set(_profile: &str, _secret: &str) -> Result<()> {
    bail!("this build has no system keyring support")
}

pub fn default_config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "pulp-tui").map(|d| d.config_dir().join("config.toml"))
}

pub fn default_data_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "pulp-tui").map(|d| d.data_dir().to_path_buf())
}

impl Config {
    /// A missing file is an empty config (the operator adds profiles from the TUI).
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let cfg: Config = toml::from_str(&s).with_context(|| format!("invalid config {}", path.display()))?;
                for (name, p) in &cfg.profiles {
                    p.validate().with_context(|| format!("profile {name}"))?;
                }
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// Atomic write (temp file + rename) so a crash never leaves a truncated config.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn pick_profile(&self, requested: Option<&str>) -> Option<String> {
        if let Some(r) = requested {
            return self.profiles.contains_key(r).then(|| r.to_string());
        }
        self.default_profile.clone().filter(|d| self.profiles.contains_key(d)).or_else(|| self.profiles.keys().next().cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
default_profile = "lab"
[settings]
max_concurrent_requests = 4
[profiles.lab]
url = "http://localhost:8080"
username = "admin"
secret = "env:PULP_PASSWORD"
[profiles.prod]
url = "https://pulp.example.com"
domain = "rhel"
auth = "bearer"
secret = "keyring"
production = true
read_only = true
"#;

    #[test]
    fn parses_example_and_picks_profiles() {
        let cfg: Config = toml::from_str(EXAMPLE).unwrap();
        assert_eq!(cfg.settings.max_concurrent_requests, 4);
        assert_eq!(cfg.settings.poll_max_ms, 10_000);
        assert_eq!(cfg.pick_profile(None).as_deref(), Some("lab"));
        assert_eq!(cfg.pick_profile(Some("prod")).as_deref(), Some("prod"));
        assert_eq!(cfg.pick_profile(Some("nope")), None);
        for p in cfg.profiles.values() {
            p.validate().unwrap();
        }
        assert_eq!(cfg.profiles["prod"].api_root, "/pulp/");
    }

    #[test]
    fn rejects_plaintext_password_and_bad_profiles() {
        let bad = "[profiles.x]\nurl = \"http://h\"\npassword = \"hunter2\"\n";
        assert!(toml::from_str::<Config>(bad).is_err());
        assert!(parse_secret_ref("hunter2").is_err());
        let mut p = Profile { url: "http://h:1/pulp".into(), ..Default::default() };
        assert!(p.validate().is_err());
        p.url = "ftp://h".into();
        assert!(p.validate().is_err());
        p.url = "https://h".into();
        p.auth = AuthMethod::None;
        p.validate().unwrap();
        p.client_cert = Some("c.pem".into());
        assert!(p.validate().is_err());
    }

    #[test]
    fn roundtrip_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c/config.toml");
        let cfg: Config = toml::from_str(EXAMPLE).unwrap();
        cfg.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), cfg);
        assert_eq!(Config::load(&dir.path().join("missing.toml")).unwrap(), Config::default());
    }

    #[test]
    fn resolves_file_and_env_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("s");
        std::fs::write(&f, "pw\n").unwrap();
        assert_eq!(resolve_secret("p", &SecretRef::File(f)).unwrap(), "pw");
        assert!(resolve_secret("p", &SecretRef::Env("PULP_TUI_SURELY_UNSET_VAR".into())).is_err());
    }
}
