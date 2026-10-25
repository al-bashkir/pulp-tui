//! Store (sqlx) for application-owned state that Pulp does not persist: tracked task
//! references, Content View definitions/versions, lifecycle environments, bindings,
//! workflow checkpoints and an audit trail. The local SQLite file holds everything; with a
//! `[database]` the Content View data lives in that external PostgreSQL or SQLite database.
//! Rows are scoped by profile name locally and by server key (`Store::scope`) externally.
//!
//! Migrations: `MIGRATIONS` (SQLite, `PRAGMA user_version`) and `PG_MIGRATIONS` (PostgreSQL,
//! `schema_version` table) grow together. The local file migrates on open; an external
//! database only with `pulp-tui db migrate`.

use crate::config::Profile;
use crate::filters::FilterRule;
use crate::pulp::RepoType;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::any::{AnyConnectOptions, AnyPoolOptions, AnyRow};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{AnyConnection, AnyPool, ConnectOptions, Connection, Executor, Row};
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MIGRATIONS: &[&str] = &[
    // 1: initial schema
    "CREATE TABLE tasks(
        profile TEXT NOT NULL, href TEXT NOT NULL, label TEXT NOT NULL, state TEXT NOT NULL,
        created_at TEXT NOT NULL, finished_at TEXT, PRIMARY KEY(profile, href));
     CREATE TABLE content_views(
        id INTEGER PRIMARY KEY, profile TEXT NOT NULL, name TEXT NOT NULL,
        description TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL, UNIQUE(profile, name));
     CREATE TABLE cv_repos(
        cv_id INTEGER NOT NULL REFERENCES content_views(id) ON DELETE CASCADE,
        repo_href TEXT NOT NULL, repo_name TEXT NOT NULL, repo_type TEXT NOT NULL,
        filters TEXT NOT NULL DEFAULT '[]', PRIMARY KEY(cv_id, repo_href));
     CREATE TABLE cv_versions(
        id INTEGER PRIMARY KEY, cv_id INTEGER NOT NULL REFERENCES content_views(id) ON DELETE CASCADE,
        number INTEGER NOT NULL, notes TEXT NOT NULL DEFAULT '', state TEXT NOT NULL,
        created_at TEXT NOT NULL, finished_at TEXT, error TEXT, UNIQUE(cv_id, number));
     CREATE TABLE cv_version_repos(
        version_id INTEGER NOT NULL REFERENCES cv_versions(id) ON DELETE CASCADE,
        repo_href TEXT NOT NULL, repo_name TEXT NOT NULL, repo_type TEXT NOT NULL, filters TEXT NOT NULL,
        source_version TEXT, derived_repo TEXT, derived_version TEXT, publication TEXT,
        pending_task TEXT, content_count INTEGER, PRIMARY KEY(version_id, repo_href));
     CREATE TABLE environments(
        id INTEGER PRIMARY KEY, profile TEXT NOT NULL, name TEXT NOT NULL,
        prior_id INTEGER REFERENCES environments(id), protected INTEGER NOT NULL DEFAULT 0,
        UNIQUE(profile, name));
     CREATE TABLE bindings(
        env_id INTEGER NOT NULL REFERENCES environments(id),
        cv_id INTEGER NOT NULL REFERENCES content_views(id) ON DELETE CASCADE,
        version_id INTEGER NOT NULL REFERENCES cv_versions(id),
        state TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(env_id, cv_id));
     CREATE TABLE binding_repos(
        env_id INTEGER NOT NULL, cv_id INTEGER NOT NULL REFERENCES content_views(id) ON DELETE CASCADE,
        repo_href TEXT NOT NULL, repo_name TEXT NOT NULL, repo_type TEXT NOT NULL,
        distribution TEXT NOT NULL, base_path TEXT NOT NULL, publication TEXT,
        PRIMARY KEY(env_id, cv_id, repo_href));
     CREATE TABLE audit(
        id INTEGER PRIMARY KEY, profile TEXT NOT NULL, at TEXT NOT NULL, action TEXT NOT NULL, detail TEXT NOT NULL);
     CREATE TABLE replication_runs(
        id INTEGER PRIMARY KEY, profile TEXT NOT NULL, source_profile TEXT NOT NULL,
        distribution TEXT NOT NULL, state TEXT NOT NULL, at TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '');",
    // 2: database id and settings, operation locks, audit actor
    "CREATE TABLE locks(
        cv_id INTEGER PRIMARY KEY REFERENCES content_views(id) ON DELETE CASCADE,
        owner TEXT NOT NULL, token TEXT NOT NULL, operation TEXT NOT NULL, acquired_at TEXT NOT NULL, renewed_at INTEGER NOT NULL);
     CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
     CREATE TABLE scope_settings(scope TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(scope, key));
     ALTER TABLE audit ADD COLUMN actor TEXT NOT NULL DEFAULT '';",
    // 3: environment labels (the served path segment) and the base path prefix of servers that served before it was stored.
    // Names pass `validate_name`, so `lower(name)` is exactly `slug(name).to_lowercase()`: no path moves.
    "ALTER TABLE environments ADD COLUMN label TEXT NOT NULL DEFAULT '';
     UPDATE environments SET label = lower(name);
     INSERT INTO scope_settings(scope, key, value)
        SELECT c.profile, 'cv_base_path_prefix', MIN(substr(b.base_path, 1, instr(b.base_path, '/') - 1))
        FROM binding_repos b JOIN content_views c ON c.id = b.cv_id
        WHERE instr(b.base_path, '/') > 0
          AND NOT EXISTS (SELECT 1 FROM scope_settings s WHERE s.scope = c.profile AND s.key = 'cv_base_path_prefix')
        GROUP BY c.profile;",
    // 4: composite Content Views, their components, the component versions a composite version used, borrowed rows.
    // `component_id` has no cascade on purpose: the database also refuses to delete a view that is still a component.
    "ALTER TABLE content_views ADD COLUMN composite INTEGER NOT NULL DEFAULT 0;
     ALTER TABLE content_views ADD COLUMN auto_publish INTEGER NOT NULL DEFAULT 0;
     ALTER TABLE cv_version_repos ADD COLUMN borrowed INTEGER NOT NULL DEFAULT 0;
     CREATE TABLE cv_components(
        composite_id INTEGER NOT NULL REFERENCES content_views(id) ON DELETE CASCADE,
        component_id INTEGER NOT NULL REFERENCES content_views(id),
        version_id INTEGER REFERENCES cv_versions(id),
        PRIMARY KEY(composite_id, component_id));
     CREATE TABLE cv_version_components(
        version_id INTEGER NOT NULL REFERENCES cv_versions(id) ON DELETE CASCADE,
        component_version_id INTEGER NOT NULL REFERENCES cv_versions(id) ON DELETE CASCADE,
        PRIMARY KEY(version_id, component_version_id));",
    // 5: Deb signing choice of a Content View publish ('' = same as source, 'none', or a signing service href)
    "ALTER TABLE cv_version_repos ADD COLUMN signing TEXT NOT NULL DEFAULT '';",
];

/// `MIGRATIONS` for PostgreSQL: every integer is BIGINT (sqlx `Any` reads SQLite integers as
/// i64; flags too), ids are identities, and the version is a row in `schema_version`.
pub const PG_MIGRATIONS: &[&str] = &[
    // 1: initial schema
    "CREATE TABLE schema_version(version BIGINT NOT NULL);
     CREATE TABLE tasks(
        profile TEXT NOT NULL, href TEXT NOT NULL, label TEXT NOT NULL, state TEXT NOT NULL,
        created_at TEXT NOT NULL, finished_at TEXT, PRIMARY KEY(profile, href));
     CREATE TABLE content_views(
        id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, profile TEXT NOT NULL, name TEXT NOT NULL,
        description TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL, UNIQUE(profile, name));
     CREATE TABLE cv_repos(
        cv_id BIGINT NOT NULL REFERENCES content_views(id) ON DELETE CASCADE,
        repo_href TEXT NOT NULL, repo_name TEXT NOT NULL, repo_type TEXT NOT NULL,
        filters TEXT NOT NULL DEFAULT '[]', PRIMARY KEY(cv_id, repo_href));
     CREATE TABLE cv_versions(
        id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
        cv_id BIGINT NOT NULL REFERENCES content_views(id) ON DELETE CASCADE,
        number BIGINT NOT NULL, notes TEXT NOT NULL DEFAULT '', state TEXT NOT NULL,
        created_at TEXT NOT NULL, finished_at TEXT, error TEXT, UNIQUE(cv_id, number));
     CREATE TABLE cv_version_repos(
        version_id BIGINT NOT NULL REFERENCES cv_versions(id) ON DELETE CASCADE,
        repo_href TEXT NOT NULL, repo_name TEXT NOT NULL, repo_type TEXT NOT NULL, filters TEXT NOT NULL,
        source_version TEXT, derived_repo TEXT, derived_version TEXT, publication TEXT,
        pending_task TEXT, content_count BIGINT, PRIMARY KEY(version_id, repo_href));
     CREATE TABLE environments(
        id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, profile TEXT NOT NULL, name TEXT NOT NULL,
        prior_id BIGINT REFERENCES environments(id), protected BIGINT NOT NULL DEFAULT 0,
        UNIQUE(profile, name));
     CREATE TABLE bindings(
        env_id BIGINT NOT NULL REFERENCES environments(id),
        cv_id BIGINT NOT NULL REFERENCES content_views(id) ON DELETE CASCADE,
        version_id BIGINT NOT NULL REFERENCES cv_versions(id),
        state TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(env_id, cv_id));
     CREATE TABLE binding_repos(
        env_id BIGINT NOT NULL, cv_id BIGINT NOT NULL REFERENCES content_views(id) ON DELETE CASCADE,
        repo_href TEXT NOT NULL, repo_name TEXT NOT NULL, repo_type TEXT NOT NULL,
        distribution TEXT NOT NULL, base_path TEXT NOT NULL, publication TEXT,
        PRIMARY KEY(env_id, cv_id, repo_href));
     CREATE TABLE audit(
        id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, profile TEXT NOT NULL, at TEXT NOT NULL,
        action TEXT NOT NULL, detail TEXT NOT NULL);
     CREATE TABLE replication_runs(
        id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, profile TEXT NOT NULL, source_profile TEXT NOT NULL,
        distribution TEXT NOT NULL, state TEXT NOT NULL, at TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '');",
    // 2: database id and settings, operation locks, audit actor
    "CREATE TABLE locks(
        cv_id BIGINT PRIMARY KEY REFERENCES content_views(id) ON DELETE CASCADE,
        owner TEXT NOT NULL, token TEXT NOT NULL, operation TEXT NOT NULL, acquired_at TEXT NOT NULL, renewed_at BIGINT NOT NULL);
     CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
     CREATE TABLE scope_settings(scope TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(scope, key));
     ALTER TABLE audit ADD COLUMN actor TEXT NOT NULL DEFAULT '';",
    // 3: environment labels and the seeded base path prefix (see `MIGRATIONS`)
    "ALTER TABLE environments ADD COLUMN label TEXT NOT NULL DEFAULT '';
     UPDATE environments SET label = lower(name);
     INSERT INTO scope_settings(scope, key, value)
        SELECT c.profile, 'cv_base_path_prefix', MIN(split_part(b.base_path, '/', 1))
        FROM binding_repos b JOIN content_views c ON c.id = b.cv_id
        WHERE strpos(b.base_path, '/') > 0
          AND NOT EXISTS (SELECT 1 FROM scope_settings s WHERE s.scope = c.profile AND s.key = 'cv_base_path_prefix')
        GROUP BY c.profile;",
    // 4: composite Content Views (see `MIGRATIONS`)
    "ALTER TABLE content_views ADD COLUMN composite BIGINT NOT NULL DEFAULT 0;
     ALTER TABLE content_views ADD COLUMN auto_publish BIGINT NOT NULL DEFAULT 0;
     ALTER TABLE cv_version_repos ADD COLUMN borrowed BIGINT NOT NULL DEFAULT 0;
     CREATE TABLE cv_components(
        composite_id BIGINT NOT NULL REFERENCES content_views(id) ON DELETE CASCADE,
        component_id BIGINT NOT NULL REFERENCES content_views(id),
        version_id BIGINT REFERENCES cv_versions(id),
        PRIMARY KEY(composite_id, component_id));
     CREATE TABLE cv_version_components(
        version_id BIGINT NOT NULL REFERENCES cv_versions(id) ON DELETE CASCADE,
        component_version_id BIGINT NOT NULL REFERENCES cv_versions(id) ON DELETE CASCADE,
        PRIMARY KEY(version_id, component_version_id));",
    // 5: Deb signing choice of a Content View publish ('' = same as source, 'none', or a signing service href)
    "ALTER TABLE cv_version_repos ADD COLUMN signing TEXT NOT NULL DEFAULT '';",
];

/// A Content View lock not renewed for this long is expired; the next operator takes it over.
pub const LOCK_TTL_SECS: i64 = 120;
/// How often the holder renews its lock.
pub const LOCK_RENEW_SECS: u64 = 30;

/// `pg_advisory_lock` key held while `db migrate` runs.
const MIGRATE_LOCK: i64 = 7_301_955;
/// Limit for reaching an external database (a dropped packet otherwise waits for the OS TCP timeout).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Sqlite,
    Postgres,
}

