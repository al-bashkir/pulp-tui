//! The shared Content View database on a real PostgreSQL (tests/env/pulp-env.sh pg-up):
//! explicit schema creation, two operators sharing state and locks, and the store methods.
//! Each test works in its own PostgreSQL schema (`options=-csearch_path=...`), so reruns
//! against the same container never meet old rows. No Pulp server is needed.

mod common;

use common::*;
use futures::future::join_all;
use pulp_tui::config::Database;
use pulp_tui::cv::CvLock;
use pulp_tui::pulp::RepoType;
use pulp_tui::store::{BindingRepo, CvRepo, CvVersionRepo, PG_MIGRATIONS, Store};
use sqlx::{Connection, Executor};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Same default as tests/env/pulp-env.sh pg-up (a test-only value).
fn password() -> String {
    std::env::var("PULP_TUI_TEST_PG_PASSWORD").unwrap_or_else(|_| "pulp_tui_test".into())
}

/// A fresh PostgreSQL schema in the test database; `connect()` is the `[database]` URL that uses only it.
struct Schema {
    name: String,
    db: Database,
    admin: String,
}

impl Schema {
    async fn new() -> Self {
        let base = std::env::var("PULP_TUI_TEST_PG").unwrap_or_else(|_| "postgres://pulp_tui@127.0.0.1:55432/pulp_tui".into());
        // The password goes through a secret file, the same mechanism as a real `[database].secret`.
        let file = std::env::temp_dir().join(format!("pulp-tui-test-{}", std::process::id())).join("pg-secret");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, password()).unwrap();
        let secret = Some(format!("file:{}", file.display()));
        let admin = Database { url: base.clone(), secret: secret.clone(), ca_cert: None }.connect_url().unwrap();
        let name = unique("ptui").replace('-', "_");
        sqlx::any::install_default_drivers();
        let mut c = sqlx::AnyConnection::connect(&admin).await.expect("PostgreSQL is not reachable; run tests/env/pulp-env.sh pg-up");
        c.execute(sqlx::raw_sql(&format!("CREATE SCHEMA {name}"))).await.unwrap();
        let db = Database { url: format!("{base}?options=-csearch_path%3D{name}"), secret, ca_cert: None };
        Schema { name, db, admin }
    }

    fn connect(&self) -> String {
        self.db.connect_url().unwrap()
    }

    fn display(&self) -> String {
        self.db.display_url()
    }

    async fn migrate(&self) -> (i64, i64) {
        Store::migrate_external(&self.connect(), &self.display()).await.unwrap()
    }

    async fn open(&self) -> Arc<Store> {
        Arc::new(Store::open_external(&self.connect(), &self.display()).await.unwrap())
    }

    /// Pretend that another pulp-tui version migrated this schema.
    async fn set_version(&self, v: i64) {
        let mut c = sqlx::AnyConnection::connect(&self.connect()).await.unwrap();
        c.execute(sqlx::raw_sql(&format!("UPDATE schema_version SET version = {v}"))).await.unwrap();
    }

    /// Best-effort; a failed test leaves its schema behind (`pg-down` removes everything).
    async fn drop_schema(self) {
        if let Ok(mut c) = sqlx::AnyConnection::connect(&self.admin).await {
            let _ = c.execute(sqlx::raw_sql(&format!("DROP SCHEMA {} CASCADE", self.name))).await;
        }
    }
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

fn repo(name: &str, t: RepoType) -> CvRepo {
    CvRepo {
        repo_href: format!("/pulp/api/v3/repositories/{}/{name}/", t.component()),
        repo_name: name.into(),
        repo_type: t,
        filters: vec![],
    }
}

