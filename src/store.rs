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
];

/// A Content View lock not renewed for this long is expired; the next operator takes it over.
pub const LOCK_TTL_SECS: i64 = 120;
/// How often the holder renews its lock.
pub const LOCK_RENEW_SECS: u64 = 30;

/// `pg_advisory_lock` key held while `db migrate` runs.
const MIGRATE_LOCK: i64 = 7_301_955;
/// `scope_settings` key of the base path prefix a server's distributions use.
const PREFIX_KEY: &str = "cv_base_path_prefix";
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
    /// publishing | published | failed | deleted
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
    match s {
        "rpm" => Ok(RepoType::Rpm),
        "file" => Ok(RepoType::File),
        _ => bail!("unknown repository type {s:?}"),
    }
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

    /// The `cv_base_path_prefix` this server has used so far, if any.
    pub async fn stored_prefix(&self, scope: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT value FROM scope_settings WHERE scope = $1 AND key = $2")
            .bind(scope)
            .bind(PREFIX_KEY)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// The first prefix used for this server wins: later config changes must not move served paths.
    pub async fn cv_prefix(&self, scope: &str, local: &str) -> Result<String> {
        sqlx::query("INSERT INTO scope_settings(scope, key, value) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
            .bind(scope)
            .bind(PREFIX_KEY)
            .bind(local)
            .execute(&self.pool)
            .await?;
        Ok(self.stored_prefix(scope).await?.unwrap_or_else(|| local.to_string()))
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
        let rows = sqlx::query("SELECT id, name, description, created_at FROM content_views WHERE profile = $1 ORDER BY name")
            .bind(profile)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(content_view_row).collect()
    }

    pub async fn content_view(&self, id: i64) -> Result<ContentView> {
        let r =
            sqlx::query("SELECT id, name, description, created_at FROM content_views WHERE id = $1").bind(id).fetch_one(&self.pool).await?;
        content_view_row(&r)
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
                    pending_task, content_count FROM cv_version_repos WHERE version_id = $1 ORDER BY repo_name",
        )
        .bind(version_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(CvVersionRepo {
                    version_id,
                    repo: Some(cv_repo_row(r)?),
                    source_version: r.try_get(4)?,
                    derived_repo: r.try_get(5)?,
                    derived_version: r.try_get(6)?,
                    publication: r.try_get(7)?,
                    pending_task: r.try_get(8)?,
                    content_count: r.try_get(9)?,
                })
            })
            .collect()
    }

    /// Persist a checkpoint after each workflow step (enables resume after a crash).
    pub async fn save_version_repo(&self, vr: &CvVersionRepo) -> Result<()> {
        sqlx::query(
            "UPDATE cv_version_repos SET source_version = $3, derived_repo = $4, derived_version = $5, publication = $6,
                    pending_task = $7, content_count = $8 WHERE version_id = $1 AND repo_href = $2",
        )
        .bind(vr.version_id)
        .bind(&vr.repo().repo_href)
        .bind(&vr.source_version)
        .bind(&vr.derived_repo)
        .bind(&vr.derived_version)
        .bind(&vr.publication)
        .bind(&vr.pending_task)
        .bind(vr.content_count)
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
        sqlx::query("INSERT INTO environments(profile, name, prior_id) VALUES ($1, $2, NULL) ON CONFLICT(profile, name) DO NOTHING")
            .bind(profile)
            .bind(LIBRARY)
            .execute(&self.pool)
            .await?;
        Ok(sqlx::query_scalar("SELECT id FROM environments WHERE profile = $1 AND name = $2")
            .bind(profile)
            .bind(LIBRARY)
            .fetch_one(&self.pool)
            .await?)
    }

    pub async fn create_env(&self, profile: &str, name: &str, prior_id: i64, protected: bool) -> Result<i64> {
        validate_name(name)?;
        let id = sqlx::query_scalar("INSERT INTO environments(profile, name, prior_id, protected) VALUES ($1, $2, $3, $4) RETURNING id")
            .bind(profile)
            .bind(name)
            .bind(prior_id)
            .bind(protected as i64)
            .fetch_one(&self.pool)
            .await
            .with_context(|| format!("environment {name} already exists?"))?;
        Ok(id)
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
        let rows = sqlx::query("SELECT id, name, prior_id, protected FROM environments WHERE profile = $1 ORDER BY id")
            .bind(profile)
            .fetch_all(&self.pool)
            .await?;
        let all = rows
            .iter()
            .map(|r| {
                let protected: i64 = r.try_get(3)?;
                Ok(Environment { id: r.try_get(0)?, name: r.try_get(1)?, prior_id: r.try_get(2)?, protected: protected != 0 })
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
            views.push(ExportView { name: cv.name, description: cv.description, repos: self.cv_repos(cv.id).await? });
        }
        let envs = self.environments(profile).await?;
        let name_of = |id: Option<i64>| id.and_then(|i| envs.iter().find(|e| e.id == i)).map(|e| e.name.clone());
        let environments = envs
            .iter()
            .filter(|e| e.prior_id.is_some())
            .map(|e| ExportEnv { name: e.name.clone(), prior: name_of(e.prior_id).unwrap_or_default(), protected: e.protected })
            .collect();
        Ok(Export { format: 1, content_views: views, environments })
    }

    /// Import definitions; existing objects with the same name are left unchanged.
    pub async fn import(&self, profile: &str, ex: &Export) -> Result<(usize, usize)> {
        if ex.format != 1 {
            bail!("unsupported export format {}", ex.format);
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
                    Some(p) => envs += self.create_env(profile, &e.name, p.id, e.protected).await.is_ok() as usize,
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
        for v in &ex.content_views {
            if existing.iter().any(|c| c.name == v.name) {
                continue;
            }
            let id = self.create_cv(profile, &v.name, &v.description).await?;
            for r in &v.repos {
                self.set_cv_repo(id, r).await?;
            }
            cvs += 1;
        }
        Ok((cvs, envs))
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
        sqlx::query("INSERT INTO environments(profile, name, prior_id) VALUES ($1, $2, NULL) ON CONFLICT(profile, name) DO NOTHING")
            .bind(dst_scope)
            .bind(LIBRARY)
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
                    sqlx::query_scalar("INSERT INTO environments(profile, name, prior_id, protected) VALUES ($1, $2, $3, $4) RETURNING id")
                        .bind(dst_scope)
                        .bind(&e.name)
                        .bind(env_ids[&prior])
                        .bind(e.protected as i64)
                        .fetch_one(&mut *tx)
                        .await?
                }
            };
            env_ids.insert(e.id, id);
        }
        for v in &views {
            let cv_id: i64 = sqlx::query_scalar(
                "INSERT INTO content_views(profile, name, description, created_at) VALUES ($1, $2, $3, $4) RETURNING id",
            )
            .bind(dst_scope)
            .bind(&v.name)
            .bind(&v.description)
            .bind(&v.created_at)
            .fetch_one(&mut *tx)
            .await?;
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
            let mut version_ids = HashMap::new();
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
                    let r = vr.repo();
                    sqlx::query(
                        "INSERT INTO cv_version_repos(version_id, repo_href, repo_name, repo_type, filters, source_version, derived_repo,
                                derived_version, publication, pending_task, content_count)
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
                    )
                    .bind(id)
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
                    .execute(&mut *tx)
                    .await?;
                }
            }
            let bindings = src.bindings_for_cv(v.id).await?;
            report.bindings += bindings.len();
            for b in &bindings {
                let env_id = env_ids.get(&b.env_id).copied().context("a binding points at an environment outside the path")?;
                let version_id = version_ids.get(&b.version_id).copied().context("a binding points at a version of another view")?;
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
        // The base path prefix stays with the server: promotions after the copy keep the same paths.
        if let Some(prefix) = src.stored_prefix(src_scope).await? {
            sqlx::query("INSERT INTO scope_settings(scope, key, value) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
                .bind(dst_scope)
                .bind(PREFIX_KEY)
                .bind(prefix)
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
    pub repos: Vec<CvRepo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportEnv {
    pub name: String,
    pub prior: String,
    #[serde(default)]
    pub protected: bool,
}

fn lock_row(r: &AnyRow) -> Result<LockHolder> {
    Ok(LockHolder { owner: r.try_get(0)?, operation: r.try_get(1)?, acquired_at: r.try_get(2)? })
}

fn content_view_row(r: &AnyRow) -> Result<ContentView> {
    Ok(ContentView { id: r.try_get(0)?, name: r.try_get(1)?, description: r.try_get(2)?, created_at: r.try_get(3)? })
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
        let _ = prod;

        let ex = s.export("lab").await.unwrap();
        let s2 = Store::open_in_memory().await.unwrap();
        assert_eq!(s2.import("lab", &ex).await.unwrap(), (1, 2));
        assert_eq!(s2.import("lab", &ex).await.unwrap(), (0, 0));
        assert_eq!(s2.export("lab").await.unwrap(), ex);

        s.delete_cv(cv).await.unwrap();
        assert!(s.versions(cv).await.unwrap().is_empty());
    }

    #[test]
    fn migration_lists_match() {
        assert_eq!(MIGRATIONS.len(), PG_MIGRATIONS.len());
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
    async fn first_prefix_wins() {
        let s = Store::open_in_memory().await.unwrap();
        assert_eq!(s.stored_prefix("k").await.unwrap(), None);
        assert_eq!(s.cv_prefix("k", "cv").await.unwrap(), "cv");
        assert_eq!(s.cv_prefix("k", "other").await.unwrap(), "cv");
        assert_eq!(s.stored_prefix("k").await.unwrap().as_deref(), Some("cv"));
        assert_eq!(s.cv_prefix("k2", "other").await.unwrap(), "other"); // per server
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
        src.cv_prefix("lab", "acme").await.unwrap();
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
        assert_eq!(dst.stored_prefix("k").await.unwrap().as_deref(), Some("acme"));
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
}