/// `Debug` shows pool sizes only (sqlx does not print the connect URL).
#[derive(Debug)]
pub struct Store {
    pool: AnyPool,
    backend: Backend,
    /// The configured `[database]`: Content View rows are scoped by server key, not profile name.
    pub external: bool,
    /// `meta` key `db_id`; empty only for a test file kept at schema 1.
    db_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRef {
    pub href: String,
    pub label: String,
    pub state: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentView {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub created_at: String,
    /// Has components instead of repositories.
    pub composite: bool,
    /// A composite: publish after a component it uses as latest publishes.
    pub auto_publish: bool,
}

/// A component of a composite; `pinned` is a version id, `None` is the latest published version.
#[derive(Debug, Clone, PartialEq)]
pub struct Component {
    pub cv_id: i64,
    pub name: String,
    pub pinned: Option<i64>,
}

/// A component version that a composite version used.
#[derive(Debug, Clone, PartialEq)]
pub struct UsedVersion {
    pub cv_id: i64,
    pub name: String,
    pub version_id: i64,
    pub number: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CvRepo {
    pub repo_href: String,
    pub repo_name: String,
    pub repo_type: RepoType,
    #[serde(default)]
    pub filters: Vec<FilterRule>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CvVersion {
    pub id: i64,
    pub cv_id: i64,
    pub number: i64,
    pub notes: String,
    /// publishing | published | failed | deleting | deleted
    pub state: String,
    pub created_at: String,
    pub finished_at: Option<String>,
    pub error: Option<String>,
}

/// Per-repository checkpoint of a Content View version publish.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CvVersionRepo {
    pub version_id: i64,
    pub repo: Option<CvRepo>,
    pub source_version: Option<String>,
    pub derived_repo: Option<String>,
    pub derived_version: Option<String>,
    pub publication: Option<String>,
    pub pending_task: Option<String>,
    pub content_count: Option<i64>,
    /// A composite's row: the objects belong to a component version and are never deleted with it.
    pub borrowed: bool,
    /// Deb signing choice of the publish: "" = same as source, "none", or a signing service href.
    pub signing: String,
}

impl CvVersionRepo {
    pub fn repo(&self) -> &CvRepo {
        self.repo.as_ref().expect("loaded from store")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Environment {
    pub id: i64,
    pub name: String,
    pub prior_id: Option<i64>,
    pub protected: bool,
    /// The path segment of this environment's served paths; unique per scope.
    pub label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Binding {
    pub env_id: i64,
    pub cv_id: i64,
    pub version_id: i64,
    /// promoting | active | partial | failed
    pub state: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BindingRepo {
    pub env_id: i64,
    pub cv_id: i64,
    pub repo_href: String,
    pub repo_name: String,
    pub repo_type: RepoType,
    pub distribution: String,
    pub base_path: String,
    pub publication: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuditEntry {
    pub at: String,
    pub action: String,
    pub detail: String,
    /// `user@host`; empty for rows written before schema 2.
    pub actor: String,
}

/// Who holds a Content View lock.
#[derive(Debug, Clone, PartialEq)]
pub struct LockHolder {
    pub owner: String,
    pub operation: String,
    /// RFC 3339.
    pub acquired_at: String,
}

/// An external database with no schema (`from` 0) or an older one: `pulp-tui db migrate` (or the TUI offer) fixes it.
#[derive(Debug, thiserror::Error)]
#[error("{display}: {}; run pulp-tui db migrate", match .from {
    0 => "database is not initialized".to_string(),
    v => format!("database schema {v} is older than this pulp-tui ({})", .to),
})]
pub struct NotMigrated {
    pub display: String,
    pub from: i64,
    pub to: i64,
}

pub const LIBRARY: &str = "Library";

fn repo_type(s: &str) -> Result<RepoType> {
    RepoType::ALL.into_iter().find(|t| t.component() == s).ok_or_else(|| anyhow!("unknown repository type {s:?}"))
}

fn type_str(t: RepoType) -> &'static str {
    t.component()
}

fn migrations(b: Backend) -> &'static [&'static str] {
    match b {
        Backend::Sqlite => MIGRATIONS,
        Backend::Postgres => PG_MIGRATIONS,
    }
}

/// SQLite: `PRAGMA user_version`. PostgreSQL: the `schema_version` row; 0 before the first migration.
async fn version(c: &mut AnyConnection, b: Backend) -> Result<i64> {
    if b == Backend::Sqlite {
        return Ok(sqlx::query_scalar("PRAGMA user_version").fetch_one(&mut *c).await?);
    }
    let found: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = current_schema() AND table_name = 'schema_version'",
    )
    .fetch_one(&mut *c)
    .await?;
    if found == 0 {
        return Ok(0);
    }
    Ok(sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM schema_version").fetch_one(&mut *c).await?)
}

/// Apply migrations up to `upto`, each batch and its version number in one transaction. Returns the version before.
async fn migrate_on(c: &mut AnyConnection, b: Backend, upto: usize) -> Result<i64> {
    let list = migrations(b);
    let v = version(c, b).await?;
    if v as usize > list.len() {
        bail!("database schema {v} is newer than this pulp-tui ({}); upgrade pulp-tui", list.len());
    }
    for (i, m) in list.iter().enumerate().take(upto).skip(v as usize) {
        let mut tx = c.begin().await?;
        // `Executor::execute`, not `RawSql::execute`: rustc cannot prove the latter `Send` in a spawned job.
        tx.execute(sqlx::raw_sql(m)).await.with_context(|| format!("migration {}", i + 1))?;
        let set = match b {
            Backend::Sqlite => format!("PRAGMA user_version = {}", i + 1),
            Backend::Postgres => format!("DELETE FROM schema_version; INSERT INTO schema_version(version) VALUES ({})", i + 1),
        };
        tx.execute(sqlx::raw_sql(&set)).await?;
        tx.commit().await?;
    }
    Ok(v)
}

impl Store {
    pub async fn open(path: &Path) -> Result<Self> {
        Self::open_file(path, MIGRATIONS.len()).await
    }

    /// A file as an older pulp-tui left it: only the first `n` migrations.
    #[cfg(test)]
    async fn open_at_version(path: &Path, n: usize) -> Result<Self> {
        Self::open_file(path, n).await
    }

    async fn open_file(path: &Path, migrations: usize) -> Result<Self> {
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let url = SqliteConnectOptions::new().filename(path).create_if_missing(true).to_url_lossy();
        let s = Self::connect(AnyConnectOptions::from_url(&url)?, AnyPoolOptions::new(), Backend::Sqlite)
            .await
            .with_context(|| format!("open {}", path.display()))?;
        sqlx::raw_sql("PRAGMA journal_mode=WAL").execute(&s.pool).await?;
        s.migrate(migrations).await?;
        // A partly migrated test file has no `meta` table yet.
        if migrations < MIGRATIONS.len() { Ok(s) } else { s.with_db_id().await }
    }

    pub async fn open_in_memory() -> Result<Self> {
        // Each SQLite memory connection is a separate database: keep exactly one, forever.
        let pool = AnyPoolOptions::new().max_connections(1).min_connections(1).idle_timeout(None).max_lifetime(None);
        let s = Self::connect("sqlite::memory:".parse()?, pool, Backend::Sqlite).await?;
        s.migrate(MIGRATIONS.len()).await?;
        s.with_db_id().await
    }

    /// The configured `[database]`. A normal start never changes its schema; `db migrate` does.
    /// Errors name `display`, never `connect` (it holds the password).
    pub async fn open_external(connect: &str, display: &str) -> Result<Self> {
        let not_migrated = |from, to| anyhow::Error::new(NotMigrated { display: display.into(), from, to });
        // sqlx would fail on a missing file; only `db migrate` creates it.
        if connect.starts_with("sqlite:") && !connect.contains("mode=") {
            let opts = SqliteConnectOptions::from_str(connect).map_err(|e| anyhow!("{display}: {e}"))?;
            if !opts.get_filename().exists() {
                return Err(not_migrated(0, MIGRATIONS.len() as i64));
            }
        }
        let s = Self::external(connect, display).await?;
        let v = s.schema_version().await.map_err(|e| anyhow!("{display}: {e:#}"))?;
        let m = migrations(s.backend).len() as i64;
        match v {
            v if v < m => Err(not_migrated(v, m)),
            v if v > m => bail!("{display}: database schema {v} is newer than this pulp-tui ({m}); upgrade pulp-tui"),
            _ => s.with_db_id().await.map_err(|e| anyhow!("{display}: {e:#}")),
        }
    }

    /// `pulp-tui db migrate`: create or upgrade the external schema. Returns the versions before and after.
    pub async fn migrate_external(connect: &str, display: &str) -> Result<(i64, i64)> {
        // Only `db migrate` creates a new SQLite file.
        let rwc = format!("{connect}{}mode=rwc", if connect.contains('?') { '&' } else { '?' });
        let connect = if connect.starts_with("sqlite:") && !connect.contains("mode=") { &rwc } else { connect };
        let s = Self::external(connect, display).await?;
        let res = async {
            let mut c = s.pool.acquire().await?;
            if s.backend == Backend::Sqlite {
                c.execute(sqlx::raw_sql("PRAGMA journal_mode=WAL")).await?;
                // A second `db migrate` fails on its batch transaction (table exists) and rolls back.
                return migrate_on(&mut c, s.backend, usize::MAX).await;
            }
            // Session lock on the migration connection: a second `db migrate` waits here.
            sqlx::query("SELECT pg_advisory_lock($1)").bind(MIGRATE_LOCK).execute(&mut *c).await?;
            let res = migrate_on(&mut c, s.backend, usize::MAX).await;
            let _ = sqlx::query("SELECT pg_advisory_unlock($1)").bind(MIGRATE_LOCK).execute(&mut *c).await;
            res
        }
        .await;
        let from = res.map_err(|e| anyhow!("{display}: {e:#}"))?;
        Ok((from, migrations(s.backend).len() as i64))
    }

    async fn external(connect: &str, display: &str) -> Result<Self> {
        let backend = if connect.starts_with("sqlite:") { Backend::Sqlite } else { Backend::Postgres };
        let fail = |e: sqlx::Error| anyhow!("{display}: {e}");
        sqlx::any::install_default_drivers();
        let opts: AnyConnectOptions = connect.parse().map_err(fail)?;
        // One plain attempt first: the pool retries a refused connection until its timeout and then only says "timed out".
        match tokio::time::timeout(CONNECT_TIMEOUT, opts.connect()).await {
            Ok(Ok(c)) => drop(c.close().await),
            Ok(Err(e)) => return Err(fail(e)),
            Err(_) => bail!("{display}: no connection after {}s", CONNECT_TIMEOUT.as_secs()),
        }
        let s = Self::connect(opts, AnyPoolOptions::new().acquire_timeout(CONNECT_TIMEOUT), backend)
            .await
            .map_err(|e| anyhow!("{display}: {e:#}"))?;
        Ok(Self { external: true, ..s })
    }

    async fn connect(opts: AnyConnectOptions, pool: AnyPoolOptions, backend: Backend) -> Result<Self> {
        sqlx::any::install_default_drivers(); // guarded by a std::sync::Once inside sqlx
        let pool = if backend == Backend::Sqlite {
            pool.after_connect(|conn, _| {
                Box::pin(async move {
                    conn.execute(sqlx::raw_sql("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000")).await?;
                    Ok(())
                })
            })
        } else {
            pool
        };
        Ok(Self { pool: pool.connect_with(opts).await?, backend, external: false, db_id: String::new() })
    }

    /// Create the database id on first use, then read it (DML only: a restricted role may do it).
    async fn with_db_id(mut self) -> Result<Self> {
        sqlx::query("INSERT INTO meta(key, value) VALUES ('db_id', $1) ON CONFLICT DO NOTHING")
            .bind(new_db_id())
            .execute(&self.pool)
            .await?;
        self.db_id = sqlx::query_scalar("SELECT value FROM meta WHERE key = 'db_id'").fetch_one(&self.pool).await?;
        Ok(self)
    }

    /// Identifies this database in locks and in the database ID label.
    pub fn db_id(&self) -> &str {
        &self.db_id
    }

    /// Other ids this database answers to (`meta` keys `alias:<id>`).
    pub async fn aliases(&self) -> Result<Vec<String>> {
        let keys: Vec<String> =
            sqlx::query_scalar("SELECT key FROM meta WHERE key LIKE 'alias:%' ORDER BY key").fetch_all(&self.pool).await?;
        Ok(keys.into_iter().filter_map(|k| k.strip_prefix("alias:").map(str::to_string)).collect())
    }

    pub async fn add_alias(&self, id: &str) -> Result<()> {
        sqlx::query("INSERT INTO meta(key, value) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(format!("alias:{id}"))
            .bind(now())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn migrate(&self, upto: usize) -> Result<()> {
        migrate_on(&mut *self.pool.acquire().await?, self.backend, upto).await.map(drop)
    }

    pub async fn schema_version(&self) -> Result<i64> {
        version(&mut *self.pool.acquire().await?, self.backend).await
    }

    /// Scope of Content View rows: the server key in an external database, else the profile name.
    pub fn scope(&self, name: &str, p: &Profile) -> String {
        if self.external { p.server_key() } else { name.to_string() }
    }

    // ---- tasks -------------------------------------------------------------------

    pub async fn track_task(&self, profile: &str, href: &str, label: &str, state: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO tasks(profile, href, label, state, created_at) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT(profile, href) DO UPDATE SET state = excluded.state",
        )
        .bind(profile)
        .bind(href)
        .bind(label)
        .bind(state)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn finish_task(&self, profile: &str, href: &str, state: &str) -> Result<()> {
        sqlx::query("UPDATE tasks SET state = $3, finished_at = $4 WHERE profile = $1 AND href = $2")
            .bind(profile)
            .bind(href)
            .bind(state)
            .bind(now())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Recent task references (unfinished first) to reattach after a restart.
    pub async fn tasks(&self, profile: &str, limit: usize) -> Result<Vec<TaskRef>> {
        let rows = sqlx::query(
            "SELECT href, label, state, created_at FROM tasks WHERE profile = $1
             ORDER BY finished_at IS NOT NULL, created_at DESC LIMIT $2",
        )
        .bind(profile)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| Ok(TaskRef { href: r.try_get(0)?, label: r.try_get(1)?, state: r.try_get(2)?, created_at: r.try_get(3)? }))
            .collect()
    }

    // ---- content views -----------------------------------------------------------

    pub async fn create_cv(&self, profile: &str, name: &str, description: &str) -> Result<i64> {
        validate_name(name)?;
        let id =
            sqlx::query_scalar("INSERT INTO content_views(profile, name, description, created_at) VALUES ($1, $2, $3, $4) RETURNING id")
                .bind(profile)
                .bind(name)
                .bind(description)
                .bind(now())
                .fetch_one(&self.pool)
                .await
                .with_context(|| format!("content view {name} already exists?"))?;
        Ok(id)
    }

    pub async fn update_cv(&self, id: i64, description: &str) -> Result<()> {
        sqlx::query("UPDATE content_views SET description = $2 WHERE id = $1").bind(id).bind(description).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn delete_cv(&self, id: i64) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM bindings WHERE cv_id = $1").bind(id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM content_views WHERE id = $1").bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn content_views(&self, profile: &str) -> Result<Vec<ContentView>> {
        let rows = sqlx::query(
            "SELECT id, name, description, created_at, composite, auto_publish FROM content_views WHERE profile = $1 ORDER BY name",
        )
        .bind(profile)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(content_view_row).collect()
    }

    pub async fn content_view(&self, id: i64) -> Result<ContentView> {
        let r = sqlx::query("SELECT id, name, description, created_at, composite, auto_publish FROM content_views WHERE id = $1")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        content_view_row(&r)
    }

    // ---- composite content views -------------------------------------------------

    pub async fn create_composite(&self, profile: &str, name: &str, description: &str) -> Result<i64> {
        validate_name(name)?;
        let id = sqlx::query_scalar(
            "INSERT INTO content_views(profile, name, description, created_at, composite) VALUES ($1, $2, $3, $4, 1) RETURNING id",
        )
        .bind(profile)
        .bind(name)
        .bind(description)
        .bind(now())
        .fetch_one(&self.pool)
        .await
        .with_context(|| format!("content view {name} already exists?"))?;
        Ok(id)
    }

    pub async fn set_auto_publish(&self, id: i64, on: bool) -> Result<()> {
        sqlx::query("UPDATE content_views SET auto_publish = $2 WHERE id = $1").bind(id).bind(on as i64).execute(&self.pool).await?;
        Ok(())
    }

    /// Components of a composite, by name.
    pub async fn components(&self, composite_id: i64) -> Result<Vec<Component>> {
        let rows = sqlx::query(
            "SELECT c.id, c.name, k.version_id FROM cv_components k JOIN content_views c ON c.id = k.component_id
             WHERE k.composite_id = $1 ORDER BY c.name",
        )
        .bind(composite_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|r| Ok(Component { cv_id: r.try_get(0)?, name: r.try_get(1)?, pinned: r.try_get(2)? })).collect()
    }

    /// Add a component, or change its pin (a version id; `None` is latest).
    pub async fn set_component(&self, composite_id: i64, component_id: i64, pinned: Option<i64>) -> Result<()> {
        let (composite, component) = (self.content_view(composite_id).await?, self.content_view(component_id).await?);
        if !composite.composite {
            bail!("{} is not a composite content view", composite.name);
        }
        if component.composite {
            bail!("{} is a composite; a composite cannot be a component", component.name);
        }
        if let Some(id) = pinned {
            let v = self.version(id).await?;
            if v.cv_id != component_id || v.state != "published" {
                bail!("v{} is not a published version of {}", v.number, component.name);
            }
        }
        sqlx::query(
            "INSERT INTO cv_components(composite_id, component_id, version_id) VALUES ($1, $2, $3)
             ON CONFLICT(composite_id, component_id) DO UPDATE SET version_id = excluded.version_id",
        )
        .bind(composite_id)
        .bind(component_id)
        .bind(pinned)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn remove_component(&self, composite_id: i64, component_id: i64) -> Result<()> {
        sqlx::query("DELETE FROM cv_components WHERE composite_id = $1 AND component_id = $2")
            .bind(composite_id)
            .bind(component_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The component versions a composite version used, by name.
    pub async fn version_components(&self, version_id: i64) -> Result<Vec<UsedVersion>> {
        let rows = sqlx::query(
            "SELECT c.id, c.name, v.id, v.number FROM cv_version_components u JOIN cv_versions v ON v.id = u.component_version_id
             JOIN content_views c ON c.id = v.cv_id WHERE u.version_id = $1 ORDER BY c.name",
        )
        .bind(version_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| Ok(UsedVersion { cv_id: r.try_get(0)?, name: r.try_get(1)?, version_id: r.try_get(2)?, number: r.try_get(3)? }))
            .collect()
    }

    /// What keeps a component version: `{composite} v{n}` for each non-deleted composite version that used it,
    /// then `{composite} (pinned)` for each pin.
    pub async fn composite_users(&self, version_id: i64) -> Result<Vec<String>> {
        let used = sqlx::query(
            "SELECT c.name, v.number FROM cv_version_components u JOIN cv_versions v ON v.id = u.version_id
             JOIN content_views c ON c.id = v.cv_id WHERE u.component_version_id = $1 AND v.state != 'deleted'
             ORDER BY c.name, v.number",
        )
        .bind(version_id)
        .fetch_all(&self.pool)
        .await?;
        let pins: Vec<String> = sqlx::query_scalar(
            "SELECT c.name FROM cv_components k JOIN content_views c ON c.id = k.composite_id WHERE k.version_id = $1 ORDER BY c.name",
        )
        .bind(version_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = vec![];
        for r in &used {
            out.push(format!("{} v{}", r.try_get::<String, _>(0)?, r.try_get::<i64, _>(1)?));
        }
        out.extend(pins.into_iter().map(|c| format!("{c} (pinned)")));
        Ok(out)
    }

    /// What keeps a view: `{composite} (component)` for each composite that lists it, then `{composite} v{n}`
    /// for each non-deleted composite version that used one of its versions.
    pub async fn composite_refs(&self, cv_id: i64) -> Result<Vec<String>> {
        let lists: Vec<String> = sqlx::query_scalar(
            "SELECT c.name FROM cv_components k JOIN content_views c ON c.id = k.composite_id WHERE k.component_id = $1 ORDER BY c.name",
        )
        .bind(cv_id)
        .fetch_all(&self.pool)
        .await?;
        let used = sqlx::query(
            "SELECT DISTINCT c.name, v.number FROM cv_version_components u JOIN cv_versions v ON v.id = u.version_id
             JOIN content_views c ON c.id = v.cv_id JOIN cv_versions w ON w.id = u.component_version_id
             WHERE w.cv_id = $1 AND v.state != 'deleted' ORDER BY c.name, v.number",
        )
        .bind(cv_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out: Vec<String> = lists.into_iter().map(|c| format!("{c} (component)")).collect();
        for r in &used {
            out.push(format!("{} v{}", r.try_get::<String, _>(0)?, r.try_get::<i64, _>(1)?));
        }
        Ok(out)
    }

    /// Composites with auto publish that list this view as latest (no pin).
    pub async fn auto_composites(&self, component_id: i64) -> Result<Vec<ContentView>> {
        let rows = sqlx::query(
            "SELECT c.id, c.name, c.description, c.created_at, c.composite, c.auto_publish
             FROM cv_components k JOIN content_views c ON c.id = k.composite_id
             WHERE k.component_id = $1 AND k.version_id IS NULL AND c.auto_publish != 0 ORDER BY c.name",
        )
        .bind(component_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(content_view_row).collect()
    }

    pub async fn set_cv_repo(&self, cv_id: i64, repo: &CvRepo) -> Result<()> {
        sqlx::query(
            "INSERT INTO cv_repos(cv_id, repo_href, repo_name, repo_type, filters) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT(cv_id, repo_href) DO UPDATE SET filters = excluded.filters, repo_name = excluded.repo_name",
        )
        .bind(cv_id)
        .bind(&repo.repo_href)
        .bind(&repo.repo_name)
        .bind(type_str(repo.repo_type))
        .bind(serde_json::to_string(&repo.filters)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn remove_cv_repo(&self, cv_id: i64, repo_href: &str) -> Result<()> {
        sqlx::query("DELETE FROM cv_repos WHERE cv_id = $1 AND repo_href = $2").bind(cv_id).bind(repo_href).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn cv_repos(&self, cv_id: i64) -> Result<Vec<CvRepo>> {
        let rows = sqlx::query("SELECT repo_href, repo_name, repo_type, filters FROM cv_repos WHERE cv_id = $1 ORDER BY repo_name")
            .bind(cv_id)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(cv_repo_row).collect()
    }

    // ---- content view versions ---------------------------------------------------

    /// Start a new numbered version with a frozen copy of the definition.
    pub async fn begin_version(&self, cv_id: i64, notes: &str, repos: &[CvRepo]) -> Result<CvVersion> {
        self.begin(cv_id, notes, repos, &[], &[]).await
    }

    /// Start a composite version: the component versions it uses (`used`) and their rows (`rows`), stored
    /// as borrowed with every checkpoint column except the task reference.
    pub async fn begin_composite_version(&self, cv_id: i64, notes: &str, used: &[i64], rows: &[CvVersionRepo]) -> Result<CvVersion> {
        self.begin(cv_id, notes, &[], used, rows).await
    }

    async fn begin(&self, cv_id: i64, notes: &str, repos: &[CvRepo], used: &[i64], rows: &[CvVersionRepo]) -> Result<CvVersion> {
        let at = now();
        let mut tx = self.pool.begin().await?;
        // Number and insert in one statement: the write lock is taken before the number is read.
        let r = sqlx::query(
            "INSERT INTO cv_versions(cv_id, number, notes, state, created_at)
             SELECT $1, COALESCE(MAX(number), 0) + 1, $2, 'publishing', $3 FROM cv_versions WHERE cv_id = $1 RETURNING id, number",
        )
        .bind(cv_id)
        .bind(notes)
        .bind(&at)
        .fetch_one(&mut *tx)
        .await?;
        let (id, number): (i64, i64) = (r.try_get(0)?, r.try_get(1)?);
        // After the insert (SQLite holds the write lock; PostgreSQL holds the row with FOR SHARE): a component version that
        // `delete_version` marked since it was resolved is not borrowed, and a later mark waits for this commit.
        let share = if self.backend == Backend::Postgres { " FOR SHARE OF v" } else { "" };
        let sql =
            format!("SELECT v.state, v.number, c.name FROM cv_versions v JOIN content_views c ON c.id = v.cv_id WHERE v.id = $1{share}");
        for u in used {
            let r = sqlx::query(&sql).bind(u).fetch_one(&mut *tx).await?;
            let (state, n, name): (String, i64, String) = (r.try_get(0)?, r.try_get(1)?, r.try_get(2)?);
            if state != "published" {
                bail!("version v{n} of {name} is no longer published");
            }
        }
        for r in repos {
            sqlx::query("INSERT INTO cv_version_repos(version_id, repo_href, repo_name, repo_type, filters) VALUES ($1, $2, $3, $4, $5)")
                .bind(id)
                .bind(&r.repo_href)
                .bind(&r.repo_name)
                .bind(type_str(r.repo_type))
                .bind(serde_json::to_string(&r.filters)?)
                .execute(&mut *tx)
                .await?;
        }
        for u in used {
            sqlx::query("INSERT INTO cv_version_components(version_id, component_version_id) VALUES ($1, $2)")
                .bind(id)
                .bind(u)
                .execute(&mut *tx)
                .await?;
        }
        for r in rows {
            insert_version_repo(&mut tx, &CvVersionRepo { version_id: id, pending_task: None, borrowed: true, ..r.clone() }).await?;
        }
        tx.commit().await?;
        Ok(CvVersion { id, cv_id, number, notes: notes.into(), state: "publishing".into(), created_at: at, finished_at: None, error: None })
    }

    pub async fn finish_version(&self, id: i64, state: &str, error: Option<&str>) -> Result<()> {
        sqlx::query("UPDATE cv_versions SET state = $2, error = $3, finished_at = $4 WHERE id = $1")
            .bind(id)
            .bind(state)
            .bind(error)
            .bind(now())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// `finish_version(id, "failed", error)` only while the version is `publishing`: a workflow that lost
    /// its lock must not overwrite the result of the operator who took it over.
    pub async fn fail_publishing_version(&self, id: i64, error: &str) -> Result<()> {
        sqlx::query("UPDATE cv_versions SET state = 'failed', error = $2, finished_at = $3 WHERE id = $1 AND state = 'publishing'")
            .bind(id)
            .bind(error)
            .bind(now())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Change the state of a version only while it is `from`; `false`: it changed meanwhile.
    pub async fn set_version_state(&self, id: i64, from: &str, to: &str) -> Result<bool> {
        let n = sqlx::query("UPDATE cv_versions SET state = $3 WHERE id = $1 AND state = $2")
            .bind(id)
            .bind(from)
            .bind(to)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n == 1)
    }

    pub async fn versions(&self, cv_id: i64) -> Result<Vec<CvVersion>> {
        let rows = sqlx::query(
            "SELECT id, cv_id, number, notes, state, created_at, finished_at, error FROM cv_versions WHERE cv_id = $1 ORDER BY number DESC",
        )
        .bind(cv_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(version_row).collect()
    }

    pub async fn version(&self, id: i64) -> Result<CvVersion> {
        let r = sqlx::query("SELECT id, cv_id, number, notes, state, created_at, finished_at, error FROM cv_versions WHERE id = $1")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        version_row(&r)
    }

    pub async fn version_repos(&self, version_id: i64) -> Result<Vec<CvVersionRepo>> {
        let rows = sqlx::query(
            "SELECT repo_href, repo_name, repo_type, filters, source_version, derived_repo, derived_version, publication,
                    pending_task, content_count, borrowed, signing FROM cv_version_repos WHERE version_id = $1 ORDER BY repo_name",
        )
        .bind(version_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let borrowed: i64 = r.try_get(10)?;
                Ok(CvVersionRepo {
                    version_id,
                    repo: Some(cv_repo_row(r)?),
                    source_version: r.try_get(4)?,
                    derived_repo: r.try_get(5)?,
                    derived_version: r.try_get(6)?,
                    publication: r.try_get(7)?,
                    pending_task: r.try_get(8)?,
                    content_count: r.try_get(9)?,
                    borrowed: borrowed != 0,
                    signing: r.try_get(11)?,
                })
            })
            .collect()
    }

    /// Persist a checkpoint after each workflow step (enables resume after a crash).
    pub async fn save_version_repo(&self, vr: &CvVersionRepo) -> Result<()> {
        sqlx::query(
            "UPDATE cv_version_repos SET source_version = $3, derived_repo = $4, derived_version = $5, publication = $6,
                    pending_task = $7, content_count = $8, signing = $9 WHERE version_id = $1 AND repo_href = $2",
        )
        .bind(vr.version_id)
        .bind(&vr.repo().repo_href)
        .bind(&vr.source_version)
        .bind(&vr.derived_repo)
        .bind(&vr.derived_version)
        .bind(&vr.publication)
        .bind(&vr.pending_task)
        .bind(vr.content_count)
        .bind(&vr.signing)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Derived repository versions referenced by any non-deleted CV version (retention guard).
    pub async fn protected_versions(&self, profile: &str) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT vr.derived_version FROM cv_version_repos vr JOIN cv_versions v ON v.id = vr.version_id
             JOIN content_views c ON c.id = v.cv_id
             WHERE c.profile = $1 AND v.state != 'deleted' AND vr.derived_version IS NOT NULL",
        )
        .bind(profile)
        .fetch_all(&self.pool)
        .await?)
    }

    // ---- environments and bindings -----------------------------------------------

    pub async fn ensure_library(&self, profile: &str) -> Result<i64> {
        sqlx::query(
            "INSERT INTO environments(profile, name, prior_id, label) VALUES ($1, $2, NULL, $3) ON CONFLICT(profile, name) DO NOTHING",
        )
        .bind(profile)
        .bind(LIBRARY)
        .bind(default_label(LIBRARY))
        .execute(&self.pool)
        .await?;
        Ok(sqlx::query_scalar("SELECT id FROM environments WHERE profile = $1 AND name = $2")
            .bind(profile)
            .bind(LIBRARY)
            .fetch_one(&self.pool)
            .await?)
    }

    pub async fn create_env(&self, profile: &str, name: &str, prior_id: i64, protected: bool) -> Result<i64> {
        self.insert_env(profile, name, prior_id, protected, &default_label(name)).await
    }

    async fn insert_env(&self, profile: &str, name: &str, prior_id: i64, protected: bool, label: &str) -> Result<i64> {
        validate_name(name)?;
        validate_label(label)?;
        // shortcut: label uniqueness is checked, not indexed (old databases can hold case-only duplicates); add a unique index once none are left.
        if let Some(used) = self.label_user(profile, label, 0).await? {
            bail!("label {label} is already used by environment {used}");
        }
        let id = sqlx::query_scalar(
            "INSERT INTO environments(profile, name, prior_id, protected, label) VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(profile)
        .bind(name)
        .bind(prior_id)
        .bind(protected as i64)
        .bind(label)
        .fetch_one(&self.pool)
        .await
        .with_context(|| format!("environment {name} already exists?"))?;
        Ok(id)
    }

    /// Name of another environment (not `except`) of the scope that has this label.
    /// An older pulp-tui can insert a row without a label (''); it reads as the default for its name.
    async fn label_user(&self, profile: &str, label: &str, except: i64) -> Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT name FROM environments WHERE profile = $1 AND (label = $2 OR (label = '' AND lower(name) = $2)) AND id != $3
             ORDER BY id LIMIT 1",
        )
        .bind(profile)
        .bind(label)
        .bind(except)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Change the path segment of an environment. Only the row changes: moving served paths is the caller's job.
    pub async fn set_env_label(&self, id: i64, label: &str) -> Result<()> {
        validate_label(label)?;
        let profile: String = sqlx::query_scalar("SELECT profile FROM environments WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| anyhow!("unknown environment"))?;
        if let Some(used) = self.label_user(&profile, label, id).await? {
            bail!("label {label} is already used by environment {used}");
        }
        sqlx::query("UPDATE environments SET label = $2 WHERE id = $1").bind(id).bind(label).execute(&self.pool).await?;
        Ok(())
    }

    /// Ids of the views with a binding or served rows in the environment, ascending.
    pub async fn env_cv_ids(&self, env_id: i64) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT cv_id FROM bindings WHERE env_id = $1 UNION SELECT cv_id FROM binding_repos WHERE env_id = $1 ORDER BY cv_id",
        )
        .bind(env_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn set_env_protected(&self, id: i64, protected: bool) -> Result<()> {
        sqlx::query("UPDATE environments SET protected = $2 WHERE id = $1").bind(id).bind(protected as i64).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn delete_env(&self, id: i64) -> Result<()> {
        let used: i64 = sqlx::query_scalar(
            "SELECT (SELECT COUNT(*) FROM bindings WHERE env_id = $1) + (SELECT COUNT(*) FROM environments WHERE prior_id = $1)",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        if used > 0 {
            bail!("environment is in use (bindings or successor environments exist)");
        }
        sqlx::query("DELETE FROM binding_repos WHERE env_id = $1").bind(id).execute(&self.pool).await?;
        sqlx::query("DELETE FROM environments WHERE id = $1 AND prior_id IS NOT NULL").bind(id).execute(&self.pool).await?;
        Ok(())
    }

    /// Environments in path order (Library first, then each successor chain).
    pub async fn environments(&self, profile: &str) -> Result<Vec<Environment>> {
        let rows = sqlx::query("SELECT id, name, prior_id, protected, label FROM environments WHERE profile = $1 ORDER BY id")
            .bind(profile)
            .fetch_all(&self.pool)
            .await?;
        let all = rows
            .iter()
            .map(|r| {
                let (name, label): (String, String) = (r.try_get(1)?, r.try_get(4)?);
                let protected: i64 = r.try_get(3)?;
                let label = if label.is_empty() { default_label(&name) } else { label }; // a row from an older pulp-tui
                Ok(Environment { id: r.try_get(0)?, name, prior_id: r.try_get(2)?, protected: protected != 0, label })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut out = vec![];
        fn walk(all: &[Environment], parent: Option<i64>, out: &mut Vec<Environment>) {
            for e in all.iter().filter(|e| e.prior_id == parent) {
                out.push(e.clone());
                walk(all, Some(e.id), out);
            }
        }
        walk(&all, None, &mut out);
        Ok(out)
    }

    pub async fn binding(&self, env_id: i64, cv_id: i64) -> Result<Option<Binding>> {
        let r = sqlx::query("SELECT env_id, cv_id, version_id, state, updated_at FROM bindings WHERE env_id = $1 AND cv_id = $2")
            .bind(env_id)
            .bind(cv_id)
            .fetch_optional(&self.pool)
            .await?;
        r.as_ref().map(binding_row).transpose()
    }

    pub async fn bindings_for_cv(&self, cv_id: i64) -> Result<Vec<Binding>> {
        let rows = sqlx::query("SELECT env_id, cv_id, version_id, state, updated_at FROM bindings WHERE cv_id = $1")
            .bind(cv_id)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(binding_row).collect()
    }

    pub async fn set_binding(&self, env_id: i64, cv_id: i64, version_id: i64, state: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO bindings(env_id, cv_id, version_id, state, updated_at) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT(env_id, cv_id) DO UPDATE SET version_id = excluded.version_id, state = excluded.state, updated_at = excluded.updated_at",
        )
        .bind(env_id)
        .bind(cv_id)
        .bind(version_id)
        .bind(state)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// `set_binding` only while the binding is still this promotion's `promoting` row (see `fail_publishing_version`).
    pub async fn finish_promoting_binding(&self, env_id: i64, cv_id: i64, version_id: i64, state: &str) -> Result<()> {
        sqlx::query(
            "UPDATE bindings SET state = $4, updated_at = $5 WHERE env_id = $1 AND cv_id = $2 AND version_id = $3 AND state = 'promoting'",
        )
        .bind(env_id)
        .bind(cv_id)
        .bind(version_id)
        .bind(state)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn binding_repos(&self, env_id: i64, cv_id: i64) -> Result<Vec<BindingRepo>> {
        let rows = sqlx::query(
            "SELECT env_id, cv_id, repo_href, repo_name, repo_type, distribution, base_path, publication
             FROM binding_repos WHERE env_id = $1 AND cv_id = $2 ORDER BY repo_name",
        )
        .bind(env_id)
        .bind(cv_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(binding_repo_row).collect()
    }

    pub async fn all_binding_repos(&self, cv_id: i64) -> Result<Vec<BindingRepo>> {
        let rows = sqlx::query(
            "SELECT env_id, cv_id, repo_href, repo_name, repo_type, distribution, base_path, publication
             FROM binding_repos WHERE cv_id = $1",
        )
        .bind(cv_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(binding_repo_row).collect()
    }

    /// Every publication a binding row of the scope still records as served: `(publication, view, environment)`.
    pub async fn served_publications(&self, profile: &str) -> Result<Vec<(String, String, String)>> {
        let rows = sqlx::query(
            "SELECT b.publication, c.name, e.name FROM binding_repos b JOIN content_views c ON c.id = b.cv_id
             JOIN environments e ON e.id = b.env_id WHERE c.profile = $1 AND b.publication IS NOT NULL ORDER BY c.name, e.name",
        )
        .bind(profile)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|r| Ok((r.try_get(0)?, r.try_get(1)?, r.try_get(2)?))).collect()
    }

    pub async fn save_binding_repo(&self, b: &BindingRepo) -> Result<()> {
        sqlx::query(
            "INSERT INTO binding_repos(env_id, cv_id, repo_href, repo_name, repo_type, distribution, base_path, publication)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT(env_id, cv_id, repo_href) DO UPDATE SET distribution = excluded.distribution,
                base_path = excluded.base_path, publication = excluded.publication",
        )
        .bind(b.env_id)
        .bind(b.cv_id)
        .bind(&b.repo_href)
        .bind(&b.repo_name)
        .bind(type_str(b.repo_type))
        .bind(&b.distribution)
        .bind(&b.base_path)
        .bind(&b.publication)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Versions currently bound to any environment (cannot be deleted).
    pub async fn version_in_use(&self, version_id: i64) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar("SELECT e.name FROM bindings b JOIN environments e ON e.id = b.env_id WHERE b.version_id = $1")
            .bind(version_id)
            .fetch_all(&self.pool)
            .await?)
    }

    // ---- Content View locks (times are client unix seconds) ----------------------------

    /// Take the lock of a Content View, or take over one not renewed for `LOCK_TTL_SECS`, in one statement.
    /// `Ok(Err(holder))`: another operator holds it.
    pub async fn try_lock(&self, cv_id: i64, owner: &str, token: &str, operation: &str, now: i64) -> Result<Result<(), LockHolder>> {
        let n = sqlx::query(
            "INSERT INTO locks(cv_id, owner, token, operation, acquired_at, renewed_at) VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT(cv_id) DO UPDATE SET owner = excluded.owner, token = excluded.token, operation = excluded.operation,
                acquired_at = excluded.acquired_at, renewed_at = excluded.renewed_at
             WHERE locks.renewed_at < $7",
        )
        .bind(cv_id)
        .bind(owner)
        .bind(token)
        .bind(operation)
        .bind(rfc3339(now.max(0) as u64))
        .bind(now)
        .bind(now - LOCK_TTL_SECS)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if n == 1 {
            return Ok(Ok(()));
        }
        match self.lock_holder(cv_id).await? {
            Some(h) => Ok(Err(h)),
            None => bail!("the lock was released while taking it; try again"),
        }
    }

    /// `false`: the lock is no longer ours (taken over after expiry, or broken).
    pub async fn renew_lock(&self, cv_id: i64, token: &str, now: i64) -> Result<bool> {
        let n = sqlx::query("UPDATE locks SET renewed_at = $3 WHERE cv_id = $1 AND token = $2")
            .bind(cv_id)
            .bind(token)
            .bind(now)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n == 1)
    }

    pub async fn release_lock(&self, cv_id: i64, token: &str) -> Result<()> {
        sqlx::query("DELETE FROM locks WHERE cv_id = $1 AND token = $2").bind(cv_id).bind(token).execute(&self.pool).await?;
        Ok(())
    }

    /// Remove the lock of holder `h` (the one shown to the operator); `false`: the lock changed or is gone.
    pub async fn break_lock(&self, cv_id: i64, h: &LockHolder) -> Result<bool> {
        let n = sqlx::query("DELETE FROM locks WHERE cv_id = $1 AND owner = $2 AND acquired_at = $3")
            .bind(cv_id)
            .bind(&h.owner)
            .bind(&h.acquired_at)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n == 1)
    }

    /// `true`: the lock of the view is held and was renewed within `LOCK_TTL_SECS`.
    pub async fn lock_live(&self, cv_id: i64, now: i64) -> Result<bool> {
        let r = sqlx::query("SELECT 1 FROM locks WHERE cv_id = $1 AND renewed_at >= $2")
            .bind(cv_id)
            .bind(now - LOCK_TTL_SECS)
            .fetch_optional(&self.pool)
            .await?;
        Ok(r.is_some())
    }

    pub async fn lock_holder(&self, cv_id: i64) -> Result<Option<LockHolder>> {
        let r =
            sqlx::query("SELECT owner, operation, acquired_at FROM locks WHERE cv_id = $1").bind(cv_id).fetch_optional(&self.pool).await?;
        r.as_ref().map(lock_row).transpose()
    }

    // ---- audit and replication -----------------------------------------------------

    pub async fn audit(&self, profile: &str, actor: &str, action: &str, detail: &str) -> Result<()> {
        sqlx::query("INSERT INTO audit(profile, at, action, detail, actor) VALUES ($1, $2, $3, $4, $5)")
            .bind(profile)
            .bind(now())
            .bind(action)
            .bind(detail)
            .bind(actor)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn audit_log(&self, profile: &str, limit: usize) -> Result<Vec<AuditEntry>> {
        let rows = sqlx::query("SELECT at, action, detail, actor FROM audit WHERE profile = $1 ORDER BY id DESC LIMIT $2")
            .bind(profile)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| Ok(AuditEntry { at: r.try_get(0)?, action: r.try_get(1)?, detail: r.try_get(2)?, actor: r.try_get(3)? }))
            .collect()
    }

    pub async fn record_replication(&self, profile: &str, source: &str, distribution: &str, state: &str, detail: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO replication_runs(profile, source_profile, distribution, state, at, detail) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(profile)
        .bind(source)
        .bind(distribution)
        .bind(state)
        .bind(now())
        .bind(detail)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Last successful replication time per distribution base path.
    pub async fn last_replications(&self, profile: &str, source: &str) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query(
            "SELECT distribution, MAX(at) FROM replication_runs WHERE profile = $1 AND source_profile = $2 AND state = 'completed'
             GROUP BY distribution",
        )
        .bind(profile)
        .bind(source)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|r| Ok((r.try_get(0)?, r.try_get(1)?))).collect()
    }

    // ---- export / import (definitions only, no secrets, no Pulp state) ---------------

    pub async fn export(&self, profile: &str) -> Result<Export> {
        let mut views = vec![];
        for cv in self.content_views(profile).await? {
            let mut components = vec![];
            for c in self.components(cv.id).await? {
                let pinned = match c.pinned {
                    Some(id) => Some(self.version(id).await?.number),
                    None => None,
                };
                components.push(ExportComponent { name: c.name, pinned });
            }
            views.push(ExportView {
                name: cv.name,
                description: cv.description,
                composite: cv.composite,
                auto_publish: cv.auto_publish,
                components,
                repos: self.cv_repos(cv.id).await?,
            });
        }
        let envs = self.environments(profile).await?;
        let name_of = |id: Option<i64>| id.and_then(|i| envs.iter().find(|e| e.id == i)).map(|e| e.name.clone());
        let environments = envs
            .iter()
            .filter(|e| e.prior_id.is_some())
            .map(|e| ExportEnv {
                name: e.name.clone(),
                prior: name_of(e.prior_id).unwrap_or_default(),
                protected: e.protected,
                label: e.label.clone(),
            })
            .collect();
        Ok(Export { format: 2, content_views: views, environments })
    }

    /// Import definitions (format 1 or 2); existing objects with the same name are left unchanged.
    /// Returns the views and environments created and warnings. A component pin (a version number) becomes
    /// latest: the versions of another database do not exist here. Components are added to the composites
    /// this call created, after all views exist, so the order of the file does not matter.
    pub async fn import(&self, profile: &str, ex: &Export) -> Result<(usize, usize, Vec<String>)> {
        if !(1..=2).contains(&ex.format) {
            bail!("unsupported export format {}", ex.format);
        }
        // Checked before any write: an entry refused halfway would leave the composites created before it empty.
        for v in &ex.content_views {
            validate_name(&v.name).with_context(|| format!("content view {:?}", v.name))?;
            for r in &v.repos {
                for f in &r.filters {
                    crate::filters::parse_rule(&f.to_string(), r.repo_type).map_err(|e| anyhow!("{} {}: {e}", v.name, r.repo_name))?;
                }
            }
        }
        for e in &ex.environments {
            validate_name(&e.name).with_context(|| format!("environment {:?}", e.name))?;
            if !e.label.is_empty() {
                validate_label(&e.label).with_context(|| format!("environment {}", e.name))?;
            }
        }
        self.ensure_library(profile).await?;
        let mut envs = 0;
        // Parents come before children in exports; loop until no progress for safety.
        let mut pending: Vec<&ExportEnv> = ex.environments.iter().collect();
        while !pending.is_empty() {
            let before = pending.len();
            let existing = self.environments(profile).await?;
            let mut waiting = vec![];
            for e in pending {
                if existing.iter().any(|x| x.name == e.name) {
                    continue;
                }
                match existing.iter().find(|x| x.name == e.prior) {
                    Some(p) => {
                        let label = if e.label.is_empty() { default_label(&e.name) } else { e.label.clone() };
                        self.insert_env(profile, &e.name, p.id, e.protected, &label).await?;
                        envs += 1;
                    }
                    None => waiting.push(e),
                }
            }
            pending = waiting;
            if pending.len() == before {
                bail!("environment(s) with unknown prior: {}", pending.iter().map(|e| e.name.as_str()).collect::<Vec<_>>().join(", "));
            }
        }
        let mut cvs = 0;
        let existing = self.content_views(profile).await?;
        let mut composites = vec![];
        for v in &ex.content_views {
            if existing.iter().any(|c| c.name == v.name) {
                continue;
            }
            if v.composite {
                let id = self.create_composite(profile, &v.name, &v.description).await?;
                self.set_auto_publish(id, v.auto_publish).await?;
                composites.push((id, v));
            } else {
                let id = self.create_cv(profile, &v.name, &v.description).await?;
                for r in &v.repos {
                    self.set_cv_repo(id, r).await?;
                }
            }
            cvs += 1;
        }
        let mut warnings = vec![];
        let views = self.content_views(profile).await?;
        for (id, v) in composites {
            for c in &v.components {
                match views.iter().find(|x| x.name == c.name) {
                    None => warnings.push(format!("{}: component {} not found; skipped", v.name, c.name)),
                    Some(x) if x.composite => warnings.push(format!("{}: component {} is a composite; skipped", v.name, c.name)),
                    Some(x) => {
                        self.set_component(id, x.id, None).await?;
                        if let Some(k) = c.pinned {
                            warnings.push(format!("{}: component {} was pinned to v{k}; imported as latest", v.name, c.name));
                        }
                    }
                }
            }
        }
        Ok((cvs, envs, warnings))
    }

    // ---- db copy ---------------------------------------------------------------------

    /// `pulp-tui db copy`, on the target: copy the Content View data of `src_scope` in `src` (any store)
    /// into `dst_scope` here, in one transaction with new ids. It never merges: a name that exists is refused.
    /// It also refuses the same database, and a view whose lock was renewed within `LOCK_TTL_SECS` (an operation
    /// is running). Task references, replication runs and locks stay behind. `src` is only read.
    pub async fn copy_from(&self, src: &Store, src_scope: &str, dst_scope: &str) -> Result<CopyReport> {
        if src.db_id() == self.db_id() {
            bail!("source and target are the same database");
        }
        let live = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64) - LOCK_TTL_SECS;
        let locked = sqlx::query(
            "SELECT c.name, l.owner, l.operation FROM locks l JOIN content_views c ON c.id = l.cv_id
             WHERE c.profile = $1 AND l.renewed_at >= $2 ORDER BY c.name LIMIT 1",
        )
        .bind(src_scope)
        .bind(live)
        .fetch_optional(&src.pool)
        .await?;
        if let Some(r) = locked {
            let (name, owner, op): (String, String, String) = (r.try_get(0)?, r.try_get(1)?, r.try_get(2)?);
            bail!("Content View {name} is locked by {owner} ({op}); copy after it ends");
        }
        let (views, envs) = (src.content_views(src_scope).await?, src.environments(src_scope).await?);
        let have = self.content_views(dst_scope).await?;
        if let Some(v) = views.iter().find(|v| have.iter().any(|h| h.name == v.name)) {
            bail!("target already has content view {} for this server; db copy never merges", v.name);
        }
        let have = self.environments(dst_scope).await?;
        if let Some(e) = envs.iter().find(|e| e.prior_id.is_some() && have.iter().any(|h| h.name == e.name)) {
            bail!("target already has environment {} for this server; db copy never merges", e.name);
        }
        if let Some(e) = envs.iter().find(|e| e.prior_id.is_some() && have.iter().any(|h| h.label == e.label)) {
            bail!("target already has an environment with label {} for this server; db copy never merges", e.label);
        }
        let src_library = envs.iter().find(|e| e.prior_id.is_none()).map(|e| e.label.clone());
        if let (Some(s), Some(t)) = (&src_library, have.iter().find(|h| h.prior_id.is_none()))
            && t.label != *s
        {
            bail!("target Library has label {}, the source {s}; db copy never merges", t.label);
        }
        let src_library = src_library.unwrap_or_else(|| default_label(LIBRARY));
        let mut log = src.audit_log(src_scope, i64::MAX as usize).await?;
        log.reverse(); // oldest first, so the new ids keep the order
        let mut report = CopyReport {
            views: views.len(),
            environments: envs.iter().filter(|e| e.prior_id.is_some()).count(),
            audit: log.len(),
            ..Default::default()
        };

        let mut tx = self.pool.begin().await?;
        // Source ids to target ids; environments come in path order, so a prior is always mapped first.
        let mut env_ids = HashMap::new();
        sqlx::query(
            "INSERT INTO environments(profile, name, prior_id, label) VALUES ($1, $2, NULL, $3) ON CONFLICT(profile, name) DO NOTHING",
        )
        .bind(dst_scope)
        .bind(LIBRARY)
        .bind(&src_library)
        .execute(&mut *tx)
        .await?;
        let library: i64 = sqlx::query_scalar("SELECT id FROM environments WHERE profile = $1 AND name = $2")
            .bind(dst_scope)
            .bind(LIBRARY)
            .fetch_one(&mut *tx)
            .await?;
        for e in &envs {
            let id = match e.prior_id {
                None => library,
                Some(prior) => {
                    sqlx::query_scalar(
                        "INSERT INTO environments(profile, name, prior_id, protected, label) VALUES ($1, $2, $3, $4, $5) RETURNING id",
                    )
                    .bind(dst_scope)
                    .bind(&e.name)
                    .bind(env_ids[&prior])
                    .bind(e.protected as i64)
                    .bind(&e.label)
                    .fetch_one(&mut *tx)
                    .await?
                }
            };
            env_ids.insert(e.id, id);
        }
        // A composite can sort before its components: its components and used versions go in after all views.
        let (mut cv_ids, mut version_ids) = (HashMap::new(), HashMap::new());
        let (mut components, mut used) = (vec![], vec![]);
        for v in &views {
            let cv_id: i64 = sqlx::query_scalar(
                "INSERT INTO content_views(profile, name, description, created_at, composite, auto_publish)
                 VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
            )
            .bind(dst_scope)
            .bind(&v.name)
            .bind(&v.description)
            .bind(&v.created_at)
            .bind(v.composite as i64)
            .bind(v.auto_publish as i64)
            .fetch_one(&mut *tx)
            .await?;
            cv_ids.insert(v.id, cv_id);
            components.extend(src.components(v.id).await?.into_iter().map(|k| (cv_id, k)));
            for r in src.cv_repos(v.id).await? {
                sqlx::query("INSERT INTO cv_repos(cv_id, repo_href, repo_name, repo_type, filters) VALUES ($1, $2, $3, $4, $5)")
                    .bind(cv_id)
                    .bind(&r.repo_href)
                    .bind(&r.repo_name)
                    .bind(type_str(r.repo_type))
                    .bind(serde_json::to_string(&r.filters)?)
                    .execute(&mut *tx)
                    .await?;
            }
            let mut versions = src.versions(v.id).await?;
            versions.reverse();
            report.versions += versions.len();
            for ver in &versions {
                let id: i64 = sqlx::query_scalar(
                    "INSERT INTO cv_versions(cv_id, number, notes, state, created_at, finished_at, error)
                     VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
                )
                .bind(cv_id)
                .bind(ver.number)
                .bind(&ver.notes)
                .bind(&ver.state)
                .bind(&ver.created_at)
                .bind(&ver.finished_at)
                .bind(&ver.error)
                .fetch_one(&mut *tx)
                .await?;
                version_ids.insert(ver.id, id);
                for vr in src.version_repos(ver.id).await? {
                    insert_version_repo(&mut tx, &CvVersionRepo { version_id: id, ..vr }).await?;
                }
                used.extend(src.version_components(ver.id).await?.into_iter().map(|u| (id, u.version_id)));
            }
            let bindings = src.bindings_for_cv(v.id).await?;
            report.bindings += bindings.len();
            for b in &bindings {
                let env_id = env_ids.get(&b.env_id).copied().context("a binding points at an environment outside the path")?;
                let version_id = versions
                    .iter()
                    .find(|x| x.id == b.version_id)
                    .map(|x| version_ids[&x.id])
                    .context("a binding points at a version of another view")?;
                sqlx::query("INSERT INTO bindings(env_id, cv_id, version_id, state, updated_at) VALUES ($1, $2, $3, $4, $5)")
                    .bind(env_id)
                    .bind(cv_id)
                    .bind(version_id)
                    .bind(&b.state)
                    .bind(&b.updated_at)
                    .execute(&mut *tx)
                    .await?;
                for r in src.binding_repos(b.env_id, v.id).await? {
                    sqlx::query(
                        "INSERT INTO binding_repos(env_id, cv_id, repo_href, repo_name, repo_type, distribution, base_path, publication)
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                    )
                    .bind(env_id)
                    .bind(cv_id)
                    .bind(&r.repo_href)
                    .bind(&r.repo_name)
                    .bind(type_str(r.repo_type))
                    .bind(&r.distribution)
                    .bind(&r.base_path)
                    .bind(&r.publication)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
        let version = |id: i64| version_ids.get(&id).copied().context("a composite points at a version outside the copy");
        for (composite, k) in &components {
            sqlx::query("INSERT INTO cv_components(composite_id, component_id, version_id) VALUES ($1, $2, $3)")
                .bind(composite)
                .bind(cv_ids.get(&k.cv_id).copied().context("a composite points at a view outside the copy")?)
                .bind(k.pinned.map(version).transpose()?)
                .execute(&mut *tx)
                .await?;
        }
        for (id, u) in &used {
            sqlx::query("INSERT INTO cv_version_components(version_id, component_version_id) VALUES ($1, $2)")
                .bind(id)
                .bind(version(*u)?)
                .execute(&mut *tx)
                .await?;
        }
        for a in &log {
            sqlx::query("INSERT INTO audit(profile, at, action, detail, actor) VALUES ($1, $2, $3, $4, $5)")
                .bind(dst_scope)
                .bind(&a.at)
                .bind(&a.action)
                .bind(&a.detail)
                .bind(if a.actor.is_empty() { "copied" } else { &a.actor })
                .execute(&mut *tx)
                .await?;
        }
        // Pulp objects the local file labeled with its id are now this database's.
        sqlx::query("INSERT INTO meta(key, value) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(format!("alias:{}", src.db_id()))
            .bind(now())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(report)
    }
}

/// What `Store::copy_from` copied; `environments` does not count Library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CopyReport {
    pub views: usize,
    pub versions: usize,
    pub environments: usize,
    pub bindings: usize,
    pub audit: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Export {
    pub format: u32,
    pub content_views: Vec<ExportView>,
    pub environments: Vec<ExportEnv>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportView {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Format 2: has components instead of repositories.
    #[serde(default)]
    pub composite: bool,
    #[serde(default)]
    pub auto_publish: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<ExportComponent>,
    pub repos: Vec<CvRepo>,
}

/// A component by view name; `pinned` is a version number, which an import turns into latest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportComponent {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportEnv {
    pub name: String,
    pub prior: String,
    #[serde(default)]
    pub protected: bool,
    /// Empty (a file from before labels): the default for the name.
    #[serde(default)]
    pub label: String,
}

fn lock_row(r: &AnyRow) -> Result<LockHolder> {
    Ok(LockHolder { owner: r.try_get(0)?, operation: r.try_get(1)?, acquired_at: r.try_get(2)? })
}

/// Columns `id, name, description, created_at, composite, auto_publish`.
fn content_view_row(r: &AnyRow) -> Result<ContentView> {
    let (composite, auto_publish): (i64, i64) = (r.try_get(4)?, r.try_get(5)?);
    Ok(ContentView {
        id: r.try_get(0)?,
        name: r.try_get(1)?,
        description: r.try_get(2)?,
        created_at: r.try_get(3)?,
        composite: composite != 0,
        auto_publish: auto_publish != 0,
    })
}

/// A full `cv_version_repos` row, every checkpoint column as `vr` holds it.
async fn insert_version_repo(c: &mut AnyConnection, vr: &CvVersionRepo) -> Result<()> {
    let r = vr.repo();
    sqlx::query(
        "INSERT INTO cv_version_repos(version_id, repo_href, repo_name, repo_type, filters, source_version, derived_repo,
                derived_version, publication, pending_task, content_count, borrowed, signing)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
    )
    .bind(vr.version_id)
    .bind(&r.repo_href)
    .bind(&r.repo_name)
    .bind(type_str(r.repo_type))
    .bind(serde_json::to_string(&r.filters)?)
    .bind(&vr.source_version)
    .bind(&vr.derived_repo)
    .bind(&vr.derived_version)
    .bind(&vr.publication)
    .bind(&vr.pending_task)
    .bind(vr.content_count)
    .bind(vr.borrowed as i64)
    .bind(&vr.signing)
    .execute(&mut *c)
    .await?;
    Ok(())
}

fn cv_repo_row(r: &AnyRow) -> Result<CvRepo> {
    Ok(CvRepo {
        repo_href: r.try_get(0)?,
        repo_name: r.try_get(1)?,
        repo_type: repo_type(r.try_get(2)?)?,
        filters: serde_json::from_str(r.try_get(3)?).unwrap_or_default(),
    })
}

fn version_row(r: &AnyRow) -> Result<CvVersion> {
    Ok(CvVersion {
        id: r.try_get(0)?,
        cv_id: r.try_get(1)?,
        number: r.try_get(2)?,
        notes: r.try_get(3)?,
        state: r.try_get(4)?,
        created_at: r.try_get(5)?,
        finished_at: r.try_get(6)?,
        error: r.try_get(7)?,
    })
}

fn binding_row(r: &AnyRow) -> Result<Binding> {
    Ok(Binding { env_id: r.try_get(0)?, cv_id: r.try_get(1)?, version_id: r.try_get(2)?, state: r.try_get(3)?, updated_at: r.try_get(4)? })
}

fn binding_repo_row(r: &AnyRow) -> Result<BindingRepo> {
    Ok(BindingRepo {
        env_id: r.try_get(0)?,
        cv_id: r.try_get(1)?,
        repo_href: r.try_get(2)?,
        repo_name: r.try_get(3)?,
        repo_type: repo_type(r.try_get(4)?)?,
        distribution: r.try_get(5)?,
        base_path: r.try_get(6)?,
        publication: r.try_get(7)?,
    })
}

/// Names become part of Pulp object names and base paths.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
        bail!("name must be 1-64 characters of [A-Za-z0-9-_.]");
    }
    Ok(())
}

/// The label a new environment gets: its name as a lower-case path segment.
pub fn default_label(name: &str) -> String {
    crate::cv::slug(name).to_lowercase()
}

/// A label is one segment of a served path.
pub fn validate_label(label: &str) -> Result<()> {
    let ok = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c);
    if label.is_empty() || label.len() > 64 || label == "." || label == ".." || !label.chars().all(ok) {
        bail!("label must be 1-64 characters of [a-z0-9-_.]");
    }
    Ok(())
}

/// Host name: the kernel's on Linux, else the shell's variable.
pub fn hostname() -> String {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        let from_env = || ["HOSTNAME", "COMPUTERNAME"].iter().find_map(|k| std::env::var(k).ok());
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .ok()
            .or_else(from_env)
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "unknown".into())
    })
    .clone()
}

/// Operator name for locks and the audit log: `user@host` (`anonymous` when Pulp did not tell the user).
pub fn actor(user: Option<&str>) -> String {
    format!("{}@{}", user.filter(|u| !u.is_empty()).unwrap_or("anonymous"), hostname())
}

/// 16 hex characters from the time, the process id and the host name.
fn new_db_id() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    hex::encode(Sha256::digest(format!("{nanos}/{}/{}", std::process::id(), hostname())))[..16].to_string()
}

/// UTC timestamp in RFC 3339 (civil-from-days, no time crate needed).
pub fn now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    rfc3339(secs)
}

pub fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filters::parse_rules;

    fn repo(name: &str, t: RepoType) -> CvRepo {
        CvRepo {
            repo_href: format!("/pulp/api/v3/repositories/{}/{0}/{name}/", t.component()),
            repo_name: name.into(),
            repo_type: t,
            filters: vec![],
        }
    }

    #[test]
    fn timestamps() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_798_761_599), "2026-12-31T23:59:59Z");
    }

    #[tokio::test]
    async fn migrations_are_idempotent_and_versioned() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("db.sqlite");
        Store::open(&p).await.unwrap().create_cv("lab", "base", "").await.unwrap();
        let s = Store::open(&p).await.unwrap();
        assert_eq!(s.content_views("lab").await.unwrap().len(), 1);
        sqlx::raw_sql("PRAGMA user_version = 99").execute(&s.pool).await.unwrap();
        drop(s);
        assert!(Store::open(&p).await.is_err());
    }

    #[tokio::test]
    async fn opens_v1_database_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pulp-tui.db");
        {
            // A file at schema 1 (made by this code at version 1): one view, one audit row without an actor.
            let s = Store::open_at_version(&path, 1).await.unwrap();
            assert_eq!(s.schema_version().await.unwrap(), 1);
            s.create_cv("p", "web", "d").await.unwrap();
            sqlx::raw_sql("INSERT INTO audit(profile, at, action, detail) VALUES ('p', 't', 'old', 'row')").execute(&s.pool).await.unwrap();
        }
        let s = Store::open(&path).await.unwrap();
        assert_eq!(s.content_views("p").await.unwrap()[0].name, "web");
        assert_eq!(s.audit_log("p", 1).await.unwrap()[0].actor, "");
        assert_eq!(s.db_id().len(), 16);
        assert_eq!(s.schema_version().await.unwrap(), MIGRATIONS.len() as i64);
    }

    #[tokio::test]
    async fn content_view_versions_and_bindings() {
        let s = Store::open_in_memory().await.unwrap();
        let cv = s.create_cv("lab", "base", "desc").await.unwrap();
        assert!(s.create_cv("lab", "base", "").await.is_err());
        assert!(s.create_cv("lab", "bad name", "").await.is_err());
        s.create_cv("other", "base", "").await.unwrap(); // profiles are isolated
        let mut r = repo("os", RepoType::Rpm);
        r.filters = parse_rules("exclude name=*-debuginfo", RepoType::Rpm).unwrap();
        s.set_cv_repo(cv, &r).await.unwrap();
        s.set_cv_repo(cv, &repo("docs", RepoType::File)).await.unwrap();
        assert_eq!(s.cv_repos(cv).await.unwrap().len(), 2);

        let v1 = s.begin_version(cv, "first", &s.cv_repos(cv).await.unwrap()).await.unwrap();
        let v2 = s.begin_version(cv, "second", &s.cv_repos(cv).await.unwrap()).await.unwrap();
        assert_eq!((v1.number, v2.number), (1, 2));
        let mut vrs = s.version_repos(v1.id).await.unwrap();
        assert_eq!(vrs[1].repo().filters.len(), 1);
        vrs[0].derived_version = Some("/dv/1/".into());
        s.save_version_repo(&vrs[0]).await.unwrap();
        s.finish_version(v1.id, "published", None).await.unwrap();
        assert_eq!(s.version(v1.id).await.unwrap().state, "published");
        assert_eq!(s.protected_versions("lab").await.unwrap(), vec!["/dv/1/"]);

        let lib = s.ensure_library("lab").await.unwrap();
        assert_eq!(s.ensure_library("lab").await.unwrap(), lib);
        let dev = s.create_env("lab", "Dev", lib, false).await.unwrap();
        let prod = s.create_env("lab", "Prod", dev, true).await.unwrap();
        let qa = s.create_env("lab", "QA", lib, false).await.unwrap();
        let order: Vec<_> = s.environments("lab").await.unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(order, ["Library", "Dev", "Prod", "QA"]);
        s.set_binding(dev, cv, v1.id, "active").await.unwrap();
        assert_eq!(s.version_in_use(v1.id).await.unwrap(), vec!["Dev"]);
        s.set_binding(dev, cv, v2.id, "active").await.unwrap();
        assert!(s.version_in_use(v1.id).await.unwrap().is_empty());
        assert!(s.delete_env(dev).await.is_err()); // bound + has successor
        s.delete_env(qa).await.unwrap();
        assert!(s.environments("lab").await.unwrap().iter().all(|e| e.id != qa));
        s.set_env_label(prod, "production").await.unwrap();

        let ex = s.export("lab").await.unwrap();
        let s2 = Store::open_in_memory().await.unwrap();
        assert_eq!(s2.import("lab", &ex).await.unwrap(), (1, 2, vec![]));
        assert_eq!(s2.import("lab", &ex).await.unwrap(), (0, 0, vec![]));
        assert_eq!(s2.export("lab").await.unwrap(), ex);

        s.delete_cv(cv).await.unwrap();
        assert!(s.versions(cv).await.unwrap().is_empty());
    }

    #[test]
    fn migration_lists_match() {
        assert_eq!(MIGRATIONS.len(), PG_MIGRATIONS.len());
    }

    #[tokio::test]
    async fn deb_signing_choice_is_a_checkpoint() {
        let s = Store::open_in_memory().await.unwrap();
        let cv = s.create_cv("k", "web", "").await.unwrap();
        let v = s.begin_version(cv, "", &[repo("apt", RepoType::Deb)]).await.unwrap();
        let mut vr = s.version_repos(v.id).await.unwrap().remove(0);
        assert_eq!(vr.signing, "", "same as source until the publish sets it");
        vr.source_version = Some("/r/1/versions/2/".into());
        vr.signing = "/pulp/api/v3/signing-services/1/".into();
        s.save_version_repo(&vr).await.unwrap();
        assert_eq!(s.version_repos(v.id).await.unwrap(), [vr]);
    }

    #[tokio::test]
    async fn deb_repository_with_a_version_rule_loads() {
        let s = Store::open_in_memory().await.unwrap();
        let cv = s.create_cv("k", "web", "").await.unwrap();
        let rules = r#"[{"action":"exclude","name":"*-dbg","version":["<","1.0"]}]"#;
        sqlx::query("INSERT INTO cv_repos(cv_id, repo_href, repo_name, repo_type, filters) VALUES ($1, '/r/deb/', 'apt', 'deb', $2)")
            .bind(cv)
            .bind(rules)
            .execute(&s.pool)
            .await
            .unwrap();
        let r = s.cv_repos(cv).await.unwrap().remove(0);
        assert_eq!(r.repo_type, RepoType::Deb);
        assert_eq!(r.filters, parse_rules("exclude name=*-dbg version<1.0", RepoType::Deb).unwrap());
        assert_eq!(r.filters[0].version, Some(("<".into(), "1.0".into())));
    }

    #[tokio::test]
    async fn migration_3_keeps_todays_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pulp-tui.db");
        {
            // Rows as schema 2 wrote them, in raw SQL (the new store code writes columns schema 2 lacks).
            // "dev" next to "Dev" shares today's segment.
            let s = Store::open_at_version(&path, 2).await.unwrap();
            sqlx::raw_sql(
                "INSERT INTO content_views(id, profile, name, created_at) VALUES (1, 'old', 'web', 't');
                 INSERT INTO environments(id, profile, name, prior_id) VALUES (1, 'old', 'Library', NULL), (2, 'old', 'Dev', 1), (3, 'old', 'dev', 1);
                 INSERT INTO binding_repos VALUES (2, 1, '/r/1/', 'os', 'rpm', '/d/1/', 'acme/dev/web/os', '/p/1/');",
            )
            .execute(&s.pool)
            .await
            .unwrap();
        }
        let s = Store::open(&path).await.unwrap();
        let labels: Vec<String> = s.environments("old").await.unwrap().iter().map(|e| format!("{}={}", e.name, e.label)).collect();
        assert_eq!(labels, ["Library=library", "Dev=dev", "dev=dev"]);
    }

    #[tokio::test]
    async fn migration_4_keeps_rows_of_normal_views() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pulp-tui.db");
        {
            // Rows as schema 3 wrote them, in raw SQL (the new store code writes columns schema 3 lacks).
            let s = Store::open_at_version(&path, 3).await.unwrap();
            sqlx::raw_sql(
                "INSERT INTO content_views(id, profile, name, created_at) VALUES (1, 'old', 'web', 't');
                 INSERT INTO cv_versions(id, cv_id, number, state, created_at) VALUES (1, 1, 1, 'published', 't');
                 INSERT INTO cv_version_repos VALUES (1, '/r/1/', 'os', 'rpm', '[]', '/r/1/versions/3/', '/d/1/', '/d/1/versions/1/', '/p/1/', NULL, 7);",
            )
            .execute(&s.pool)
            .await
            .unwrap();
        }
        let s = Store::open(&path).await.unwrap();
        let cv = s.content_view(1).await.unwrap();
        assert_eq!((cv.name.as_str(), cv.composite, cv.auto_publish), ("web", false, false));
        let rows = s.version_repos(1).await.unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!((r.repo().repo_href.as_str(), r.repo().repo_name.as_str(), r.repo().repo_type), ("/r/1/", "os", RepoType::Rpm));
        assert_eq!(r.source_version.as_deref(), Some("/r/1/versions/3/"));
        assert_eq!((r.derived_repo.as_deref(), r.derived_version.as_deref()), (Some("/d/1/"), Some("/d/1/versions/1/")));
        assert_eq!((r.publication.as_deref(), r.pending_task.as_deref(), r.content_count), (Some("/p/1/"), None, Some(7)));
        assert!(!r.borrowed);
        assert_eq!(r.signing, "", "schema 5: a row written before it is same as source");
    }

    #[tokio::test]
    async fn labels_default_validate_and_stay_unique() {
        let s = Store::open_in_memory().await.unwrap();
        let lib = s.ensure_library("k").await.unwrap();
        let dev = s.create_env("k", "Dev", lib, false).await.unwrap();
        let envs = s.environments("k").await.unwrap();
        assert_eq!((envs[0].label.as_str(), envs[1].label.as_str()), ("library", "dev"));
        for bad in ["", "Dev", "a/b", ".", "..", &"x".repeat(65)] {
            assert!(validate_label(bad).is_err(), "{bad}");
        }
        validate_label("dev-2.x_y").unwrap();
        assert_eq!(s.set_env_label(dev, "Bad").await.unwrap_err().to_string(), "label must be 1-64 characters of [a-z0-9-_.]");
        assert_eq!(s.set_env_label(dev, "library").await.unwrap_err().to_string(), "label library is already used by environment Library");
        assert_eq!(s.create_env("k", "dev", lib, false).await.unwrap_err().to_string(), "label dev is already used by environment Dev");
        s.set_env_label(dev, "dev").await.unwrap(); // its own label is not a clash
        s.set_env_label(dev, "development").await.unwrap();
        s.create_env("k", "dev", lib, false).await.unwrap(); // the default is free now
        s.create_env("other", "Dev", s.ensure_library("other").await.unwrap(), false).await.unwrap(); // per scope
    }

    #[tokio::test]
    async fn an_environment_row_without_a_label_reads_as_its_default() {
        let s = Store::open_in_memory().await.unwrap();
        let lib = s.ensure_library("k").await.unwrap();
        // A row as an older pulp-tui writes it after schema 3: the column default, ''.
        sqlx::query("INSERT INTO environments(profile, name, prior_id) VALUES ('k', 'QA', $1)").bind(lib).execute(&s.pool).await.unwrap();
        let envs = s.environments("k").await.unwrap();
        assert_eq!(envs[1].label, "qa");
        assert_eq!(crate::cv::base_path("web", &envs[1].label, "os"), "qa/web/os");
        assert_eq!(s.create_env("k", "qa", lib, false).await.unwrap_err().to_string(), "label qa is already used by environment QA");
        let dev = s.create_env("k", "Dev", lib, false).await.unwrap();
        assert_eq!(s.set_env_label(dev, "qa").await.unwrap_err().to_string(), "label qa is already used by environment QA");
        s.set_env_label(envs[1].id, "qa").await.unwrap(); // its own default is not a clash
        assert_eq!(s.environments("k").await.unwrap()[1].label, "qa");
    }

    #[tokio::test]
    async fn env_cv_ids_lists_views_with_a_binding_or_served_rows() {
        let s = Store::open_in_memory().await.unwrap();
        let lib = s.ensure_library("k").await.unwrap();
        let dev = s.create_env("k", "Dev", lib, false).await.unwrap();
        let (a, _unbound, c) =
            (s.create_cv("k", "a", "").await.unwrap(), s.create_cv("k", "b", "").await.unwrap(), s.create_cv("k", "c", "").await.unwrap());
        assert!(s.env_cv_ids(dev).await.unwrap().is_empty());
        let v = s.begin_version(c, "", &[]).await.unwrap();
        s.set_binding(dev, c, v.id, "active").await.unwrap(); // a binding, no served rows
        let row = BindingRepo {
            env_id: dev,
            cv_id: a,
            repo_href: "/r/1/".into(),
            repo_name: "os".into(),
            repo_type: RepoType::Rpm,
            distribution: "/d/1/".into(),
            base_path: "dev/a/os".into(),
            publication: None,
        };
        s.save_binding_repo(&row).await.unwrap(); // served rows, no binding
        s.save_binding_repo(&BindingRepo { repo_href: "/r/2/".into(), ..row }).await.unwrap();
        assert_eq!(s.env_cv_ids(dev).await.unwrap(), [a, c]);
    }

    #[tokio::test]
    async fn copy_keeps_labels_and_refuses_label_clash() {
        let src = Store::open_in_memory().await.unwrap();
        let lib = src.ensure_library("lab").await.unwrap();
        let dev = src.create_env("lab", "Dev", lib, false).await.unwrap();
        src.set_env_label(lib, "base").await.unwrap();
        src.set_env_label(dev, "development").await.unwrap();
        let labels = |envs: Vec<Environment>| envs.iter().map(|e| format!("{}={}", e.name, e.label)).collect::<Vec<_>>();

        // An empty scope: both labels are kept, Library's too.
        let dst = Store::open_in_memory().await.unwrap();
        dst.copy_from(&src, "lab", "k").await.unwrap();
        assert_eq!(labels(dst.environments("k").await.unwrap()), ["Library=base", "Dev=development"]);

        // Another environment of the target scope uses the label.
        let dst2 = Store::open_in_memory().await.unwrap();
        let lib2 = dst2.ensure_library("k").await.unwrap();
        let staging = dst2.create_env("k", "Staging", lib2, false).await.unwrap();
        dst2.set_env_label(staging, "development").await.unwrap();
        let e = dst2.copy_from(&src, "lab", "k").await.unwrap_err().to_string();
        assert_eq!(e, "target already has an environment with label development for this server; db copy never merges");
        assert_eq!(labels(dst2.environments("k").await.unwrap()), ["Library=library", "Staging=development"]);

        // The target Library has another label than the source Library.
        let dst3 = Store::open_in_memory().await.unwrap();
        let lib3 = dst3.ensure_library("k").await.unwrap();
        dst3.set_env_label(lib3, "lib").await.unwrap();
        let e = dst3.copy_from(&src, "lab", "k").await.unwrap_err().to_string();
        assert_eq!(e, "target Library has label lib, the source base; db copy never merges");
        assert_eq!(labels(dst3.environments("k").await.unwrap()), ["Library=lib"]);
    }

    #[tokio::test]
    async fn import_takes_labels_and_defaults_missing_ones() {
        let s = Store::open_in_memory().await.unwrap();
        let env = |name: &str, label: &str| ExportEnv { name: name.into(), prior: "Library".into(), protected: false, label: label.into() };
        let ex = Export { format: 1, content_views: vec![], environments: vec![env("Dev", "development"), env("QA", "")] };
        s.import("k", &ex).await.unwrap();
        let labels: Vec<_> = s.environments("k").await.unwrap().iter().map(|e| format!("{}={}", e.name, e.label)).collect();
        assert_eq!(labels, ["Library=library", "Dev=development", "QA=qa"]);
        // An old export has no label field.
        let old: Export =
            serde_json::from_str(r#"{"format":1,"content_views":[],"environments":[{"name":"Test","prior":"Library"}]}"#).unwrap();
        assert_eq!(old.environments[0].label, "");
        // A clash is reported, not skipped.
        let e = s.import("k", &Export { environments: vec![env("Other", "qa")], ..ex }).await.unwrap_err().to_string();
        assert_eq!(e, "label qa is already used by environment QA");
    }

    #[tokio::test]
    async fn import_checks_every_entry_before_it_writes() {
        let s = Store::open_in_memory().await.unwrap();
        // A rule without criteria would exclude every package; the composite must not be created without its components.
        let ex: Export = serde_json::from_value(serde_json::json!({ "format": 2, "environments": [], "content_views": [
            { "name": "all", "composite": true, "components": [{ "name": "os" }], "repos": [] },
            { "name": "os", "repos": [{ "repo_href": "/r/1/", "repo_name": "baseos", "repo_type": "rpm", "filters": [{ "action": "exclude" }] }] }
        ] }))
        .unwrap();
        let e = s.import("k", &ex).await.unwrap_err().to_string();
        assert_eq!(e, "os baseos: rule \"exclude\" has no criteria");
        assert!(s.content_views("k").await.unwrap().is_empty(), "nothing written");
        assert!(s.environments("k").await.unwrap().is_empty(), "not even Library");
    }

    #[tokio::test]
    async fn db_id_is_stable_and_aliases_work() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.db");
        let id = Store::open(&p).await.unwrap().db_id().to_string();
        assert_eq!(id.len(), 16);
        let s = Store::open(&p).await.unwrap();
        assert_eq!(s.db_id(), id);
        s.add_alias("abcd").await.unwrap();
        s.add_alias("abcd").await.unwrap(); // idempotent
        assert_eq!(s.aliases().await.unwrap(), vec!["abcd".to_string()]);
    }

    #[tokio::test]
    async fn external_database_keeps_one_db_id() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("cv.db").display());
        Store::migrate_external(&url, "cv.db").await.unwrap();
        let id = Store::open_external(&url, "cv.db").await.unwrap().db_id().to_string();
        assert_eq!(id.len(), 16);
        Store::migrate_external(&url, "cv.db").await.unwrap();
        assert_eq!(Store::open_external(&url, "cv.db").await.unwrap().db_id(), id);
    }