#[tokio::test]
async fn postgres_schema_lifecycle() {
    if !live_enabled() {
        return;
    }
    let s = Schema::new().await;
    let n = PG_MIGRATIONS.len() as i64;
    let e = format!("{:#}", Store::open_external(&s.connect(), &s.display()).await.unwrap_err());
    assert!(e.contains("database is not initialized; run pulp-tui db migrate"), "{e}");
    assert!(!e.contains(&password()), "the password must not appear in errors: {e}");

    assert_eq!(s.migrate().await, (0, n));
    assert_eq!(s.migrate().await, (n, n), "a second db migrate changes nothing");
    let store = s.open().await;
    assert!(store.external);
    assert_eq!(store.schema_version().await.unwrap(), n);
    let id = store.db_id().to_string();
    assert_eq!(id.len(), 16);
    assert_eq!(s.open().await.db_id(), id, "the database id is created once");

    // An older or newer schema is refused; only `db migrate` upgrades.
    s.set_version(n - 1).await;
    let e = Store::open_external(&s.connect(), &s.display()).await.unwrap_err().to_string();
    assert!(e.contains(&format!("database schema {} is older than this pulp-tui ({n}); run pulp-tui db migrate", n - 1)), "{e}");
    s.set_version(n + 1).await;
    let e = Store::open_external(&s.connect(), &s.display()).await.unwrap_err().to_string();
    assert!(e.contains(&format!("database schema {} is newer than this pulp-tui ({n}); upgrade pulp-tui", n + 1)), "{e}");
    s.set_version(n).await;
    assert_eq!(s.open().await.db_id(), id);
    s.drop_schema().await;
}

