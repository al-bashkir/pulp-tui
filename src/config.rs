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
/// Keyring user of `[database].secret = "keyring"`. Profiles use their name as the user; a
/// profile name cannot hold brackets, so the two never share an entry.
pub const DATABASE_KEYRING_USER: &str = "[database]";
/// `url` of a profile's `database` that keeps the profile in the local file, whatever `[database]` says.
pub const LOCAL_DATABASE: &str = "local";

/// Keyring user of `keyring` in `[profiles.<profile>.database].secret`.
pub fn database_keyring_user(profile: &str) -> String {
    format!("{DATABASE_KEYRING_USER}:{profile}")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    #[serde(default)]
    pub settings: Settings,
    /// External Content View / lifecycle database; the default for profiles without their own `database`.
    /// Without either, Content Views stay in the local file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<Database>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Database {
    /// `postgres://user@host:5432/db` or `sqlite:///path/cv.db`; never with a password.
    pub url: String,
    /// `env:VAR`, `file:/path` or `keyring` (keyring user `[database]`, or `[database]:<profile>` for a profile's own).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert: Option<PathBuf>,
}

impl Database {
    /// The marker `url = "local"`: the profile uses the local file, not an external database.
    pub fn is_local(&self) -> bool {
        self.url == LOCAL_DATABASE
    }

    /// Errors never repeat the URL: it could hold a password.
    fn parsed(&self) -> Result<url::Url> {
        let u = url::Url::parse(&self.url).map_err(|e| anyhow::anyhow!("invalid database url: {e}"))?;
        if !matches!(u.scheme(), "postgres" | "postgresql" | "sqlite") {
            bail!("database url must use postgres, postgresql or sqlite");
        }
        if u.password().is_some()
            || u.query_pairs().any(|(k, _)| k.eq_ignore_ascii_case("password") || k.eq_ignore_ascii_case("sslpassword"))
        {
            bail!("put the database password in secret, not in the URL");
        }
        Ok(u)
    }

    pub fn validate(&self) -> Result<()> {
        if self.is_local() {
            return Ok(());
        }
        self.parsed()?;
        if let Some(s) = &self.secret {
            parse_secret_ref(s)?;
        }
        Ok(())
    }

    /// The URL for sqlx, with the password and the TLS settings. Never show or log it.
    /// A `keyring` secret is read from `keyring_user`.
    pub fn connect_url(&self, keyring_user: &str) -> Result<String> {
        let mut u = self.parsed()?;
        if u.scheme() == "sqlite" {
            return Ok(self.url.clone());
        }
        if let Some(s) = &self.secret {
            let pw = resolve_secret(keyring_user, &parse_secret_ref(s)?)?;
            // `set_password` leaves `%` as is and sqlx percent-decodes: encode every byte but ASCII alphanumerics.
            let pw: String =
                pw.bytes().map(|b| if b.is_ascii_alphanumeric() { char::from(b).to_string() } else { format!("%{b:02X}") }).collect();
            u.set_password(Some(&pw)).map_err(|_| anyhow::anyhow!("database url needs a host to set the password"))?;
        }
        // sqlx connects to the last `host`/`hostaddr` parameter, else to the URL host.
        let host = u.query_pairs().filter(|(k, _)| k == "host" || k == "hostaddr").map(|(_, v)| v.into_owned()).last();
        let host = host.or_else(|| u.host_str().map(|h| h.to_ascii_lowercase()));
        // A socket path (`%2F...` in the URL host) or a loopback address: traffic stays on this machine.
        let local = host.is_some_and(|h| {
            h.starts_with('/') || h.starts_with("%2f") || matches!(h.as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]")
        });
        let ca = self.ca_cert.as_ref().map(|p| p.to_string_lossy().into_owned());
        // sqlx applies parameters in order, so the ones appended below win over any alias left in the URL.
        let replaced = |k: &str| (k == "sslrootcert" && ca.is_some()) || (k == "sslmode" && !local);
        let keep: Vec<(String, String)> =
            u.query_pairs().filter(|(k, _)| !replaced(k)).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        let mut q = u.query_pairs_mut();
        q.clear().extend_pairs(keep);
        if !local {
            q.append_pair("sslmode", "verify-full");
        }
        if let Some(ca) = ca {
            q.append_pair("sslrootcert", &ca);
        }
        drop(q);
        Ok(u.to_string())
    }