    #[tokio::test]
    async fn audit_records_actor() {
        let s = Store::open_in_memory().await.unwrap();
        s.audit("k", "alice@ws12", "cv.promote", "web v1 -> Dev").await.unwrap();
        assert_eq!(s.audit_log("k", 1).await.unwrap()[0].actor, "alice@ws12");
    }

    #[test]
    fn actor_is_user_at_host() {
        assert!(!hostname().is_empty());
        assert_eq!(actor(Some("alice")), format!("alice@{}", hostname()));
        assert_eq!(actor(None), format!("anonymous@{}", hostname()));
    }

    #[tokio::test]
    async fn locks_follow_their_content_view() {
        let s = Store::open_in_memory().await.unwrap();
        let cv = s.create_cv("k", "web", "").await.unwrap();
        let insert = format!("INSERT INTO locks VALUES ({cv}, 'a@h', 't', 'publish', '2026-01-01T00:00:00Z', 1)");
        sqlx::raw_sql(&insert).execute(&s.pool).await.unwrap();
        assert!(sqlx::raw_sql(&insert).execute(&s.pool).await.is_err(), "one lock per view");
        s.delete_cv(cv).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM locks").fetch_one(&s.pool).await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn lock_conflict_expiry_and_release() {
        let s = Store::open_in_memory().await.unwrap();
        let cv = s.create_cv("k", "web", "").await.unwrap();
        assert!(s.try_lock(cv, "alice@a", "t1", "promote to Prod", 1000).await.unwrap().is_ok());
        let h = s.try_lock(cv, "bob@b", "t2", "publish", 1050).await.unwrap().unwrap_err();
        assert_eq!((h.owner.as_str(), h.operation.as_str()), ("alice@a", "promote to Prod"));
        assert!(s.renew_lock(cv, "t1", 1100).await.unwrap());
        assert!(s.try_lock(cv, "bob@b", "t2", "publish", 1100 + LOCK_TTL_SECS - 1).await.unwrap().is_err());
        assert!(s.try_lock(cv, "bob@b", "t2", "publish", 1100 + LOCK_TTL_SECS + 1).await.unwrap().is_ok(), "expired lock taken over");
        assert!(!s.renew_lock(cv, "t1", 1300).await.unwrap(), "old holder lost it");
        s.release_lock(cv, "t1").await.unwrap(); // wrong token: no effect
        assert!(s.lock_holder(cv).await.unwrap().is_some());
        s.release_lock(cv, "t2").await.unwrap();
        assert!(s.lock_holder(cv).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn break_lock_removes_only_the_shown_holder() {
        let s = Store::open_in_memory().await.unwrap();
        let cv = s.create_cv("k", "web", "").await.unwrap();
        s.try_lock(cv, "alice@a", "t1", "publish", 1000).await.unwrap().unwrap();
        let shown = s.lock_holder(cv).await.unwrap().unwrap();
        assert_eq!((shown.owner.as_str(), shown.acquired_at.as_str()), ("alice@a", "1970-01-01T00:16:40Z"));
        // While the dialog was open, Alice's lock expired and Bob took it over.
        s.try_lock(cv, "bob@b", "t2", "promote to Prod", 1000 + LOCK_TTL_SECS + 1).await.unwrap().unwrap();
        assert!(!s.break_lock(cv, &shown).await.unwrap(), "Bob's lock is not the one shown");
        let now = s.lock_holder(cv).await.unwrap().unwrap();
        assert_eq!(now.owner, "bob@b");
        assert!(s.break_lock(cv, &now).await.unwrap());
        assert!(s.lock_holder(cv).await.unwrap().is_none());
        assert!(!s.break_lock(cv, &now).await.unwrap(), "nothing left to remove");
    }

    #[tokio::test]
    async fn writes_after_a_lost_lock_keep_the_new_holders_result() {
        let s = Store::open_in_memory().await.unwrap();
        let cv = s.create_cv("k", "web", "").await.unwrap();
        let lib = s.ensure_library("k").await.unwrap();
        let (v1, v2) = (s.begin_version(cv, "", &[]).await.unwrap(), s.begin_version(cv, "", &[]).await.unwrap());
        // Still publishing: the stop is recorded.
        s.fail_publishing_version(v1.id, "lock lost").await.unwrap();
        let v = s.version(v1.id).await.unwrap();
        assert_eq!((v.state.as_str(), v.error.as_deref()), ("failed", Some("lock lost")));
        // The new holder published it meanwhile: unchanged.
        s.finish_version(v2.id, "published", None).await.unwrap();
        s.fail_publishing_version(v2.id, "lock lost").await.unwrap();
        let v = s.version(v2.id).await.unwrap();
        assert_eq!((v.state.as_str(), v.error), ("published", None));

        // Still this promotion: the final state is recorded.
        s.set_binding(lib, cv, v1.id, "promoting").await.unwrap();
        s.finish_promoting_binding(lib, cv, v1.id, "partial").await.unwrap();
        assert_eq!(s.binding(lib, cv).await.unwrap().unwrap().state, "partial");
        // The new holder finished the same or another version meanwhile: unchanged.
        for (vid, state) in [(v1.id, "active"), (v2.id, "active"), (v2.id, "promoting")] {
            s.set_binding(lib, cv, vid, state).await.unwrap();
            s.finish_promoting_binding(lib, cv, v1.id, "partial").await.unwrap();
            let b = s.binding(lib, cv).await.unwrap().unwrap();
            assert_eq!((b.version_id, b.state.as_str()), (vid, state));
        }
    }

    #[tokio::test]
    async fn external_schema_is_checked_not_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.path().join("cv.db").display());
        let e = Store::open_external(&url, "cv.db").await.unwrap_err().to_string();
        assert!(e.contains("not initialized; run pulp-tui db migrate"), "{e}");
        let err = Store::open_external(&url, "cv.db").await.unwrap_err();
        let nm = err.downcast_ref::<NotMigrated>().unwrap();
        assert_eq!((nm.from, nm.to), (0, MIGRATIONS.len() as i64));
        let older = NotMigrated { display: "cv.db".into(), from: 1, to: 2 }.to_string();
        assert_eq!(older, "cv.db: database schema 1 is older than this pulp-tui (2); run pulp-tui db migrate");
        assert_eq!(Store::migrate_external(&url, "cv.db").await.unwrap(), (0, MIGRATIONS.len() as i64));
        assert_eq!(Store::migrate_external(&url, "cv.db").await.unwrap(), (MIGRATIONS.len() as i64, MIGRATIONS.len() as i64));
        let s = Store::open_external(&url, "cv.db").await.unwrap();
        assert!(s.external);
        let p = crate::config::Profile { url: "https://pulp.example.com".into(), ..Default::default() };
        assert_eq!(s.scope("lab", &p), p.server_key());
        assert_eq!(Store::open_in_memory().await.unwrap().scope("lab", &p), "lab");

        sqlx::raw_sql("PRAGMA user_version = 99").execute(&s.pool).await.unwrap();
        let e = Store::open_external(&url, "cv.db").await.unwrap_err().to_string();
        assert!(e.contains("is newer than this pulp-tui"), "{e}");
        // `db migrate` creates a new SQLite file without `mode=rwc` in the URL.
        let fresh = format!("sqlite://{}", dir.path().join("new.db").display());
        Store::migrate_external(&fresh, "new.db").await.unwrap();
    }