#[tokio::test]
async fn two_operators_share_state_and_locks() {
    if !live_enabled() {
        return;
    }
    let s = Schema::new().await;
    s.migrate().await;
    let (a, b) = (s.open().await, s.open().await);
    assert_eq!(a.db_id(), b.db_id());

    // State written by A is visible to B, with the actor of the audit row.
    let cv = a.create_cv("k", "web", "shared").await.unwrap();
    let seen = b.content_views("k").await.unwrap();
    assert_eq!((seen[0].id, seen[0].name.as_str()), (cv, "web"));
    a.audit("k", "alice@ws1", "cv.create", "web").await.unwrap();
    assert_eq!(b.audit_log("k", 1).await.unwrap()[0].actor, "alice@ws1");

    // One lock per Content View: B is refused while A holds it, and gets it after A lets go.
    let lock_a = CvLock::acquire(a.clone(), cv, "web", "alice@ws1", "publish").await.unwrap();
    let e = CvLock::acquire(b.clone(), cv, "web", "bob@ws2", "promote to Prod").await.unwrap_err().to_string();
    assert!(e.starts_with("Content View web is locked by alice@ws1 (publish) since "), "{e}");
    lock_a.check().unwrap();
    drop(lock_a);
    // The release runs in a background task: wait for it.
    for _ in 0..50 {
        if b.lock_holder(cv).await.unwrap().is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let lock_b = CvLock::acquire(b.clone(), cv, "web", "bob@ws2", "promote to Prod").await.unwrap();
    let e = CvLock::acquire(a.clone(), cv, "web", "alice@ws1", "publish").await.unwrap_err().to_string();
    assert!(e.contains("locked by bob@ws2 (promote to Prod)"), "{e}");
    let shown = a.lock_holder(cv).await.unwrap().unwrap();
    assert_eq!((shown.owner.as_str(), shown.operation.as_str()), ("bob@ws2", "promote to Prod"));
    assert!(a.break_lock(cv, &shown).await.unwrap());
    assert!(!a.break_lock(cv, &shown).await.unwrap(), "already broken");
    drop(lock_b);

    // Eight simultaneous attempts from both connections: PostgreSQL lets exactly one win.
    let other = a.create_cv("k", "api", "").await.unwrap();
    let now = unix_now();
    let tries = (0..8).map(|i| {
        let st = if i % 2 == 0 { a.clone() } else { b.clone() };
        async move { st.try_lock(other, &format!("op{i}@h"), &format!("t{i}"), "publish", now).await.unwrap().is_ok() }
    });
    assert_eq!(join_all(tries).await.into_iter().filter(|won| *won).count(), 1);
    s.drop_schema().await;
}

#[tokio::test]
async fn store_methods_on_postgres() {
    if !live_enabled() {
        return;
    }
    let s = Schema::new().await;
    s.migrate().await;
    let st = s.open().await;
    let k = "https://pulp.example.com/pulp/#default";

    // Definitions, versions and checkpoints.
    let cv = st.create_cv(k, "base", "desc").await.unwrap();
    assert!(st.create_cv(k, "base", "").await.is_err(), "names are unique per server");
    assert!(st.create_cv(k, "bad name", "").await.is_err());
    st.create_cv("https://other/pulp/#default", "base", "").await.unwrap(); // another server: no clash
    let (os, docs) = (repo("os", RepoType::Rpm), repo("docs", RepoType::File));
    for r in [&os, &docs] {
        st.set_cv_repo(cv, r).await.unwrap();
    }
    st.set_cv_repo(cv, &os).await.unwrap(); // upsert
    assert_eq!(st.cv_repos(cv).await.unwrap().len(), 2);
    let v1 = st.begin_version(cv, "first", &[os.clone(), docs.clone()]).await.unwrap();
    let v2 = st.begin_version(cv, "second", std::slice::from_ref(&os)).await.unwrap();
    assert_eq!((v1.number, v2.number), (1, 2));
    let vr = CvVersionRepo {
        version_id: v1.id,
        repo: Some(os.clone()),
        source_version: Some("/rv/1/".into()),
        derived_repo: Some("/dr/".into()),
        derived_version: Some("/dr/versions/1/".into()),
        publication: Some("/pub/1/".into()),
        pending_task: Some("/t/9/".into()),
        content_count: Some(7),
    };
    st.save_version_repo(&vr).await.unwrap();
    assert_eq!(st.version_repos(v1.id).await.unwrap().iter().find(|r| r.repo().repo_name == "os"), Some(&vr));
    st.finish_version(v1.id, "published", None).await.unwrap();
    st.fail_publishing_version(v2.id, "boom").await.unwrap();
    st.fail_publishing_version(v1.id, "lost").await.unwrap(); // not publishing: unchanged
    assert_eq!(st.version(v1.id).await.unwrap().state, "published");
    assert_eq!(st.version(v2.id).await.unwrap().error.as_deref(), Some("boom"));
    assert_eq!(st.protected_versions(k).await.unwrap(), vec!["/dr/versions/1/"]);

    // Environments, bindings and their repositories.
    let lib = st.ensure_library(k).await.unwrap();
    assert_eq!(st.ensure_library(k).await.unwrap(), lib);
    let dev = st.create_env(k, "Dev", lib, false).await.unwrap();
    let prod = st.create_env(k, "Prod", dev, true).await.unwrap();
    let qa = st.create_env(k, "QA", lib, false).await.unwrap();
    let names: Vec<_> = st.environments(k).await.unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["Library", "Dev", "Prod", "QA"]);
    st.set_binding(dev, cv, v1.id, "active").await.unwrap();
    st.set_binding(prod, cv, v1.id, "promoting").await.unwrap();
    st.finish_promoting_binding(prod, cv, v1.id, "partial").await.unwrap();
    st.finish_promoting_binding(dev, cv, v1.id, "failed").await.unwrap(); // not promoting: unchanged
    assert_eq!(st.binding(dev, cv).await.unwrap().unwrap().state, "active");
    let mut in_use = st.version_in_use(v1.id).await.unwrap();
    in_use.sort();
    assert_eq!(in_use, ["Dev", "Prod"]);
    let br = BindingRepo {
        env_id: dev,
        cv_id: cv,
        repo_href: os.repo_href.clone(),
        repo_name: "os".into(),
        repo_type: RepoType::Rpm,
        distribution: "/d/1/".into(),
        base_path: "cv/Dev/base/os".into(),
        publication: Some("/pub/1/".into()),
    };
    st.save_binding_repo(&br).await.unwrap();
    st.save_binding_repo(&BindingRepo { publication: None, ..br.clone() }).await.unwrap(); // upsert
    assert_eq!(st.binding_repos(dev, cv).await.unwrap(), vec![BindingRepo { publication: None, ..br }]);
    assert_eq!(st.binding(prod, cv).await.unwrap().unwrap().state, "partial");
    assert_eq!(st.bindings_for_cv(cv).await.unwrap().len(), 2);
    assert!(st.delete_env(dev).await.is_err(), "bound and has a successor");
    st.delete_env(qa).await.unwrap();

    // Audit trail, replication runs, task references, the first base path prefix.
    st.audit(k, "ann@ws1", "cv.promote", "base v1 -> Dev").await.unwrap();
    st.audit(k, "bob@ws2", "cv.publish", "base v2").await.unwrap();
    let log = st.audit_log(k, 5).await.unwrap();
    assert_eq!(
        log.iter().map(|a| (a.action.as_str(), a.actor.as_str())).collect::<Vec<_>>(),
        [("cv.publish", "bob@ws2"), ("cv.promote", "ann@ws1")]
    );
    st.record_replication(k, "src", "cv/os", "completed", "").await.unwrap();
    assert_eq!(st.last_replications(k, "src").await.unwrap().len(), 1);
    st.track_task(k, "/t/1/", "sync os", "running").await.unwrap();
    st.finish_task(k, "/t/1/", "completed").await.unwrap();
    assert_eq!(st.tasks(k, 5).await.unwrap()[0].state, "completed");
    assert_eq!(st.cv_prefix(k, "cv").await.unwrap(), "cv");
    assert_eq!(st.cv_prefix(k, "other").await.unwrap(), "cv", "the first prefix of a server wins");

    // Export and import between two servers' scopes of the same database.
    let ex = st.export(k).await.unwrap();
    assert_eq!(st.import("https://copy/pulp/#default", &ex).await.unwrap(), (1, 2));
    assert_eq!(st.import("https://copy/pulp/#default", &ex).await.unwrap(), (0, 0));

    // db copy from a local (SQLite) store: new ids, history kept, aliases recorded.
    let local = Store::open_in_memory().await.unwrap();
    let llib = local.ensure_library("lab").await.unwrap();
    let ldev = local.create_env("lab", "Dev", llib, true).await.unwrap();
    let lcv = local.create_cv("lab", "web", "from the file").await.unwrap();
    let lrepo = repo("tools", RepoType::File);
    local.set_cv_repo(lcv, &lrepo).await.unwrap();
    let lv = local.begin_version(lcv, "n", std::slice::from_ref(&lrepo)).await.unwrap();
    local.finish_version(lv.id, "published", None).await.unwrap();
    local.set_binding(ldev, lcv, lv.id, "active").await.unwrap();
    local.audit("lab", "", "cv.promote", "web v1 -> Dev").await.unwrap();
    let k2 = "https://copied/pulp/#default";
    let r = st.copy_from(&local, "lab", k2).await.unwrap();
    assert_eq!((r.views, r.versions, r.environments, r.bindings, r.audit), (1, 1, 1, 1, 1));
    let copied = st.content_views(k2).await.unwrap().remove(0);
    assert_eq!((copied.name.as_str(), copied.description.as_str()), ("web", "from the file"));
    let dev2 = st.environments(k2).await.unwrap().into_iter().find(|e| e.name == "Dev").unwrap();
    assert!(dev2.protected);
    let b = st.binding(dev2.id, copied.id).await.unwrap().unwrap();
    assert_eq!(st.version(b.version_id).await.unwrap().number, 1);
    assert_eq!(st.audit_log(k2, 5).await.unwrap()[0].actor, "copied");
    assert_eq!(st.aliases().await.unwrap(), vec![local.db_id().to_string()]);
    let e = st.copy_from(&local, "lab", k2).await.unwrap_err().to_string();
    assert!(e.contains("never merges"), "{e}");

    // Deleting a Content View removes everything that hangs on it, including a lock still held.
    let lock = CvLock::acquire(st.clone(), cv, "base", "ann@ws1", "delete").await.unwrap();
    st.delete_cv(cv).await.unwrap();
    assert!(st.versions(cv).await.unwrap().is_empty());
    assert!(st.cv_repos(cv).await.unwrap().is_empty());
    assert!(st.lock_holder(cv).await.unwrap().is_none());
    drop(lock);
    s.drop_schema().await;
}
