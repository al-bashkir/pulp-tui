//! Local SQLite store for application-owned state that Pulp does not persist: tracked
//! task references, Content View definitions/versions, lifecycle environments, bindings,
//! workflow checkpoints and an audit trail. Everything is scoped by profile name.
//!
//! Limits: single operator per database file. Migrations are versioned with
//! `PRAGMA user_version`; the schema avoids SQLite-only features so it can move to a
//! shared PostgreSQL service when several operators must coordinate.

use crate::filters::FilterRule;
use crate::pulp::RepoType;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

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
];

pub struct Store {
    conn: Mutex<Connection>,
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
}

pub const LIBRARY: &str = "Library";

fn repo_type(s: String) -> rusqlite::Result<RepoType> {
    match s.as_str() {
        "rpm" => Ok(RepoType::Rpm),
        "file" => Ok(RepoType::File),
        _ => Err(rusqlite::Error::InvalidColumnType(0, s, rusqlite::types::Type::Text)),
    }
}

fn type_str(t: RepoType) -> &'static str {
    t.component()
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        Self::init(Connection::open(path).with_context(|| format!("open {}", path.display()))?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let v = conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? as usize;
        if v > MIGRATIONS.len() {
            bail!("database schema {v} is newer than this pulp-tui ({}); upgrade pulp-tui", MIGRATIONS.len());
        }
        for (i, m) in MIGRATIONS.iter().enumerate().skip(v) {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(m).with_context(|| format!("migration {}", i + 1))?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
        }
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock cannot leave SQLite inconsistent (transactions roll back).
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- tasks -------------------------------------------------------------------

    pub fn track_task(&self, profile: &str, href: &str, label: &str, state: &str) -> Result<()> {
        self.db().execute(
            "INSERT INTO tasks(profile, href, label, state, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(profile, href) DO UPDATE SET state = excluded.state",
            params![profile, href, label, state, now()],
        )?;
        Ok(())
    }

    pub fn finish_task(&self, profile: &str, href: &str, state: &str) -> Result<()> {
        self.db().execute(
            "UPDATE tasks SET state = ?3, finished_at = ?4 WHERE profile = ?1 AND href = ?2",
            params![profile, href, state, now()],
        )?;
        Ok(())
    }

    /// Recent task references (unfinished first) to reattach after a restart.
    pub fn tasks(&self, profile: &str, limit: usize) -> Result<Vec<TaskRef>> {
        let db = self.db();
        let mut st = db.prepare(
            "SELECT href, label, state, created_at FROM tasks WHERE profile = ?1
             ORDER BY finished_at IS NOT NULL, created_at DESC LIMIT ?2",
        )?;
        let rows = st.query_map(params![profile, limit as i64], |r| {
            Ok(TaskRef { href: r.get(0)?, label: r.get(1)?, state: r.get(2)?, created_at: r.get(3)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- content views -----------------------------------------------------------

    pub fn create_cv(&self, profile: &str, name: &str, description: &str) -> Result<i64> {
        validate_name(name)?;
        let db = self.db();
        db.execute(
            "INSERT INTO content_views(profile, name, description, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![profile, name, description, now()],
        )
        .with_context(|| format!("content view {name} already exists?"))?;
        Ok(db.last_insert_rowid())
    }

    pub fn update_cv(&self, id: i64, description: &str) -> Result<()> {
        self.db().execute("UPDATE content_views SET description = ?2 WHERE id = ?1", params![id, description])?;
        Ok(())
    }

    pub fn delete_cv(&self, id: i64) -> Result<()> {
        let db = self.db();
        let tx = db.unchecked_transaction()?;
        tx.execute("DELETE FROM bindings WHERE cv_id = ?1", [id])?;
        tx.execute("DELETE FROM content_views WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn content_views(&self, profile: &str) -> Result<Vec<ContentView>> {
        let db = self.db();
        let mut st = db.prepare("SELECT id, name, description, created_at FROM content_views WHERE profile = ?1 ORDER BY name")?;
        let rows =
            st.query_map([profile], |r| Ok(ContentView { id: r.get(0)?, name: r.get(1)?, description: r.get(2)?, created_at: r.get(3)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn content_view(&self, id: i64) -> Result<ContentView> {
        Ok(self.db().query_row("SELECT id, name, description, created_at FROM content_views WHERE id = ?1", [id], |r| {
            Ok(ContentView { id: r.get(0)?, name: r.get(1)?, description: r.get(2)?, created_at: r.get(3)? })
        })?)
    }

    pub fn set_cv_repo(&self, cv_id: i64, repo: &CvRepo) -> Result<()> {
        self.db().execute(
            "INSERT INTO cv_repos(cv_id, repo_href, repo_name, repo_type, filters) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(cv_id, repo_href) DO UPDATE SET filters = excluded.filters, repo_name = excluded.repo_name",
            params![cv_id, repo.repo_href, repo.repo_name, type_str(repo.repo_type), serde_json::to_string(&repo.filters)?],
        )?;
        Ok(())
    }

    pub fn remove_cv_repo(&self, cv_id: i64, repo_href: &str) -> Result<()> {
        self.db().execute("DELETE FROM cv_repos WHERE cv_id = ?1 AND repo_href = ?2", params![cv_id, repo_href])?;
        Ok(())
    }

    pub fn cv_repos(&self, cv_id: i64) -> Result<Vec<CvRepo>> {
        let db = self.db();
        let mut st = db.prepare("SELECT repo_href, repo_name, repo_type, filters FROM cv_repos WHERE cv_id = ?1 ORDER BY repo_name")?;
        let rows = st.query_map([cv_id], |r| {
            Ok(CvRepo {
                repo_href: r.get(0)?,
                repo_name: r.get(1)?,
                repo_type: repo_type(r.get(2)?)?,
                filters: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- content view versions ---------------------------------------------------

    /// Start a new numbered version with a frozen copy of the definition.
    pub fn begin_version(&self, cv_id: i64, notes: &str, repos: &[CvRepo]) -> Result<CvVersion> {
        let db = self.db();
        let tx = db.unchecked_transaction()?;
        let number: i64 = tx.query_row("SELECT COALESCE(MAX(number), 0) + 1 FROM cv_versions WHERE cv_id = ?1", [cv_id], |r| r.get(0))?;
        let at = now();
        tx.execute(
            "INSERT INTO cv_versions(cv_id, number, notes, state, created_at) VALUES (?1, ?2, ?3, 'publishing', ?4)",
            params![cv_id, number, notes, at],
        )?;
        let id = tx.last_insert_rowid();
        for r in repos {
            tx.execute(
                "INSERT INTO cv_version_repos(version_id, repo_href, repo_name, repo_type, filters) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, r.repo_href, r.repo_name, type_str(r.repo_type), serde_json::to_string(&r.filters)?],
            )?;
        }
        tx.commit()?;
        Ok(CvVersion { id, cv_id, number, notes: notes.into(), state: "publishing".into(), created_at: at, finished_at: None, error: None })
    }

    pub fn finish_version(&self, id: i64, state: &str, error: Option<&str>) -> Result<()> {
        self.db()
            .execute("UPDATE cv_versions SET state = ?2, error = ?3, finished_at = ?4 WHERE id = ?1", params![id, state, error, now()])?;
        Ok(())
    }

    pub fn versions(&self, cv_id: i64) -> Result<Vec<CvVersion>> {
        let db = self.db();
        let mut st = db.prepare(
            "SELECT id, cv_id, number, notes, state, created_at, finished_at, error FROM cv_versions WHERE cv_id = ?1 ORDER BY number DESC",
        )?;
        let rows = st.query_map([cv_id], version_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn version(&self, id: i64) -> Result<CvVersion> {
        Ok(self.db().query_row(
            "SELECT id, cv_id, number, notes, state, created_at, finished_at, error FROM cv_versions WHERE id = ?1",
            [id],
            version_row,
        )?)
    }

    pub fn version_repos(&self, version_id: i64) -> Result<Vec<CvVersionRepo>> {
        let db = self.db();
        let mut st = db.prepare(
            "SELECT repo_href, repo_name, repo_type, filters, source_version, derived_repo, derived_version, publication,
                    pending_task, content_count FROM cv_version_repos WHERE version_id = ?1 ORDER BY repo_name",
        )?;
        let rows = st.query_map([version_id], |r| {
            Ok(CvVersionRepo {
                version_id,
                repo: Some(CvRepo {
                    repo_href: r.get(0)?,
                    repo_name: r.get(1)?,
                    repo_type: repo_type(r.get(2)?)?,
                    filters: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
                }),
                source_version: r.get(4)?,
                derived_repo: r.get(5)?,
                derived_version: r.get(6)?,
                publication: r.get(7)?,
                pending_task: r.get(8)?,
                content_count: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Persist a checkpoint after each workflow step (enables resume after a crash).
    pub fn save_version_repo(&self, vr: &CvVersionRepo) -> Result<()> {
        self.db().execute(
            "UPDATE cv_version_repos SET source_version = ?3, derived_repo = ?4, derived_version = ?5, publication = ?6,
                    pending_task = ?7, content_count = ?8 WHERE version_id = ?1 AND repo_href = ?2",
            params![
                vr.version_id,
                vr.repo().repo_href,
                vr.source_version,
                vr.derived_repo,
                vr.derived_version,
                vr.publication,
                vr.pending_task,
                vr.content_count
            ],
        )?;
        Ok(())
    }

    /// Derived repository versions referenced by any non-deleted CV version (retention guard).
    pub fn protected_versions(&self, profile: &str) -> Result<Vec<String>> {
        let db = self.db();
        let mut st = db.prepare(
            "SELECT vr.derived_version FROM cv_version_repos vr JOIN cv_versions v ON v.id = vr.version_id
             JOIN content_views c ON c.id = v.cv_id
             WHERE c.profile = ?1 AND v.state != 'deleted' AND vr.derived_version IS NOT NULL",
        )?;
        let rows = st.query_map([profile], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- environments and bindings -----------------------------------------------

    pub fn ensure_library(&self, profile: &str) -> Result<i64> {
        let db = self.db();
        db.execute("INSERT OR IGNORE INTO environments(profile, name, prior_id) VALUES (?1, ?2, NULL)", params![profile, LIBRARY])?;
        Ok(db.query_row("SELECT id FROM environments WHERE profile = ?1 AND name = ?2", params![profile, LIBRARY], |r| r.get(0))?)
    }

    pub fn create_env(&self, profile: &str, name: &str, prior_id: i64, protected: bool) -> Result<i64> {
        validate_name(name)?;
        let db = self.db();
        db.execute(
            "INSERT INTO environments(profile, name, prior_id, protected) VALUES (?1, ?2, ?3, ?4)",
            params![profile, name, prior_id, protected],
        )
        .with_context(|| format!("environment {name} already exists?"))?;
        Ok(db.last_insert_rowid())
    }

    pub fn set_env_protected(&self, id: i64, protected: bool) -> Result<()> {
        self.db().execute("UPDATE environments SET protected = ?2 WHERE id = ?1", params![id, protected])?;
        Ok(())
    }

    pub fn delete_env(&self, id: i64) -> Result<()> {
        let db = self.db();
        let used: i64 = db.query_row(
            "SELECT (SELECT COUNT(*) FROM bindings WHERE env_id = ?1) + (SELECT COUNT(*) FROM environments WHERE prior_id = ?1)",
            [id],
            |r| r.get(0),
        )?;
        if used > 0 {
            bail!("environment is in use (bindings or successor environments exist)");
        }
        db.execute("DELETE FROM binding_repos WHERE env_id = ?1", [id])?;
        db.execute("DELETE FROM environments WHERE id = ?1 AND prior_id IS NOT NULL", [id])?;
        Ok(())
    }

    /// Environments in path order (Library first, then each successor chain).
    pub fn environments(&self, profile: &str) -> Result<Vec<Environment>> {
        let db = self.db();
        let mut st = db.prepare("SELECT id, name, prior_id, protected FROM environments WHERE profile = ?1 ORDER BY id")?;
        let all: Vec<Environment> = st
            .query_map([profile], |r| Ok(Environment { id: r.get(0)?, name: r.get(1)?, prior_id: r.get(2)?, protected: r.get(3)? }))?
            .collect::<rusqlite::Result<_>>()?;
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

    pub fn binding(&self, env_id: i64, cv_id: i64) -> Result<Option<Binding>> {
        Ok(self
            .db()
            .query_row(
                "SELECT env_id, cv_id, version_id, state, updated_at FROM bindings WHERE env_id = ?1 AND cv_id = ?2",
                params![env_id, cv_id],
                binding_row,
            )
            .optional()?)
    }

    pub fn bindings_for_cv(&self, cv_id: i64) -> Result<Vec<Binding>> {
        let db = self.db();
        let mut st = db.prepare("SELECT env_id, cv_id, version_id, state, updated_at FROM bindings WHERE cv_id = ?1")?;
        let rows = st.query_map([cv_id], binding_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_binding(&self, env_id: i64, cv_id: i64, version_id: i64, state: &str) -> Result<()> {
        self.db().execute(
            "INSERT INTO bindings(env_id, cv_id, version_id, state, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(env_id, cv_id) DO UPDATE SET version_id = excluded.version_id, state = excluded.state, updated_at = excluded.updated_at",
            params![env_id, cv_id, version_id, state, now()],
        )?;
        Ok(())
    }

    pub fn binding_repos(&self, env_id: i64, cv_id: i64) -> Result<Vec<BindingRepo>> {
        let db = self.db();
        let mut st = db.prepare(
            "SELECT env_id, cv_id, repo_href, repo_name, repo_type, distribution, base_path, publication
             FROM binding_repos WHERE env_id = ?1 AND cv_id = ?2 ORDER BY repo_name",
        )?;
        let rows = st.query_map(params![env_id, cv_id], binding_repo_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn all_binding_repos(&self, cv_id: i64) -> Result<Vec<BindingRepo>> {
        let db = self.db();
        let mut st = db.prepare(
            "SELECT env_id, cv_id, repo_href, repo_name, repo_type, distribution, base_path, publication
             FROM binding_repos WHERE cv_id = ?1",
        )?;
        let rows = st.query_map([cv_id], binding_repo_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn save_binding_repo(&self, b: &BindingRepo) -> Result<()> {
        self.db().execute(
            "INSERT INTO binding_repos(env_id, cv_id, repo_href, repo_name, repo_type, distribution, base_path, publication)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(env_id, cv_id, repo_href) DO UPDATE SET distribution = excluded.distribution,
                base_path = excluded.base_path, publication = excluded.publication",
            params![b.env_id, b.cv_id, b.repo_href, b.repo_name, type_str(b.repo_type), b.distribution, b.base_path, b.publication],
        )?;
        Ok(())
    }

    /// Versions currently bound to any environment (cannot be deleted).
    pub fn version_in_use(&self, version_id: i64) -> Result<Vec<String>> {
        let db = self.db();
        let mut st = db.prepare("SELECT e.name FROM bindings b JOIN environments e ON e.id = b.env_id WHERE b.version_id = ?1")?;
        let rows = st.query_map([version_id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- audit and replication -----------------------------------------------------

    pub fn audit(&self, profile: &str, action: &str, detail: &str) -> Result<()> {
        self.db()
            .execute("INSERT INTO audit(profile, at, action, detail) VALUES (?1, ?2, ?3, ?4)", params![profile, now(), action, detail])?;
        Ok(())
    }

    pub fn audit_log(&self, profile: &str, limit: usize) -> Result<Vec<AuditEntry>> {
        let db = self.db();
        let mut st = db.prepare("SELECT at, action, detail FROM audit WHERE profile = ?1 ORDER BY id DESC LIMIT ?2")?;
        let rows =
            st.query_map(params![profile, limit as i64], |r| Ok(AuditEntry { at: r.get(0)?, action: r.get(1)?, detail: r.get(2)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn record_replication(&self, profile: &str, source: &str, distribution: &str, state: &str, detail: &str) -> Result<()> {
        self.db().execute(
            "INSERT INTO replication_runs(profile, source_profile, distribution, state, at, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![profile, source, distribution, state, now(), detail],
        )?;
        Ok(())
    }

    /// Last successful replication time per distribution base path.
    pub fn last_replications(&self, profile: &str, source: &str) -> Result<Vec<(String, String)>> {
        let db = self.db();
        let mut st = db.prepare(
            "SELECT distribution, MAX(at) FROM replication_runs WHERE profile = ?1 AND source_profile = ?2 AND state = 'completed'
             GROUP BY distribution",
        )?;
        let rows = st.query_map(params![profile, source], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- export / import (definitions only, no secrets, no Pulp state) ---------------

    pub fn export(&self, profile: &str) -> Result<Export> {
        let mut views = vec![];
        for cv in self.content_views(profile)? {
            views.push(ExportView { name: cv.name, description: cv.description, repos: self.cv_repos(cv.id)? });
        }
        let envs = self.environments(profile)?;
        let name_of = |id: Option<i64>| id.and_then(|i| envs.iter().find(|e| e.id == i)).map(|e| e.name.clone());
        let environments = envs
            .iter()
            .filter(|e| e.prior_id.is_some())
            .map(|e| ExportEnv { name: e.name.clone(), prior: name_of(e.prior_id).unwrap_or_default(), protected: e.protected })
            .collect();
        Ok(Export { format: 1, content_views: views, environments })
    }

    /// Import definitions; existing objects with the same name are left unchanged.
    pub fn import(&self, profile: &str, ex: &Export) -> Result<(usize, usize)> {
        if ex.format != 1 {
            bail!("unsupported export format {}", ex.format);
        }
        self.ensure_library(profile)?;
        let mut envs = 0;
        // Parents come before children in exports; loop until no progress for safety.
        let mut pending: Vec<&ExportEnv> = ex.environments.iter().collect();
        while !pending.is_empty() {
            let before = pending.len();
            let existing = self.environments(profile)?;
            pending.retain(|e| {
                if existing.iter().any(|x| x.name == e.name) {
                    return false;
                }
                match existing.iter().find(|x| x.name == e.prior) {
                    Some(p) => {
                        envs += self.create_env(profile, &e.name, p.id, e.protected).is_ok() as usize;
                        false
                    }
                    None => true,
                }
            });
            if pending.len() == before {
                bail!("environment(s) with unknown prior: {}", pending.iter().map(|e| e.name.as_str()).collect::<Vec<_>>().join(", "));
            }
        }
        let mut cvs = 0;
        let existing = self.content_views(profile)?;
        for v in &ex.content_views {
            if existing.iter().any(|c| c.name == v.name) {
                continue;
            }
            let id = self.create_cv(profile, &v.name, &v.description)?;
            for r in &v.repos {
                self.set_cv_repo(id, r)?;
            }
            cvs += 1;
        }
        Ok((cvs, envs))
    }
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

fn version_row(r: &rusqlite::Row) -> rusqlite::Result<CvVersion> {
    Ok(CvVersion {
        id: r.get(0)?,
        cv_id: r.get(1)?,
        number: r.get(2)?,
        notes: r.get(3)?,
        state: r.get(4)?,
        created_at: r.get(5)?,
        finished_at: r.get(6)?,
        error: r.get(7)?,
    })
}

fn binding_row(r: &rusqlite::Row) -> rusqlite::Result<Binding> {
    Ok(Binding { env_id: r.get(0)?, cv_id: r.get(1)?, version_id: r.get(2)?, state: r.get(3)?, updated_at: r.get(4)? })
}

fn binding_repo_row(r: &rusqlite::Row) -> rusqlite::Result<BindingRepo> {
    Ok(BindingRepo {
        env_id: r.get(0)?,
        cv_id: r.get(1)?,
        repo_href: r.get(2)?,
        repo_name: r.get(3)?,
        repo_type: repo_type(r.get(4)?)?,
        distribution: r.get(5)?,
        base_path: r.get(6)?,
        publication: r.get(7)?,
    })
}

/// Names become part of Pulp object names and base paths.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
        bail!("name must be 1-64 characters of [A-Za-z0-9-_.]");
    }
    Ok(())
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

    #[test]
    fn migrations_are_idempotent_and_versioned() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("db.sqlite");
        Store::open(&p).unwrap().create_cv("lab", "base", "").unwrap();
        let s = Store::open(&p).unwrap();
        assert_eq!(s.content_views("lab").unwrap().len(), 1);
        s.db().pragma_update(None, "user_version", 99).unwrap();
        drop(s);
        assert!(Store::open(&p).is_err());
    }

    #[test]
    fn content_view_versions_and_bindings() {
        let s = Store::open_in_memory().unwrap();
        let cv = s.create_cv("lab", "base", "desc").unwrap();
        assert!(s.create_cv("lab", "base", "").is_err());
        assert!(s.create_cv("lab", "bad name", "").is_err());
        s.create_cv("other", "base", "").unwrap(); // profiles are isolated
        let mut r = repo("os", RepoType::Rpm);
        r.filters = parse_rules("exclude name=*-debuginfo", RepoType::Rpm).unwrap();
        s.set_cv_repo(cv, &r).unwrap();
        s.set_cv_repo(cv, &repo("docs", RepoType::File)).unwrap();
        assert_eq!(s.cv_repos(cv).unwrap().len(), 2);

        let v1 = s.begin_version(cv, "first", &s.cv_repos(cv).unwrap()).unwrap();
        let v2 = s.begin_version(cv, "second", &s.cv_repos(cv).unwrap()).unwrap();
        assert_eq!((v1.number, v2.number), (1, 2));
        let mut vrs = s.version_repos(v1.id).unwrap();
        assert_eq!(vrs[1].repo().filters.len(), 1);
        vrs[0].derived_version = Some("/dv/1/".into());
        s.save_version_repo(&vrs[0]).unwrap();
        s.finish_version(v1.id, "published", None).unwrap();
        assert_eq!(s.version(v1.id).unwrap().state, "published");
        assert_eq!(s.protected_versions("lab").unwrap(), vec!["/dv/1/"]);

        let lib = s.ensure_library("lab").unwrap();
        assert_eq!(s.ensure_library("lab").unwrap(), lib);
        let dev = s.create_env("lab", "Dev", lib, false).unwrap();
        let prod = s.create_env("lab", "Prod", dev, true).unwrap();
        let qa = s.create_env("lab", "QA", lib, false).unwrap();
        let order: Vec<_> = s.environments("lab").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(order, ["Library", "Dev", "Prod", "QA"]);
        s.set_binding(dev, cv, v1.id, "active").unwrap();
        assert_eq!(s.version_in_use(v1.id).unwrap(), vec!["Dev"]);
        s.set_binding(dev, cv, v2.id, "active").unwrap();
        assert!(s.version_in_use(v1.id).unwrap().is_empty());
        assert!(s.delete_env(dev).is_err()); // bound + has successor
        s.delete_env(qa).unwrap();
        assert!(s.environments("lab").unwrap().iter().all(|e| e.id != qa));
        let _ = prod;

        let ex = s.export("lab").unwrap();
        let s2 = Store::open_in_memory().unwrap();
        assert_eq!(s2.import("lab", &ex).unwrap(), (1, 2));
        assert_eq!(s2.import("lab", &ex).unwrap(), (0, 0));
        assert_eq!(s2.export("lab").unwrap(), ex);

        s.delete_cv(cv).unwrap();
        assert!(s.versions(cv).unwrap().is_empty());
    }

    #[test]
    fn tasks_roundtrip() {
        let s = Store::open_in_memory().unwrap();
        s.track_task("lab", "/t/1/", "sync os", "running").unwrap();
        s.track_task("lab", "/t/2/", "publish", "waiting").unwrap();
        s.finish_task("lab", "/t/1/", "completed").unwrap();
        let t = s.tasks("lab", 10).unwrap();
        assert_eq!(t[0].href, "/t/2/"); // unfinished first
        assert_eq!(t[1].state, "completed");
    }
}