    #[tokio::test]
    async fn missing_sqlite_file_is_not_migrated_and_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("new.db");
        let e = Store::open_external(&format!("sqlite://{}", f.display()), "new.db").await.unwrap_err();
        assert_eq!(e.downcast_ref::<NotMigrated>().map(|n| n.from), Some(0));
        assert!(!f.exists(), "a normal open never creates the file");
    }

    #[tokio::test]
    async fn connect_error_hides_password() {
        let e = Store::open_external("postgres://u:hunter2@127.0.0.1:1/x", "postgres://u@127.0.0.1:1/x").await.unwrap_err();
        assert!(!format!("{e:#}").contains("hunter2"));
        assert!(format!("{e:#}").starts_with("postgres://u@127.0.0.1:1/x: "), "{e:#}");
    }

    #[tokio::test]
    async fn tasks_roundtrip() {
        let s = Store::open_in_memory().await.unwrap();
        s.track_task("lab", "/t/1/", "sync os", "running").await.unwrap();
        s.track_task("lab", "/t/2/", "publish", "waiting").await.unwrap();
        s.finish_task("lab", "/t/1/", "completed").await.unwrap();
        let t = s.tasks("lab", 10).await.unwrap();
        assert_eq!(t[0].href, "/t/2/"); // unfinished first
        assert_eq!(t[1].state, "completed");
    }

    #[tokio::test]
    async fn copy_remaps_ids_and_keeps_history() {
        let src = Store::open_in_memory().await.unwrap();
        let lib = src.ensure_library("lab").await.unwrap();
        let dev = src.create_env("lab", "Dev", lib, true).await.unwrap();
        let cv = src.create_cv("lab", "web", "d").await.unwrap();
        let repo = CvRepo { repo_href: "/r/1/".into(), repo_name: "os".into(), repo_type: RepoType::Rpm, filters: vec![] };
        src.set_cv_repo(cv, &repo).await.unwrap();
        let v = src.begin_version(cv, "n", std::slice::from_ref(&repo)).await.unwrap();
        src.finish_version(v.id, "published", None).await.unwrap();
        src.set_binding(dev, cv, v.id, "active").await.unwrap();
        src.audit("lab", "", "cv.promote", "web v1 -> Dev").await.unwrap();

        let dst = Store::open_in_memory().await.unwrap();
        dst.create_cv("other-server", "web", "").await.unwrap(); // other scope: no clash
        let r = dst.copy_from(&src, "lab", "https://h/pulp/#default").await.unwrap();
        assert_eq!((r.views, r.versions, r.environments, r.bindings, r.audit), (1, 1, 1, 1, 1));
        let k = "https://h/pulp/#default";
        let cvs = dst.content_views(k).await.unwrap();
        let envs = dst.environments(k).await.unwrap();
        let dev2 = envs.iter().find(|e| e.name == "Dev").unwrap();
        assert!(dev2.protected);
        let b = dst.binding(dev2.id, cvs[0].id).await.unwrap().unwrap();
        assert_eq!(dst.version(b.version_id).await.unwrap().number, 1);
        assert_eq!(dst.audit_log(k, 5).await.unwrap()[0].actor, "copied");
        assert_eq!(dst.aliases().await.unwrap(), vec![src.db_id().to_string()]);
    }

    #[tokio::test]
    async fn copy_keeps_everything_and_leaves_the_source_alone() {
        let src = Store::open_in_memory().await.unwrap();
        let lib = src.ensure_library("lab").await.unwrap();
        let dev = src.create_env("lab", "Dev", lib, false).await.unwrap();
        let prod = src.create_env("lab", "Prod", dev, true).await.unwrap();
        let cv = src.create_cv("lab", "web", "d").await.unwrap();
        let (os, tools) = (repo("os", RepoType::Rpm), repo("tools", RepoType::File));
        for r in [&os, &tools] {
            src.set_cv_repo(cv, r).await.unwrap();
        }
        let v1 = src.begin_version(cv, "one", &[os.clone(), tools.clone()]).await.unwrap();
        src.finish_version(v1.id, "failed", Some("boom")).await.unwrap();
        let v2 = src.begin_version(cv, "two", std::slice::from_ref(&os)).await.unwrap();
        src.finish_version(v2.id, "published", None).await.unwrap();
        let vr = CvVersionRepo {
            version_id: v2.id,
            repo: Some(os.clone()),
            source_version: Some("/rv/1/".into()),
            derived_repo: Some("/dr/".into()),
            derived_version: Some("/dr/versions/1/".into()),
            publication: Some("/pub/1/".into()),
            pending_task: Some("/t/9/".into()),
            content_count: Some(7),
            borrowed: false,
            signing: "none".into(),
        };
        src.save_version_repo(&vr).await.unwrap();
        for (env, ver) in [(dev, v1.id), (prod, v2.id)] {
            src.set_binding(env, cv, ver, "active").await.unwrap();
            let b = BindingRepo {
                env_id: env,
                cv_id: cv,
                repo_href: os.repo_href.clone(),
                repo_name: "os".into(),
                repo_type: RepoType::Rpm,
                distribution: format!("/d/{env}/"),
                base_path: format!("cv/{env}/os"),
                publication: Some("/pub/1/".into()),
            };
            src.save_binding_repo(&b).await.unwrap();
        }
        src.audit("lab", "ann@h", "cv.create", "web").await.unwrap();
        src.audit("lab", "", "cv.publish", "web v1").await.unwrap();

        let dst = Store::open_in_memory().await.unwrap();
        // Rows in the target shift every id: a copy that kept source ids would point at the wrong rows.
        let other = dst.create_cv("x", "pad", "").await.unwrap();
        let pad = dst.begin_version(other, "", &[]).await.unwrap();
        dst.set_binding(dst.ensure_library("x").await.unwrap(), other, pad.id, "active").await.unwrap();
        let r = dst.copy_from(&src, "lab", "k").await.unwrap();
        assert_eq!((r.views, r.versions, r.environments, r.bindings, r.audit), (1, 2, 2, 2, 2));

        let cv2 = dst.content_views("k").await.unwrap().remove(0);
        assert_eq!(
            (cv2.name.as_str(), cv2.description.as_str(), cv2.created_at.as_str()),
            ("web", "d", src.content_view(cv).await.unwrap().created_at.as_str())
        );
        assert_eq!(dst.cv_repos(cv2.id).await.unwrap().len(), 2);
        let (s1, s2) = (src.version(v1.id).await.unwrap(), src.version(v2.id).await.unwrap());
        let vs = dst.versions(cv2.id).await.unwrap();
        assert_eq!(vs.iter().map(|v| v.number).collect::<Vec<_>>(), vec![2, 1]);
        let (n2, n1) = (&vs[0], &vs[1]);
        assert_eq!(
            (&n1.notes, &n1.state, &n1.error, &n1.created_at, &n1.finished_at),
            (&s1.notes, &s1.state, &s1.error, &s1.created_at, &s1.finished_at)
        );
        assert_eq!(
            (&n2.notes, &n2.state, &n2.error, &n2.created_at, &n2.finished_at),
            (&s2.notes, &s2.state, &s2.error, &s2.created_at, &s2.finished_at)
        );
        assert_eq!(dst.version_repos(n1.id).await.unwrap().len(), 2);
        assert_eq!(dst.version_repos(n2.id).await.unwrap(), vec![CvVersionRepo { version_id: n2.id, ..vr.clone() }]);

        let envs = dst.environments("k").await.unwrap();
        assert_eq!(
            envs.iter().map(|e| (e.name.as_str(), e.protected)).collect::<Vec<_>>(),
            [("Library", false), ("Dev", false), ("Prod", true)]
        );
        assert_eq!((envs[1].prior_id, envs[2].prior_id), (Some(envs[0].id), Some(envs[1].id)));
        let b = dst.binding(envs[2].id, cv2.id).await.unwrap().unwrap();
        assert_eq!((b.version_id, b.state), (n2.id, "active".to_string()));
        assert_eq!(dst.binding(envs[1].id, cv2.id).await.unwrap().unwrap().version_id, n1.id);
        let brs = dst.binding_repos(envs[2].id, cv2.id).await.unwrap();
        assert_eq!((brs.len(), brs[0].cv_id, brs[0].base_path.as_str()), (1, cv2.id, format!("cv/{prod}/os").as_str()));
        let log = dst.audit_log("k", 5).await.unwrap();
        assert_eq!(
            log.iter().map(|a| (a.action.as_str(), a.actor.as_str())).collect::<Vec<_>>(),
            [("cv.publish", "copied"), ("cv.create", "ann@h")]
        );
        assert_eq!(dst.content_views("x").await.unwrap().len(), 1, "other scopes are untouched");

        // The source still has everything, and no alias.
        assert_eq!(src.content_views("lab").await.unwrap().len(), 1);
        assert_eq!(src.versions(cv).await.unwrap().len(), 2);
        assert_eq!(src.environments("lab").await.unwrap().len(), 3);
        assert_eq!(src.audit_log("lab", 5).await.unwrap().len(), 2);
        assert!(src.aliases().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn copy_refuses_name_clash() {
        let src = Store::open_in_memory().await.unwrap();
        src.create_cv("lab", "web", "").await.unwrap();
        let dst = Store::open_in_memory().await.unwrap();
        dst.create_cv("k", "web", "").await.unwrap();
        let e = dst.copy_from(&src, "lab", "k").await.unwrap_err().to_string();
        assert!(e.contains("never merges"), "{e}");
        assert_eq!(dst.content_views("k").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn copy_refuses_environment_clash_and_writes_nothing() {
        let src = Store::open_in_memory().await.unwrap();
        let lib = src.ensure_library("lab").await.unwrap();
        src.create_env("lab", "Dev", lib, false).await.unwrap();
        src.create_cv("lab", "api", "").await.unwrap();
        src.audit("lab", "a", "x", "y").await.unwrap();
        let dst = Store::open_in_memory().await.unwrap();
        let dlib = dst.ensure_library("k").await.unwrap();
        dst.create_env("k", "Dev", dlib, false).await.unwrap();
        let e = dst.copy_from(&src, "lab", "k").await.unwrap_err().to_string();
        assert_eq!(e, "target already has environment Dev for this server; db copy never merges");
        assert!(dst.content_views("k").await.unwrap().is_empty());
        assert!(dst.audit_log("k", 5).await.unwrap().is_empty());
        assert!(dst.aliases().await.unwrap().is_empty());
        let e = Store::open_in_memory().await.unwrap();
        e.create_cv("k", "api", "").await.unwrap();
        let msg = e.copy_from(&src, "lab", "k").await.unwrap_err().to_string();
        assert_eq!(msg, "target already has content view api for this server; db copy never merges");
    }

    #[tokio::test]
    async fn copy_refuses_the_same_database_and_live_locks() {
        let src = Store::open_in_memory().await.unwrap();
        let cv = src.create_cv("lab", "web", "").await.unwrap();
        assert!(src.copy_from(&src, "lab", "k").await.unwrap_err().to_string().contains("source and target are the same database"));
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        src.try_lock(cv, "alice@ws1", "t1", "publish", now).await.unwrap().unwrap();
        let dst = Store::open_in_memory().await.unwrap();
        let e = dst.copy_from(&src, "lab", "k").await.unwrap_err().to_string();
        assert_eq!(e, "Content View web is locked by alice@ws1 (publish); copy after it ends");
        assert!(dst.content_views("k").await.unwrap().is_empty());
        // A crashed session's lock (not renewed for LOCK_TTL_SECS) does not block.
        src.release_lock(cv, "t1").await.unwrap();
        src.try_lock(cv, "bob@ws2", "t2", "publish", now - LOCK_TTL_SECS - 1).await.unwrap().unwrap();
        assert_eq!(dst.copy_from(&src, "lab", "k").await.unwrap().views, 1);
    }

    #[tokio::test]
    async fn copy_failure_rolls_back_the_whole_target() {
        let src = Store::open_in_memory().await.unwrap();
        let cv = src.create_cv("lab", "web", "").await.unwrap();
        src.begin_version(cv, "", &[]).await.unwrap();
        src.audit("lab", "a", "x", "y").await.unwrap();
        let dst = Store::open_in_memory().await.unwrap();
        // A failure in the last step of the copy (the audit rows): everything written before it must go.
        sqlx::raw_sql("CREATE TRIGGER stop BEFORE INSERT ON audit BEGIN SELECT RAISE(ABORT, 'stop'); END")
            .execute(&dst.pool)
            .await
            .unwrap();
        assert!(dst.copy_from(&src, "lab", "k").await.is_err());
        assert!(dst.content_views("k").await.unwrap().is_empty());
        assert!(dst.environments("k").await.unwrap().is_empty());
        assert!(dst.aliases().await.unwrap().is_empty());
    }

    /// A published version of `cv` with one row that has a derived repository, version and publication
    /// (and a leftover task reference, which a borrowed row must not take).
    async fn published(s: &Store, cv: i64, name: &str) -> CvVersion {
        let r = repo(name, RepoType::Rpm);
        let v = s.begin_version(cv, "", std::slice::from_ref(&r)).await.unwrap();
        let vr = CvVersionRepo {
            version_id: v.id,
            repo: Some(r),
            derived_repo: Some(format!("/dr/{name}/")),
            derived_version: Some(format!("/dr/{name}/versions/{}/", v.number)),
            publication: Some(format!("/pub/{name}/{}/", v.number)),
            pending_task: Some("/t/done/".into()),
            ..Default::default()
        };
        s.save_version_repo(&vr).await.unwrap();
        s.finish_version(v.id, "published", None).await.unwrap();
        s.version(v.id).await.unwrap()
    }

    #[tokio::test]
    async fn components_pins_and_refusals() {
        let s = Store::open_in_memory().await.unwrap();
        let (os, extras) = (s.create_cv("k", "os", "").await.unwrap(), s.create_cv("k", "extras", "").await.unwrap());
        let c = s.create_composite("k", "rocky9", "").await.unwrap();
        let v1 = published(&s, os, "baseos").await;
        s.set_component(c, os, Some(v1.id)).await.unwrap();
        s.set_component(c, extras, None).await.unwrap();
        let got: Vec<_> = s.components(c).await.unwrap().into_iter().map(|x| (x.name, x.pinned)).collect();
        assert_eq!(got, [("extras".to_string(), None), ("os".to_string(), Some(v1.id))]);
        assert!(s.content_view(c).await.unwrap().composite && !s.content_view(os).await.unwrap().composite);
        let err = |r: Result<()>| r.unwrap_err().to_string();
        assert_eq!(err(s.set_component(os, extras, None).await), "os is not a composite content view");
        let other = s.create_composite("k", "other", "").await.unwrap();
        assert_eq!(err(s.set_component(other, c, None).await), "rocky9 is a composite; a composite cannot be a component");
        let failed = s.begin_version(extras, "", &[]).await.unwrap();
        s.finish_version(failed.id, "failed", None).await.unwrap();
        assert_eq!(err(s.set_component(c, extras, Some(failed.id)).await), "v1 is not a published version of extras");
        assert_eq!(
            err(s.set_component(c, extras, Some(v1.id)).await),
            "v1 is not a published version of extras",
            "a version of another view"
        );
        assert_eq!(s.components(c).await.unwrap()[0].pinned, None, "a refused pin changes nothing");
        assert!(s.auto_composites(extras).await.unwrap().is_empty(), "auto publish is off by default");
        s.set_auto_publish(c, true).await.unwrap();
        assert_eq!(s.auto_composites(extras).await.unwrap().iter().map(|v| v.id).collect::<Vec<_>>(), [c]);
        assert!(s.auto_composites(os).await.unwrap().is_empty(), "a pinned component does not trigger it");
        s.set_component(c, os, None).await.unwrap(); // upsert: os is latest now
        assert_eq!(s.auto_composites(os).await.unwrap().len(), 1);
        s.remove_component(c, extras).await.unwrap();
        assert_eq!(s.components(c).await.unwrap().iter().map(|x| x.cv_id).collect::<Vec<_>>(), [os]);
    }

    #[tokio::test]
    async fn composite_versions_borrow_rows_and_block_deletes() {
        let s = Store::open_in_memory().await.unwrap();
        let os = s.create_cv("k", "os", "").await.unwrap();
        let v1 = published(&s, os, "baseos").await;
        let c = s.create_composite("k", "rocky9", "").await.unwrap();
        s.set_component(c, os, Some(v1.id)).await.unwrap();
        let rows = s.version_repos(v1.id).await.unwrap();
        assert!(!rows[0].borrowed && rows[0].pending_task.is_some());
        let cv1 = s.begin_composite_version(c, "n", &[v1.id], &rows).await.unwrap();
        assert_eq!((cv1.number, cv1.state.as_str(), s.version(cv1.id).await.unwrap().state.as_str()), (1, "publishing", "publishing"));
        let want: Vec<_> =
            rows.iter().map(|r| CvVersionRepo { version_id: cv1.id, borrowed: true, pending_task: None, ..r.clone() }).collect();
        assert_eq!(s.version_repos(cv1.id).await.unwrap(), want);
        assert_eq!(s.version_repos(v1.id).await.unwrap(), rows, "the component's rows are unchanged");
        assert_eq!(
            s.version_components(cv1.id).await.unwrap(),
            [UsedVersion { cv_id: os, name: "os".into(), version_id: v1.id, number: 1 }]
        );
        assert_eq!(s.composite_users(v1.id).await.unwrap(), ["rocky9 v1", "rocky9 (pinned)"]);
        assert_eq!(s.composite_refs(os).await.unwrap(), ["rocky9 (component)", "rocky9 v1"]);
        assert!(s.delete_cv(os).await.is_err(), "still a component");
        assert_eq!(s.versions(os).await.unwrap().len(), 1, "a refused delete removes nothing");
        s.remove_component(c, os).await.unwrap();
        s.finish_version(cv1.id, "deleted", None).await.unwrap();
        assert!(s.composite_users(v1.id).await.unwrap().is_empty());
        assert!(s.composite_refs(os).await.unwrap().is_empty());
        s.delete_cv(os).await.unwrap();
        assert!(s.version_components(cv1.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn composite_version_refuses_a_component_version_no_longer_published() {
        let s = Store::open_in_memory().await.unwrap();
        let os = s.create_cv("k", "os", "").await.unwrap();
        let v1 = published(&s, os, "baseos").await;
        let c = s.create_composite("k", "rocky9", "").await.unwrap();
        let rows = s.version_repos(v1.id).await.unwrap();
        s.finish_version(v1.id, "deleted", None).await.unwrap(); // deleted after the composite resolved it
        let e = s.begin_composite_version(c, "", &[v1.id], &rows).await.unwrap_err();
        assert_eq!(e.to_string(), "version v1 of os is no longer published");
        assert!(s.versions(c).await.unwrap().is_empty(), "nothing written");
        assert!(s.composite_users(v1.id).await.unwrap().is_empty());
        assert_eq!(s.begin_version(c, "", &[]).await.unwrap().number, 1, "the number was not taken");
    }

    #[tokio::test]
    async fn export_v2_carries_composites_and_import_unpins() {
        let src = Store::open_in_memory().await.unwrap();
        let (os, extras) = (src.create_cv("lab", "os", "").await.unwrap(), src.create_cv("lab", "extras", "").await.unwrap());
        let v1 = published(&src, os, "baseos").await;
        let c = src.create_composite("lab", "rocky9", "").await.unwrap();
        src.set_auto_publish(c, true).await.unwrap();
        src.set_component(c, os, Some(v1.id)).await.unwrap();
        src.set_component(c, extras, None).await.unwrap();

        let ex = src.export("lab").await.unwrap();
        assert_eq!(ex.format, 2);
        let c = ex.content_views.iter().find(|v| v.name == "rocky9").unwrap();
        assert!(c.composite && c.auto_publish && c.repos.is_empty());
        assert_eq!(c.components.iter().map(|x| (x.name.as_str(), x.pinned)).collect::<Vec<_>>(), [("extras", None), ("os", Some(1))]);
        assert!(ex.content_views.iter().filter(|v| v.name != "rocky9").all(|v| !v.composite && v.components.is_empty()));

        let dst = Store::open_in_memory().await.unwrap();
        assert_eq!(
            dst.import("k", &ex).await.unwrap(),
            (3, 0, vec!["rocky9: component os was pinned to v1; imported as latest".to_string()])
        );
        let imported = dst.content_views("k").await.unwrap().into_iter().find(|v| v.name == "rocky9").unwrap();
        assert!(imported.composite && imported.auto_publish);
        let got = dst.components(imported.id).await.unwrap();
        assert_eq!(got.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["extras", "os"]);
        assert!(got.iter().all(|x| x.pinned.is_none()));
        assert_eq!(dst.import("k", &ex).await.unwrap(), (0, 0, vec![]));

        // A component that is missing or is a composite is skipped with a named warning.
        let comp = |name: &str| ExportComponent { name: name.into(), pinned: None };
        let view = |name: &str, composite: bool, components| ExportView {
            name: name.into(),
            description: String::new(),
            composite,
            auto_publish: false,
            components,
            repos: vec![],
        };
        let odd = Export {
            format: 2,
            content_views: vec![view("odd", true, vec![comp("gone"), comp("rocky9"), comp("os")]), view("plain", false, vec![])],
            environments: vec![],
        };
        assert_eq!(
            dst.import("k", &odd).await.unwrap(),
            (2, 0, vec!["odd: component gone not found; skipped".to_string(), "odd: component rocky9 is a composite; skipped".to_string()])
        );
        let odd_id = dst.content_views("k").await.unwrap().into_iter().find(|v| v.name == "odd").unwrap().id;
        assert_eq!(dst.components(odd_id).await.unwrap().len(), 1);

        // A file from before format 2 has none of the new fields.
        let old: Export = serde_json::from_str(r#"{"format":1,"content_views":[{"name":"old","repos":[]}],"environments":[]}"#).unwrap();
        assert_eq!(dst.import("k", &old).await.unwrap(), (1, 0, vec![]));
        assert!(!dst.content_views("k").await.unwrap().iter().find(|v| v.name == "old").unwrap().composite);

        let e = dst.import("k", &Export { format: 3, ..ex }).await.unwrap_err().to_string();
        assert_eq!(e, "unsupported export format 3");
    }

    #[tokio::test]
    async fn copy_keeps_composites() {
        let src = Store::open_in_memory().await.unwrap();
        let (os, extras) = (src.create_cv("lab", "rocky9-os", "").await.unwrap(), src.create_cv("lab", "rocky9-extras", "").await.unwrap());
        let (vo, ve) = (published(&src, os, "baseos").await, published(&src, extras, "epel").await);
        let c = src.create_composite("lab", "rocky9", "").await.unwrap(); // sorts before its components
        src.set_component(c, os, Some(vo.id)).await.unwrap();
        src.set_component(c, extras, None).await.unwrap();
        src.set_auto_publish(c, true).await.unwrap();
        let mut rows = src.version_repos(vo.id).await.unwrap();
        rows.extend(src.version_repos(ve.id).await.unwrap());
        let cv1 = src.begin_composite_version(c, "", &[vo.id, ve.id], &rows).await.unwrap();
        src.finish_version(cv1.id, "published", None).await.unwrap();

        let dst = Store::open_in_memory().await.unwrap();
        // Rows in the target shift every id: a copy that kept source ids would point at the wrong rows.
        let pad = dst.create_cv("x", "pad", "").await.unwrap();
        dst.begin_version(pad, "", &[]).await.unwrap();
        let r = dst.copy_from(&src, "lab", "k").await.unwrap();
        assert_eq!((r.views, r.versions), (3, 3));
        let views = dst.content_views("k").await.unwrap();
        assert_eq!(views.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(), ["rocky9", "rocky9-extras", "rocky9-os"]);
        let (c2, ex2, os2) = (&views[0], &views[1], &views[2]);
        assert!(c2.composite && c2.auto_publish && !os2.composite && !os2.auto_publish);
        let (vo2, ve2) = (dst.versions(os2.id).await.unwrap()[0].id, dst.versions(ex2.id).await.unwrap()[0].id);
        let got: Vec<_> = dst.components(c2.id).await.unwrap().into_iter().map(|x| (x.cv_id, x.name, x.pinned)).collect();
        assert_eq!(got, [(ex2.id, "rocky9-extras".to_string(), None), (os2.id, "rocky9-os".to_string(), Some(vo2))]);
        let n1 = dst.versions(c2.id).await.unwrap().remove(0);
        let used: Vec<_> = dst.version_components(n1.id).await.unwrap().into_iter().map(|u| (u.name, u.number, u.version_id)).collect();
        assert_eq!(used, [("rocky9-extras".to_string(), 1, ve2), ("rocky9-os".to_string(), 1, vo2)]);
        let copied = dst.version_repos(n1.id).await.unwrap();
        assert!(copied.len() == 2 && copied.iter().all(|r| r.borrowed));
        let want: Vec<_> = src.version_repos(cv1.id).await.unwrap().into_iter().map(|r| CvVersionRepo { version_id: n1.id, ..r }).collect();
        assert_eq!(copied, want, "publications equal the source rows");
        assert!(!dst.version_repos(vo2).await.unwrap()[0].borrowed, "a component's own row is not borrowed");
    }
}