    /// The URL without a password, for messages.
    pub fn display_url(&self) -> String {
        match url::Url::parse(&self.url) {
            Ok(mut u) => {
                let _ = u.set_password(None);
                u.to_string()
            }
            Err(_) => "invalid database url".into(),
        }
    }
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
    /// Own Content View database; overrides `[database]`. `url = "local"` keeps the profile in the local file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<Database>,
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
            database: None,
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
        if let Some(d) = &self.database {
            d.validate().context("database")?;
        }
        Ok(())
    }

    /// `scheme://host:port` used to compare servers (self-replication guard).
    pub fn origin(&self) -> String {
        url::Url::parse(&self.url).map(|u| u.origin().ascii_serialization()).unwrap_or_else(|_| self.url.clone())
    }

    /// Scope of Content View rows in a shared database: the same server under any profile name.
    pub fn server_key(&self) -> String {
        format!("{}{}#{}", self.origin(), self.api_root, self.domain.as_deref().unwrap_or("default"))
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
                // The toml error text quotes the source line, which can hold a password: report its message and line only.
                let cfg: Config = toml::from_str(&s).map_err(|e| {
                    let line = e.span().map_or(0, |r| s[..r.start].matches('\n').count() + 1);
                    anyhow::anyhow!("invalid config {} line {line}: {}", path.display(), e.message())
                })?;
                for (name, p) in &cfg.profiles {
                    p.validate().with_context(|| format!("profile {name}"))?;
                }
                if let Some(d) = &cfg.database {
                    d.validate().context("[database]")?;
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

    /// The external database of a profile and the keyring user of its secret: the profile's own `database`,
    /// else `[database]`. `None`: the local file (nothing set, or the `local` marker). An unknown name gets `[database]`.
    pub fn database_for(&self, profile: &str) -> Option<(Database, String)> {
        let (d, user) = match self.profiles.get(profile).and_then(|p| p.database.as_ref()) {
            Some(d) => (d, database_keyring_user(profile)),
            None => (self.database.as_ref()?, DATABASE_KEYRING_USER.to_string()),
        };
        (!d.is_local()).then(|| (d.clone(), user))
    }

    /// `import-cv` creates objects in an external database: a read-only profile must not do that.
    pub fn check_shared_write(&self, profile: &str) -> Result<()> {
        if self.database_for(profile).is_some() && self.profiles.get(profile).is_some_and(|p| p.read_only) {
            bail!("profile {profile} is read-only; import-cv writes to the shared database");
        }
        Ok(())
    }

    /// `db copy` writes to the target database, also the local file (as the TUI): a read-only profile must not run it.
    pub fn check_copy_write(&self, profile: &str) -> Result<()> {
        if self.profiles.get(profile).is_some_and(|p| p.read_only) {
            bail!("profile {profile} is read-only; db copy writes to the database");
        }
        Ok(())
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

    #[test]
    fn shared_database_writes_need_a_writable_profile() {
        let mut cfg: Config = toml::from_str(EXAMPLE).unwrap();
        assert!(cfg.check_shared_write("prod").is_ok(), "the local file: nothing changes");
        cfg.database = Some(Database { url: "postgres://u@h/x".into(), secret: None, ca_cert: None });
        assert!(cfg.check_shared_write("lab").is_ok() && cfg.check_copy_write("lab").is_ok());
        assert_eq!(
            cfg.check_shared_write("prod").unwrap_err().to_string(),
            "profile prod is read-only; import-cv writes to the shared database"
        );
        assert_eq!(cfg.check_copy_write("prod").unwrap_err().to_string(), "profile prod is read-only; db copy writes to the database");
        // A profile's own database counts too, and `local` leaves the shared one.
        cfg.database = None;
        cfg.profiles.get_mut("prod").unwrap().database = Some(Database { url: "postgres://u@h/x".into(), secret: None, ca_cert: None });
        assert!(cfg.check_shared_write("prod").is_err());
        cfg.database = Some(Database { url: "postgres://u@h/x".into(), secret: None, ca_cert: None });
        cfg.profiles.get_mut("prod").unwrap().database = Some(Database { url: LOCAL_DATABASE.into(), secret: None, ca_cert: None });
        assert!(cfg.check_shared_write("prod").is_ok(), "import-cv into the local file is allowed");
        assert!(cfg.check_copy_write("prod").is_err(), "db copy refuses a read-only profile, also on the local file");
    }

    #[test]
    fn database_password_in_url_rejected() {
        let d = Database { url: "postgres://u:pw@db.example.com/x".into(), secret: None, ca_cert: None };
        assert!(d.validate().unwrap_err().to_string().contains("secret"));
        let c: Result<Config, _> = toml::from_str("[database]\nurl = \"postgres://u@h/x\"\npassword = \"x\"\n");
        assert!(c.is_err());
        // sqlx also reads a password (and the client key password) from the query string.
        for q in ["password=pw", "PassWord=pw", "sslpassword=pw", "SSLPASSWORD=pw"] {
            let d = Database { url: format!("postgres://u@h/x?sslmode=require&{q}"), secret: None, ca_cert: None };
            assert_eq!(d.validate().unwrap_err().to_string(), "put the database password in secret, not in the URL", "{q}");
        }
        assert!(Database { url: "mysql://u@h/x".into(), secret: None, ca_cert: None }.validate().is_err());
    }

    #[test]
    fn connect_url_keeps_every_password_byte() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("pw");
        let d = Database { url: "postgres://u@127.0.0.1/x".into(), secret: Some(format!("file:{}", f.display())), ca_cert: None };
        for pw in ["a%41b", "p@ss:w/rd", "p@ss word+"] {
            std::fs::write(&f, pw).unwrap();
            let u = url::Url::parse(&d.connect_url(DATABASE_KEYRING_USER).unwrap()).unwrap();
            // sqlx percent-decodes the password (the encoded form has no `+`, `&` or `=`, so this decodes the same).
            let got = url::form_urlencoded::parse(u.password().unwrap().as_bytes()).next().unwrap().0;
            assert_eq!(got, pw);
        }
    }

    #[test]
    fn parse_errors_do_not_echo_the_config_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for text in [
            "[database]\nurl = \"postgres://u@h/x\"\npassword = \"hunter2\"\n",
            "[profiles.x]\nurl = \"http://h\"\npassword = \"hunter2\"\n",
        ] {
            std::fs::write(&path, text).unwrap();
            let e = format!("{:#}", Config::load(&path).unwrap_err());
            assert!(!e.contains("hunter2"), "{e}");
            assert!(e.starts_with(&format!("invalid config {} line 3: unknown field `password`, expected ", path.display())), "{e}");
        }
    }

    #[test]
    fn connect_url_tls_rules() {
        unsafe { std::env::set_var("PTUI_TEST_DBPW", "p@ss word") };
        let d = |u: &str| Database { url: u.into(), secret: Some("env:PTUI_TEST_DBPW".into()), ca_cert: Some("/ca.pem".into()) };
        let remote = d("postgres://u@db.example.com:5432/x?sslmode=disable").connect_url(DATABASE_KEYRING_USER).unwrap();
        assert!(remote.contains("sslmode=verify-full") && !remote.contains("sslmode=disable"));
        assert!(remote.contains("sslrootcert=%2Fca.pem") || remote.contains("sslrootcert=/ca.pem"));
        assert!(remote.contains("p%40ss%20word"));
        assert!(!d("postgres://u@127.0.0.1/x").connect_url(DATABASE_KEYRING_USER).unwrap().contains("verify-full"));
        assert!(!d("postgres://u@db.example.com/x").display_url().contains("p%40ss"));
        // sqlx connects to the `host` parameter, not the URL host; a socket path stays on the machine.
        assert!(d("postgres://u@127.0.0.1/x?host=db.example.com").connect_url(DATABASE_KEYRING_USER).unwrap().contains("verify-full"));
        assert!(!d("postgres://u@%2Frun%2Fpostgresql/x").connect_url(DATABASE_KEYRING_USER).unwrap().contains("verify-full"));
        assert!(
            !d("postgres://u@db.example.com/x?host=/run/postgresql").connect_url(DATABASE_KEYRING_USER).unwrap().contains("verify-full")
        );
    }

    #[test]
    fn database_for_resolution_order() {
        let mut cfg: Config = toml::from_str(EXAMPLE).unwrap();
        assert_eq!(cfg.database_for("lab"), None, "nothing set: the local file");
        let top = Database { url: "postgres://u@db.example.com/x".into(), secret: Some("keyring".into()), ca_cert: None };
        cfg.database = Some(top.clone());
        assert_eq!(cfg.database_for("lab"), Some((top.clone(), "[database]".into())));
        assert_eq!(cfg.database_for("gone"), Some((top.clone(), "[database]".into())), "unknown profile: the default");
        let own = Database { url: "sqlite:///srv/cv.db".into(), secret: None, ca_cert: None };
        cfg.profiles.get_mut("prod").unwrap().database = Some(own.clone());
        assert_eq!(cfg.database_for("prod"), Some((own, "[database]:prod".into())));
        cfg.profiles.get_mut("lab").unwrap().database = Some(Database { url: LOCAL_DATABASE.into(), secret: None, ca_cert: None });
        assert_eq!(cfg.database_for("lab"), None, "local wins over [database]");
    }

    #[test]
    fn profile_database_roundtrip_and_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let head = "[profiles.x]\nurl = \"http://h\"\nauth = \"none\"\n[profiles.x.database]\n";
        std::fs::write(&path, format!("{head}url = \"local\"\n")).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.profiles["x"].database.as_ref().unwrap().is_local());
        cfg.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), cfg);
        std::fs::write(&path, format!("{head}url = \"postgres://u:hunter2@h/x\"\n")).unwrap();
        let e = format!("{:#}", Config::load(&path).unwrap_err());
        assert!(e.contains("profile x") && e.contains("put the database password in secret") && !e.contains("hunter2"), "{e}");
    }

    #[test]
    fn database_keyring_user_cannot_be_a_profile_name() {
        assert_eq!(DATABASE_KEYRING_USER, "[database]");
        assert_eq!(database_keyring_user("prod"), "[database]:prod");
        assert!(crate::store::validate_name(&database_keyring_user("prod")).is_err());
        // Profiles created in the TUI pass validate_name, so they never share this keyring entry.
        assert!(crate::store::validate_name(DATABASE_KEYRING_USER).is_err());
        assert!(crate::store::validate_name("database").is_ok(), "the old user was a valid profile name");
    }

    #[test]
    fn server_key_normalizes() {
        let p = |u: &str| Profile { url: u.into(), ..Default::default() };
        assert_eq!(p("https://Pulp.Example.com:443").server_key(), p("https://pulp.example.com").server_key());
        assert_eq!(p("https://pulp.example.com").server_key(), "https://pulp.example.com/pulp/#default");
    }
}
