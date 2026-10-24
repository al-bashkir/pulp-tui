//! Application state and event handling. Rendering never waits on I/O: every network or
//! disk operation runs as a background job that sends back a closure applied here.

use super::actions::ap;
use super::form::{FieldKind, Form, FormEvent, PickItem, Source};
use super::kinds::{self, Act, Kind};
use super::tasks::TaskCenter;
use crate::config::{Config, Database, Settings};
use crate::cv::CvService;
use crate::pulp::models::Task;
use crate::pulp::{Capabilities, PulpClient};
use crate::store::{NotMigrated, Store};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// State change produced by a background job, applied on the UI thread.
pub type Apply = Box<dyn FnOnce(&mut App) + Send>;
pub type Submit = Box<dyn FnMut(&mut App, &Form) -> Result<(), String>>;
pub type OnYes = Box<dyn FnOnce(&mut App)>;
pub type OnInput = Box<dyn FnOnce(&mut App, String)>;
pub type PaletteEntry = (String, OnYes);

pub enum Msg {
    JobDone {
        id: u64,
        result: Result<Apply, String>,
    },
    JobProgress {
        id: u64,
        text: String,
    },
    /// Task snapshot from a workflow that polls the task itself.
    Task(Task),
    TaskPolled {
        href: String,
        result: Result<Task, String>,
    },
}

pub struct Conn {
    pub client: PulpClient,
    pub caps: Arc<Capabilities>,
    /// `Store::scope` of the profile the client was built from (a later config edit needs a reconnect).
    pub scope: String,
}

pub const NO_PROFILE: &str = "select a server profile first (Content Views belong to a server)";

pub struct Job {
    pub id: u64,
    pub label: String,
    pub progress: String,
    pub started: Instant,
    abort: tokio::task::AbortHandle,
    /// Loads are cancelled silently by Esc; operations are reported.
    pub is_load: bool,
}

/// Everything a background job may use. Cheap to clone.
#[derive(Clone)]
pub struct Ctx {
    pub client: PulpClient,
    pub caps: Arc<Capabilities>,
    /// The local file: task references and replication runs.
    pub store: Arc<Store>,
    /// Content View / lifecycle store, or why the shared database is unavailable.
    pub cv_store: Result<Arc<Store>, String>,
    /// `Store::scope` of the connected server.
    pub scope: String,
    /// Operator name for locks and the audit log.
    pub actor: String,
    pub settings: Settings,
    pub job: u64,
    tx: UnboundedSender<Msg>,
}

impl Ctx {
    pub fn progress(&self, text: impl Into<String>) {
        let _ = self.tx.send(Msg::JobProgress { id: self.job, text: text.into() });
    }
    pub fn on_task(&self) -> impl Fn(&Task) + Send + Sync + use<> {
        let tx = self.tx.clone();
        move |t: &Task| {
            let _ = tx.send(Msg::Task(t.clone()));
        }
    }
    pub fn log(&self) -> impl Fn(String) + Send + Sync + use<> {
        let (tx, id) = (self.tx.clone(), self.job);
        move |s: String| {
            tracing::info!(job = id, "{s}");
            let _ = tx.send(Msg::JobProgress { id, text: s });
        }
    }
    pub fn cv(&self) -> anyhow::Result<CvService> {
        Ok(CvService {
            client: self.client.clone(),
            store: self.cv_store.clone().map_err(unavailable)?,
            prefix: self.settings.cv_base_path_prefix.clone(),
            scope: self.scope.clone(),
            actor: self.actor.clone(),
            lock: None,
        })
    }
}

/// What Content View screens and actions report while the shared database is unavailable.
pub fn unavailable(e: String) -> anyhow::Error {
    anyhow::anyhow!("shared database unavailable: {e}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Ok,
    Warn,
    Error,
}

impl Level {
    /// The word in `[tag]` before a notice text, in the footer and in the Logs screen.
    pub fn tag(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Ok => "ok",
            Level::Warn => "warning",
            Level::Error => "error",
        }
    }
}

pub struct Notice {
    pub level: Level,
    pub text: String,
    pub at: Instant,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Screen {
    Dashboard,
    Help,
    List(Kind),
}

pub struct ListState {
    pub kind: Kind,
    pub items: Vec<Value>,
    pub count: u64,
    pub offset: u64,
    pub selected: usize,
    pub filter: String,
    pub sort: Option<(usize, bool)>,
    pub loading: bool,
    pub error: Option<String>,
    pub generation: u64,
    pub marked: BTreeSet<String>,
}

impl ListState {
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            items: vec![],
            count: 0,
            offset: 0,
            selected: 0,
            filter: String::new(),
            sort: None,
            loading: false,
            error: None,
            generation: 0,
            marked: BTreeSet::new(),
        }
    }
    pub fn current(&self) -> Option<&Value> {
        self.items.get(self.selected)
    }
}

pub fn item_key(v: &Value) -> String {
    v.get("pulp_href").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| v.get("id").map(|x| x.to_string()).unwrap_or_default())
}

pub enum Modal {
    Menu { title: String, items: Vec<Act>, selected: usize, item: Value },
    Form { form: Form, submit: Submit },
    Confirm { title: String, lines: Vec<String>, typed: Option<String>, input: String, on_yes: Option<OnYes>, danger: bool, scroll: usize },
    Text { title: String, lines: Vec<String>, scroll: usize },
    Input { title: String, input: String, on_done: Option<OnInput> },
    Palette { input: String, selected: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Nav,
    List,
}

pub const PAGE: u64 = 100;

pub struct App {
    pub cfg: Config,
    pub cfg_path: PathBuf,
    pub store: Arc<Store>,
    /// The CV store of the active profile: `store` without a database, else its external one, or why that is not open.
    pub cv_store: Result<Arc<Store>, String>,
    /// Open external stores by connect URL. A key holds the password: never show or log it.
    pub stores: Arc<std::sync::Mutex<HashMap<String, Arc<Store>>>>,
    /// The local file `<data_dir>/pulp-tui.db` (set by main; empty in tests).
    pub local_db: PathBuf,
    pub profile: Option<String>,
    pub conn: Option<Conn>,
    pub connecting: bool,
    pub screen: Screen,
    pub nav: usize,
    pub focus: Focus,
    pub list: ListState,
    pub history: Vec<(Screen, ListState)>,
    pub modal: Option<Modal>,
    pub notices: VecDeque<Notice>,
    pub jobs: Vec<Job>,
    pub tasks: TaskCenter,
    pub dashboard: Vec<String>,
    /// Last repository visited, used by the Repository Versions screen.
    pub last_repo: Option<(crate::pulp::RepoType, String, String)>,
    pub quit: bool,
    pub tx: UnboundedSender<Msg>,
    pub rx: Option<UnboundedReceiver<Msg>>,
    /// Newest session-log row the Logs screen has loaded.
    pub logs_seen: u64,
    next_id: u64,
}

impl App {
    pub fn new(cfg: Config, cfg_path: PathBuf, store: Arc<Store>, cv_store: Result<Arc<Store>, String>) -> Self {
        let (tx, rx) = unbounded_channel();
        let s = &cfg.settings;
        let tasks = TaskCenter::new(Duration::from_millis(s.poll_min_ms.max(100)), Duration::from_millis(s.poll_max_ms.max(s.poll_min_ms)));
        Self {
            cfg,
            cfg_path,
            store,
            cv_store,
            stores: Default::default(),
            local_db: PathBuf::new(),
            profile: None,
            conn: None,
            connecting: false,
            screen: Screen::Dashboard,
            nav: 0,
            focus: Focus::List,
            list: ListState::new(Kind::Profiles),
            history: vec![],
            modal: None,
            notices: VecDeque::new(),
            jobs: vec![],
            tasks,
            dashboard: vec![],
            last_repo: None,
            quit: false,
            tx,
            rx: Some(rx),
            logs_seen: 0,
            next_id: 1,
        }
    }

    // ---- notifications -------------------------------------------------------------

    pub fn notify(&mut self, level: Level, text: impl Into<String>) {
        let text = text.into();
        match level {
            Level::Error => tracing::warn!("{text}"),
            _ => tracing::info!("{text}"),
        }
        let server = self.profile.as_deref().unwrap_or_default();
        crate::session_log::record("notice", server, format!("[{}] {text}", level.tag()), vec![text.clone()]);
        self.notices.push_back(Notice { level, text, at: Instant::now() });
        while self.notices.len() > 200 {
            self.notices.pop_front();
        }
    }
    pub fn ok(&mut self, t: impl Into<String>) {
        self.notify(Level::Ok, t)
    }
    pub fn error(&mut self, t: impl Into<String>) {
        self.notify(Level::Error, t)
    }

    // ---- jobs ----------------------------------------------------------------------

    pub fn ctx(&self) -> Option<Ctx> {
        let c = self.conn.as_ref()?;
        Some(Ctx {
            client: c.client.clone(),
            caps: c.caps.clone(),
            store: self.store.clone(),
            cv_store: self.cv_store.clone(),
            scope: c.scope.clone(),
            actor: crate::store::actor(c.caps.user.as_ref().map(|u| u.username.as_str())),
            settings: self.cfg.settings.clone(),
            job: 0,
            tx: self.tx.clone(),
        })
    }

    /// Content View store scope of a profile: the connection's when it is the connected one,
    /// else from the config. An external store never falls back to the bare profile name.
    pub fn scope(&self, name: &str) -> Result<String, String> {
        let active = self.profile.as_deref() == Some(name);
        if let (Some(c), true) = (&self.conn, active) {
            return Ok(c.scope.clone());
        }
        let external = match &self.cv_store {
            Ok(st) if active => st.external,
            _ => self.cfg.database_for(name).is_some(),
        };
        match self.cfg.profiles.get(name) {
            Some(p) if external => Ok(p.server_key()),
            None if external => Err(NO_PROFILE.into()),
            _ => Ok(name.to_string()), // the local file
        }
    }

    /// The active profile uses an external database (other operators may share it): the store in use, as in
    /// `scope`, not the edited config (an Edit applies at the next Connect).
    pub fn shared(&self) -> bool {
        match (&self.cv_store, self.profile.as_deref()) {
            (Ok(st), Some(_)) => st.external,
            (_, Some(p)) => self.cfg.database_for(p).is_some(),
            _ => false,
        }
    }

    /// The CV store of a database (`None`: the local file). Profiles with one connect URL share one store;
    /// a failed open is not cached.
    pub fn open_store(&self, db: Option<(Database, String)>) -> impl Future<Output = anyhow::Result<Arc<Store>>> + Send + 'static {
        let (local, stores) = (self.store.clone(), self.stores.clone());
        async move {
            let Some((d, user)) = db else { return Ok(local) };
            let url = d.connect_url(&user)?;
            let hit = stores.lock().unwrap().get(&url).cloned();
            if let Some(st) = hit {
                return Ok(st);
            }
            let st = Arc::new(Store::open_external(&url, &d.display_url()).await?);
            Ok(stores.lock().unwrap().entry(url).or_insert(st).clone())
        }
    }

    /// The CV store and scope of any profile, also one that is not connected.
    pub fn profile_store(&self, name: &str) -> impl Future<Output = anyhow::Result<(Arc<Store>, String)>> + Send + 'static {
        let active = self.cv_store.as_ref().ok().filter(|_| self.profile.as_deref() == Some(name)).cloned();
        let (open, scope) = (self.open_store(self.cfg.database_for(name)), self.scope(name));
        async move {
            let scope = scope.map_err(anyhow::Error::msg)?;
            let st = match active {
                Some(st) => st,
                None => open.await?,
            };
            Ok((st, scope))
        }
    }

    /// Open the store of `db` in a job, then call `done`. A database that is not migrated gives `done(Err)`,
    /// then the offer to migrate it (not for a read-only profile, not over an open modal).
    pub fn with_store(
        &mut self,
        name: &str,
        read_only: bool,
        db: Option<(Database, String)>,
        done: impl Fn(&mut App, Result<Arc<Store>, String>) + Send + Sync + 'static,
    ) {
        let done = Arc::new(done);
        let Some((d, user)) = db else {
            let local = self.store.clone();
            return done(self, Ok(local));
        };
        let (name, display, open) = (name.to_string(), d.display_url(), self.open_store(Some((d.clone(), user.clone()))));
        self.spawn(format!("open database {display}"), true, async move {
            let r = open.await;
            ap(move |app| {
                let e = match r {
                    Ok(st) => return done(app, Ok(st)),
                    Err(e) => e,
                };
                let Some(nm) = e.downcast_ref::<NotMigrated>() else { return done(app, Err(format!("{e:#}"))) };
                let how = format!("run pulp-tui db migrate --profile {name}");
                done(app, Err(format!("{display}: database not migrated (open Servers, Connect again to migrate, or {how})")));
                if read_only && Act::MigrateDb.mutates(&Kind::Profiles, true) {
                    app.notify(Level::Warn, format!("profile {name} is read-only, so {display} is not migrated ({how})"));
                    return;
                }
                if app.modal.is_some() {
                    return; // the CV screens say how to migrate
                }
                let from = if nm.from == 0 { "empty".to_string() } else { nm.from.to_string() };
                let lines = vec![
                    format!("Database: {display}"),
                    format!("Schema: {from} -> {}", nm.to),
                    format!("Other operators using this database need pulp-tui {} or newer after this.", env!("CARGO_PKG_VERSION")),
                    "Esc: do not migrate now.".into(),
                ];
                app.confirm(format!("Migrate database of {name}?"), lines, move |app| {
                    let open = app.open_store(Some((d.clone(), user.clone())));
                    app.spawn(format!("migrate {display}"), false, async move {
                        let (a, b) = Store::migrate_external(&d.connect_url(&user)?, &display).await?;
                        let st = open.await?;
                        ap(move |app| {
                            app.ok(format!("{display}: schema {a} -> {b}"));
                            done(app, Ok(st));
                        })
                    });
                });
            })
        });
    }

    /// Run `f` in the background against the active server.
    pub fn job<F, Fut>(&mut self, label: impl Into<String>, f: F)
    where
        F: FnOnce(Ctx) -> Fut,
        Fut: Future<Output = anyhow::Result<Apply>> + Send + 'static,
    {
        let Some(mut ctx) = self.ctx() else {
            self.error("not connected to a Pulp server (open Servers and select a profile)");
            return;
        };
        ctx.job = self.next_id;
        self.spawn(label.into(), false, f(ctx));
    }

    /// Run a job that needs no server connection (profile tests, keyring, files).
    pub fn spawn<Fut>(&mut self, label: String, is_load: bool, fut: Fut) -> u64
    where
        Fut: Future<Output = anyhow::Result<Apply>> + Send + 'static,
    {
        let id = self.next_id;
        self.next_id += 1;
        let tx = self.tx.clone();
        let handle = tokio::spawn(async move {
            let result = fut.await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::JobDone { id, result });
        });
        self.jobs.push(Job { id, label, progress: String::new(), started: Instant::now(), abort: handle.abort_handle(), is_load });
        id
    }

    /// Cancel the client side only: an already-submitted Pulp task keeps running.
    pub fn abort_job(&mut self, id: u64) {
        if let Some(i) = self.jobs.iter().position(|j| j.id == id) {
            let j = self.jobs.remove(i);
            j.abort.abort();
            if !j.is_load {
                self.notify(Level::Warn, format!("stopped waiting for '{}' (server-side tasks continue; see Tasks)", j.label));
            }
        }
    }

    fn abort_all(&mut self) {
        for j in self.jobs.drain(..) {
            j.abort.abort();
        }
    }

    pub fn handle_msg(&mut self, m: Msg) {
        match m {
            Msg::JobDone { id, result } => {
                let Some(i) = self.jobs.iter().position(|j| j.id == id) else { return }; // aborted
                let job = self.jobs.remove(i);
                match result {
                    Ok(apply) => apply(self),
                    Err(e) => self.error(format!("{}: {e}", job.label)),
                }
            }
            Msg::JobProgress { id, text } => {
                if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
                    j.progress = text;
                }
            }
            Msg::Task(t) => self.task_update(t, true),
            Msg::TaskPolled { href, result } => match result {
                Ok(t) => self.task_update(t, false),
                Err(e) => self.tasks.poll_failed(&href, e),
            },
        }
        if self.list.kind == Kind::Tasks && self.screen == Screen::List(Kind::Tasks) {
            self.list.items = self.task_items();
            self.list.count = self.list.items.len() as u64;
        }
    }

    /// Task Center rows from the in-memory tracker (no I/O).
    pub fn task_items(&self) -> Vec<Value> {
        self.tasks
            .items
            .iter()
            .map(|t| {
                serde_json::json!({
                    "pulp_href": t.href, "label": t.label, "state": t.state, "progress": t.progress(),
                    "started": t.task.as_ref().and_then(|x| x.started_at.clone()).unwrap_or_default(),
                    "error": t.error, "task": t.task,
                })
            })
            .collect()
    }

    // ---- tasks ---------------------------------------------------------------------

    /// Remember a submitted Pulp task (persisted so it is re-attached after a restart).
    pub fn track_task(&mut self, href: &str, label: &str) {
        self.tasks.track(href, label, "waiting");
        if let Some(p) = self.profile.clone() {
            let (st, href, label) = (self.store.clone(), href.to_string(), label.to_string());
            self.spawn("persist task reference".into(), true, async move {
                st.track_task(&p, &href, &label, "waiting").await?;
                ap(|_| {})
            });
        }
    }

    fn task_update(&mut self, t: Task, driven: bool) {
        let href = t.pulp_href.clone();
        let state = t.state.clone();
        let err = t.error_message();
        if self.tasks.update(t, driven) {
            let label = self.tasks.items.iter().find(|x| x.href == href).map(|x| x.label.clone()).unwrap_or_default();
            if let Some(p) = self.profile.clone() {
                let (st, href, state) = (self.store.clone(), href.clone(), state.clone());
                self.spawn("persist task state".into(), true, async move {
                    st.finish_task(&p, &href, &state).await?;
                    ap(|_| {})
                });
            }
            match state.as_str() {
                "completed" => self.ok(format!("task completed: {label}")),
                _ => self.error(format!("task {state}: {label}{}", err.map(|e| format!(": {e}")).unwrap_or_default())),
            }
            // Refresh the visible list once when a task finishes (not on every tick).
            if matches!(self.screen, Screen::List(_)) && !self.list.loading {
                self.reload();
            }
        }
    }

    /// Called every UI tick: schedule due task polls.
    pub fn tick(&mut self) {
        // Local screen: it must follow the log with or without a connection.
        let last = crate::session_log::last_id();
        if matches!(self.screen, Screen::List(Kind::Logs { .. })) && !self.list.loading && last != self.logs_seen {
            self.logs_seen = last;
            self.reload();
        }
        let Some(ctx) = self.ctx() else { return };
        for href in self.tasks.due() {
            let (c, tx) = (ctx.client.clone(), self.tx.clone());
            tokio::spawn(async move {
                let result = c.get::<Task>(&href, &[]).await.map_err(|e| e.to_string());
                let _ = tx.send(Msg::TaskPolled { href, result });
            });
        }
        let now = Instant::now();
        while self.notices.front().is_some_and(|n| now.duration_since(n.at) > Duration::from_secs(600)) {
            self.notices.pop_front();
        }
    }

    // ---- connection ----------------------------------------------------------------

    pub fn connect(&mut self, name: &str) {
        let Some(p) = self.cfg.profiles.get(name).cloned() else {
            self.error(format!("unknown profile {name}"));
            return;
        };
        self.abort_all();
        self.conn = None;
        self.profile = Some(name.to_string());
        self.connecting = true;
        self.tasks = TaskCenter::new(
            Duration::from_millis(self.cfg.settings.poll_min_ms.max(100)),
            Duration::from_millis(self.cfg.settings.poll_max_ms),
        );
        self.dashboard.clear();
        self.history.clear();
        let db = self.cfg.database_for(name);
        if let Some((d, _)) = &db {
            self.cv_store = Err(format!("{}: opening", d.display_url())); // never the previous profile's store
        }
        let n = name.to_string();
        self.with_store(name, p.read_only, db, move |app, r| {
            if app.profile.as_deref() != Some(n.as_str()) {
                return; // switched again meanwhile
            }
            app.cv_store = r;
            // Also on an error: the screen then shows it instead of "opening".
            if matches!(app.screen, Screen::Dashboard | Screen::List(Kind::ContentViews | Kind::CvVersions { .. } | Kind::Environments)) {
                app.reload();
            }
        });
        // After the store is set: `scope` reads it. Always Ok, the profile exists.
        let scope = self.scope(name).unwrap_or_default();
        let (settings, name) = (self.cfg.settings.clone(), name.to_string());
        let fut = async move {
            let client = PulpClient::new(&name, &p, &settings)?;
            let caps = Capabilities::detect(&client).await?;
            let apply: Apply = Box::new(move |app: &mut App| {
                if app.profile.as_deref() != Some(client.profile()) {
                    return; // switched again meanwhile
                }
                app.connecting = false;
                let user = caps.user.as_ref().map(|u| u.username.clone()).unwrap_or_else(|| "anonymous".into());
                app.ok(format!("connected to {} as {user}", client.profile()));
                if let Some(e) = &caps.openapi_error {
                    app.notify(Level::Warn, format!("OpenAPI schema unavailable ({e}); capability checks are limited"));
                }
                app.conn = Some(Conn { client, caps: Arc::new(caps), scope });
                app.reattach_tasks();
                app.open(Screen::Dashboard);
            });
            anyhow::Ok(apply)
        };
        let label = format!("connect {}", self.profile.as_deref().unwrap_or_default());
        let tx = self.tx.clone();
        let id = self.next_id;
        self.next_id += 1;
        let handle = tokio::spawn(async move {
            let result = fut.await.map_err(|e| format!("{e:#}"));
            let result = result.or_else(|e| {
                Ok::<Apply, String>(Box::new(move |app: &mut App| {
                    app.connecting = false;
                    app.error(format!("connection failed: {e}"));
                }))
            });
            let _ = tx.send(Msg::JobDone { id, result });
        });
        self.jobs.push(Job { id, label, progress: String::new(), started: Instant::now(), abort: handle.abort_handle(), is_load: true });
    }

    fn reattach_tasks(&mut self) {
        let Some(p) = self.profile.clone() else { return };
        let st = self.store.clone();
        // Not named "load ...": list reloads cancel those.
        self.spawn("reattach tasks".into(), true, async move {
            let refs = st.tasks(&p, 50).await?;
            ap(move |app| {
                let mut n = 0;
                for r in refs.iter().rev() {
                    app.tasks.track(&r.href, &r.label, &r.state);
                    n += (!crate::pulp::models::TASK_FINAL_STATES.contains(&r.state.as_str())) as usize;
                }
                if n > 0 {
                    app.notify(Level::Info, format!("re-attached to {n} unfinished task(s) from a previous session"));
                }
            })
        });
    }

    // ---- navigation ----------------------------------------------------------------

    pub fn open(&mut self, s: Screen) {
        if let Screen::List(k) = &s {
            if let Some(i) = kinds::NAV.iter().position(|(_, n)| n == k) {
                self.nav = i + 1;
            }
            self.list = ListState::new(k.clone());
        } else if s == Screen::Dashboard {
            self.nav = 0;
        }
        self.screen = s;
        self.reload();
    }

    /// Open a sub-list (e.g. versions of a repository); Esc returns.
    pub fn push(&mut self, k: Kind) {
        let prev = std::mem::replace(&mut self.list, ListState::new(k.clone()));
        self.history.push((self.screen.clone(), prev));
        self.screen = Screen::List(k);
        self.reload();
    }

    pub fn back(&mut self) {
        if let Some((s, l)) = self.history.pop() {
            self.screen = s;
            self.list = l;
            self.reload();
        }
    }

    pub fn reload(&mut self) {
        match self.screen.clone() {
            Screen::Dashboard => super::actions::load_dashboard(self),
            Screen::Help => {}
            Screen::List(Kind::Tasks) => {
                self.list.items = self.task_items();
                self.list.count = self.list.items.len() as u64;
                self.list.loading = false;
                self.list.error = None;
                self.list.selected = self.list.selected.min(self.list.items.len().saturating_sub(1));
            }
            Screen::List(k) => {
                // Cancel a still-running load of the same list; a newer generation wins.
                for id in self.jobs.iter().filter(|j| j.is_load && j.label.starts_with("load ")).map(|j| j.id).collect::<Vec<_>>() {
                    self.abort_job(id);
                }
                self.list.generation += 1;
                self.list.loading = true;
                self.list.error = None;
                let generation = self.list.generation;
                let (offset, filter) = (self.list.offset, self.list.filter.clone());
                let profile = self.profile.clone().unwrap_or_default();
                let (cv, scope) = (self.cv_store.clone(), self.scope(&profile));
                let cfg = self.cfg.clone();
                let needs_conn = k.needs_connection();
                let ctx = self.ctx();
                if needs_conn && ctx.is_none() {
                    self.list.loading = false;
                    self.list.error = Some(if self.connecting {
                        "connecting...".into()
                    } else {
                        "not connected: select a server profile in Servers".into()
                    });
                    return;
                }
                if let Some(f) = ctx.as_ref().and_then(|c| k.unsupported(&c.caps)) {
                    self.list.loading = false;
                    self.list.error = Some(f);
                    return;
                }
                let scope = match scope {
                    Ok(s) => s,
                    Err(e) if matches!(k, Kind::ContentViews | Kind::CvVersions { .. } | Kind::Environments) => {
                        self.list.loading = false;
                        self.list.error = Some(e);
                        return;
                    }
                    Err(_) => String::new(), // other screens never read the CV store
                };
                let fut = async move {
                    let cv = cv.as_deref().map_err(String::as_str);
                    let (items, count) = kinds::load(&k, ctx.as_ref(), cv, &scope, &profile, &cfg, offset, &filter).await?;
                    let apply: Apply = Box::new(move |app: &mut App| {
                        if app.list.generation != generation {
                            return; // stale response
                        }
                        app.list.loading = false;
                        app.set_items(items, count);
                    });
                    anyhow::Ok(apply)
                };
                let tx = self.tx.clone();
                let id = self.next_id;
                self.next_id += 1;
                let handle = tokio::spawn(async move {
                    let result = fut.await.map_err(|e| format!("{e:#}")).or_else(|e| {
                        Ok::<Apply, String>(Box::new(move |app: &mut App| {
                            if app.list.generation == generation {
                                app.list.loading = false;
                                app.list.error = Some(e);
                            }
                        }))
                    });
                    let _ = tx.send(Msg::JobDone { id, result });
                });
                self.jobs.push(Job {
                    id,
                    label: "load list".into(),
                    progress: String::new(),
                    started: Instant::now(),
                    abort: handle.abort_handle(),
                    is_load: true,
                });
            }
        }
    }

    /// Replace the rows of the list. A cursor below the first row stays on the same item when rows move
    /// (new rows at the top of Logs); on the first row it keeps following the top.
    fn set_items(&mut self, items: Vec<Value>, count: u64) {
        let keep = if self.list.selected > 0 { self.list.current().map(item_key).filter(|k| !k.is_empty()) } else { None };
        self.list.items = items;
        self.list.count = count;
        self.apply_sort();
        let clamped = self.list.selected.min(self.list.items.len().saturating_sub(1));
        self.list.selected = keep.and_then(|k| self.list.items.iter().position(|v| item_key(v) == k)).unwrap_or(clamped);
    }

    fn apply_sort(&mut self) {
        if let Some((col, desc)) = self.list.sort {
            let cols = self.list.kind.columns();
            if let Some(c) = cols.get(col) {
                let get = c.get;
                self.list.items.sort_by(|a, b| {
                    let (x, y) = (get(a), get(b));
                    let o = match (x.parse::<f64>(), y.parse::<f64>()) {
                        (Ok(p), Ok(q)) => p.partial_cmp(&q).unwrap_or(std::cmp::Ordering::Equal),
                        _ => x.to_lowercase().cmp(&y.to_lowercase()),
                    };
                    if desc { o.reverse() } else { o }
                });
            }
        }
    }

    // ---- modals --------------------------------------------------------------------

    pub fn form(&mut self, form: Form, submit: impl FnMut(&mut App, &Form) -> Result<(), String> + 'static) {
        self.modal = Some(Modal::Form { form, submit: Box::new(submit) });
    }

    /// Load the options of a pick field in the background. The answer, also a load error, goes into the popup.
    pub fn load_pick(&mut self, form_id: u64, field: usize, source: Source, role: String) {
        if self.conn.is_none() {
            return self.apply_pick(form_id, field, Err("not connected".into()));
        }
        self.job("load choices", move |ctx| async move {
            let result = super::pick::load_options(&ctx, &source, &role).await.map_err(|e| format!("{e:#}"));
            super::actions::ap(move |app| app.apply_pick(form_id, field, result))
        });
    }

    /// Hand loaded options to the form, if that form is still the open one.
    pub fn apply_pick(&mut self, form_id: u64, field: usize, result: Result<Vec<PickItem>, String>) {
        if let Some(Modal::Form { form, .. }) = &mut self.modal
            && form.id == form_id
        {
            form.set_options(field, result);
        }
    }

    pub fn confirm(&mut self, title: impl Into<String>, lines: Vec<String>, on_yes: impl FnOnce(&mut App) + 'static) {
        self.modal = Some(Modal::Confirm {
            title: title.into(),
            lines,
            typed: None,
            input: String::new(),
            on_yes: Some(Box::new(on_yes)),
            danger: false,
            scroll: 0,
        });
    }

    /// Destructive confirmation: the operator types `typed` (a name) to proceed.
    pub fn confirm_danger(
        &mut self,
        title: impl Into<String>,
        mut lines: Vec<String>,
        typed: &str,
        on_yes: impl FnOnce(&mut App) + 'static,
    ) {
        if self.profile.as_ref().and_then(|p| self.cfg.profiles.get(p)).is_some_and(|p| p.production) {
            lines.insert(0, format!("!!! PRODUCTION SERVER: {} !!!", self.profile.clone().unwrap_or_default()));
        }
        self.modal = Some(Modal::Confirm {
            title: title.into(),
            lines,
            typed: Some(typed.to_string()),
            input: String::new(),
            on_yes: Some(Box::new(on_yes)),
            danger: true,
            scroll: 0,
        });
    }

    pub fn text(&mut self, title: impl Into<String>, lines: Vec<String>) {
        self.modal = Some(Modal::Text { title: title.into(), lines, scroll: 0 });
    }

    pub fn input(&mut self, title: impl Into<String>, initial: &str, on_done: impl FnOnce(&mut App, String) + 'static) {
        self.modal = Some(Modal::Input { title: title.into(), input: initial.into(), on_done: Some(Box::new(on_done)) });
    }

    // ---- keys ----------------------------------------------------------------------

    pub fn handle_key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.modal.is_some() {
            self.modal_key(k);
            return;
        }
        match k.code {
            KeyCode::Char('q') => {
                let busy = self.jobs.iter().filter(|j| !j.is_load).count();
                if busy > 0 {
                    self.confirm(
                        "Quit?",
                        vec![
                            format!("{busy} operation(s) are still running in this client."),
                            "Quitting stops waiting for them. Pulp tasks already submitted keep running".into(),
                            "on the server and are re-attached in the Task Center on next start.".into(),
                            "Unfinished Content View publishes can be resumed from their checkpoints.".into(),
                        ],
                        |app| app.quit = true,
                    );
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Char('?') => self.open(Screen::Help),
            KeyCode::Char(':') => self.modal = Some(Modal::Palette { input: String::new(), selected: 0 }),
            KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.modal = Some(Modal::Palette { input: String::new(), selected: 0 })
            }
            KeyCode::Char('N') => {
                let lines = self.notices.iter().rev().map(|n| format!("{:?}: {}", n.level, n.text)).collect();
                self.text("Notifications", lines);
            }
            KeyCode::Tab => self.focus = if self.focus == Focus::Nav { Focus::List } else { Focus::Nav },
            _ if self.focus == Focus::Nav => self.nav_key(k),
            _ => match self.screen.clone() {
                Screen::List(_) => self.list_key(k),
                Screen::Dashboard | Screen::Help => match k.code {
                    KeyCode::Char('r') => self.reload(),
                    KeyCode::Esc => self.back(),
                    KeyCode::Up | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('k') => {
                        self.focus = Focus::Nav;
                        self.nav_key(k);
                    }
                    _ => {}
                },
            },
        }
    }

    fn nav_key(&mut self, k: KeyEvent) {
        let n = kinds::NAV.len() + 2; // + Dashboard + Help
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.nav = (self.nav + n - 1) % n,
            KeyCode::Down | KeyCode::Char('j') => self.nav = (self.nav + 1) % n,
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                let s = match self.nav {
                    0 => Screen::Dashboard,
                    i if i <= kinds::NAV.len() => Screen::List(kinds::NAV[i - 1].1.clone()),
                    _ => Screen::Help,
                };
                self.history.clear();
                self.open(s);
                self.focus = Focus::List;
            }
            KeyCode::Esc => self.focus = Focus::List,
            _ => {}
        }
    }

    fn list_key(&mut self, k: KeyEvent) {
        let len = self.list.items.len();
        let page = 15;
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => self.list.selected = (self.list.selected + 1).min(len.saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => self.list.selected = self.list.selected.saturating_sub(1),
            KeyCode::PageDown => self.list.selected = (self.list.selected + page).min(len.saturating_sub(1)),
            KeyCode::PageUp => self.list.selected = self.list.selected.saturating_sub(page),
            KeyCode::Home | KeyCode::Char('g') => self.list.selected = 0,
            KeyCode::End | KeyCode::Char('G') => self.list.selected = len.saturating_sub(1),
            KeyCode::Left | KeyCode::Char('h') => self.focus = Focus::Nav,
            KeyCode::Char('n') if self.list.offset + PAGE < self.list.count => {
                self.list.offset += PAGE;
                self.list.selected = 0;
                self.reload();
            }
            KeyCode::Char('p') if self.list.offset > 0 => {
                self.list.offset = self.list.offset.saturating_sub(PAGE);
                self.list.selected = 0;
                self.reload();
            }
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char('/') => {
                let cur = self.list.filter.clone();
                self.input("Search (server-side filter; empty clears)", &cur, |app, s| {
                    app.list.filter = s;
                    app.list.offset = 0;
                    app.list.selected = 0;
                    app.reload();
                });
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                let ncols = self.list.kind.columns().len().max(1);
                self.list.sort = Some(match (self.list.sort, k.code) {
                    (Some((c, d)), KeyCode::Char('S')) => (c, !d),
                    (Some((c, _)), _) => ((c + 1) % ncols, false),
                    (None, _) => (0, false),
                });
                self.apply_sort();
            }
            KeyCode::Char(' ') => {
                if let Some(v) = self.list.current() {
                    let key = item_key(v);
                    if !self.list.marked.remove(&key) {
                        self.list.marked.insert(key);
                    }
                    self.list.selected = (self.list.selected + 1).min(len.saturating_sub(1));
                }
            }
            KeyCode::Char('t') => {
                if let Some(next) = self.list.kind.toggle() {
                    if self.history.is_empty() {
                        self.open(Screen::List(next));
                    } else {
                        self.list = ListState::new(next.clone());
                        self.screen = Screen::List(next);
                        self.reload();
                    }
                }
            }
            KeyCode::Esc => {
                if self.list.loading {
                    for id in self.jobs.iter().filter(|j| j.is_load && j.label.starts_with("load ")).map(|j| j.id).collect::<Vec<_>>() {
                        self.abort_job(id);
                    }
                    self.list.loading = false;
                    self.list.error = Some("request cancelled".into());
                } else if !self.list.marked.is_empty() {
                    self.list.marked.clear();
                } else {
                    self.back();
                }
            }
            KeyCode::Enter => self.open_menu(),
            KeyCode::Char(c) => {
                let item = self.list.current().cloned().unwrap_or(Value::Null);
                if let Some(a) = self.available_actions(&item).into_iter().find(|a| a.key() == c) {
                    super::actions::run(self, a, item);
                }
            }
            _ => {}
        }
    }

    pub fn available_actions(&self, item: &Value) -> Vec<Act> {
        let ro = self.conn.as_ref().is_some_and(|c| c.client.read_only());
        let shared = self.shared();
        self.list
            .kind
            .actions()
            .into_iter()
            .filter(|a| !(ro && a.mutates(&self.list.kind, shared)))
            .filter(|a| item.is_object() || !a.needs_item())
            .collect()
    }

    fn open_menu(&mut self) {
        let item = self.list.current().cloned().unwrap_or(Value::Null);
        // The Logs screen has one action: Enter opens it without a menu.
        if matches!(self.list.kind, Kind::Logs { .. }) {
            if item.is_object() {
                super::actions::run(self, Act::Inspect, item);
            }
            return;
        }
        let items = self.available_actions(&item);
        if items.is_empty() {
            return;
        }
        let ro = self.conn.as_ref().is_some_and(|c| c.client.read_only());
        let title = if ro { "Actions (read-only mode)".to_string() } else { "Actions".to_string() };
        self.modal = Some(Modal::Menu { title, items, selected: 0, item });
    }

    fn modal_key(&mut self, k: KeyEvent) {
        let Some(modal) = self.modal.take() else { return };
        match modal {
            Modal::Menu { title, items, mut selected, item } => match k.code {
                KeyCode::Esc => {}
                KeyCode::Up | KeyCode::Char('k') => {
                    selected = (selected + items.len() - 1) % items.len();
                    self.modal = Some(Modal::Menu { title, items, selected, item });
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1) % items.len();
                    self.modal = Some(Modal::Menu { title, items, selected, item });
                }
                KeyCode::Enter => super::actions::run(self, items[selected], item),
                KeyCode::Char(c) => match items.iter().find(|a| a.key() == c) {
                    Some(a) => super::actions::run(self, *a, item),
                    None => self.modal = Some(Modal::Menu { title, items, selected, item }),
                },
                _ => self.modal = Some(Modal::Menu { title, items, selected, item }),
            },
            Modal::Form { mut form, mut submit } => match form.handle_key(k) {
                FormEvent::Cancel => {}
                FormEvent::None => self.modal = Some(Modal::Form { form, submit }),
                FormEvent::Load(i) => {
                    let (id, role) = (form.id, form.get("role").to_string());
                    let source = match &form.fields[i].kind {
                        FieldKind::Pick { source, .. } => Some(source.clone()),
                        _ => None,
                    };
                    self.modal = Some(Modal::Form { form, submit });
                    if let Some(source) = source {
                        self.load_pick(id, i, source, role);
                    }
                }
                FormEvent::Submit => {
                    let res = form.validate().and_then(|_| submit(self, &form));
                    if let Err(e) = res {
                        form.error = Some(e);
                        self.modal = Some(Modal::Form { form, submit });
                    }
                }
            },
            Modal::Confirm { title, lines, typed, mut input, mut on_yes, danger, mut scroll } => {
                let ok = match &typed {
                    Some(t) => k.code == KeyCode::Enter && input.trim() == t,
                    None => matches!(k.code, KeyCode::Char('y') | KeyCode::Enter),
                };
                if ok {
                    if let Some(f) = on_yes.take() {
                        f(self);
                    }
                    return;
                }
                let max = lines.len().saturating_sub(1);
                match (k.code, &typed) {
                    (KeyCode::Esc, _) | (KeyCode::Char('n'), None) => return,
                    (KeyCode::Backspace, Some(_)) => {
                        input.pop();
                    }
                    (KeyCode::Down, _) | (KeyCode::Char('j'), None) => scroll = (scroll + 1).min(max),
                    (KeyCode::Up, _) | (KeyCode::Char('k'), None) => scroll = scroll.saturating_sub(1),
                    (KeyCode::PageDown, _) => scroll = (scroll + 20).min(max),
                    (KeyCode::PageUp, _) => scroll = scroll.saturating_sub(20),
                    (KeyCode::Char(c), Some(_)) => input.push(c),
                    _ => {}
                }
                self.modal = Some(Modal::Confirm { title, lines, typed, input, on_yes, danger, scroll });
            }
            Modal::Text { title, lines, mut scroll } => {
                let max = lines.len().saturating_sub(1);
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => return,
                    KeyCode::Down | KeyCode::Char('j') => scroll = (scroll + 1).min(max),
                    KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                    KeyCode::PageDown | KeyCode::Char(' ') => scroll = (scroll + 20).min(max),
                    KeyCode::PageUp => scroll = scroll.saturating_sub(20),
                    KeyCode::Home | KeyCode::Char('g') => scroll = 0,
                    KeyCode::End | KeyCode::Char('G') => scroll = max,
                    _ => {}
                }
                self.modal = Some(Modal::Text { title, lines, scroll });
            }
            Modal::Input { title, mut input, mut on_done } => match k.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    if let Some(f) = on_done.take() {
                        f(self, input.trim().to_string());
                    }
                }
                KeyCode::Backspace => {
                    input.pop();
                    self.modal = Some(Modal::Input { title, input, on_done });
                }
                KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.modal = Some(Modal::Input { title, input: String::new(), on_done });
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    self.modal = Some(Modal::Input { title, input, on_done });
                }
                _ => self.modal = Some(Modal::Input { title, input, on_done }),
            },
            Modal::Palette { mut input, mut selected } => {
                let entries = self.palette_entries(&input);
                match k.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        if let Some((_, f)) = entries.into_iter().nth(selected) {
                            f(self);
                        }
                    }
                    KeyCode::Down => {
                        selected = (selected + 1).min(entries.len().saturating_sub(1));
                        self.modal = Some(Modal::Palette { input, selected });
                    }
                    KeyCode::Up => {
                        selected = selected.saturating_sub(1);
                        self.modal = Some(Modal::Palette { input, selected });
                    }
                    KeyCode::Backspace => {
                        input.pop();
                        self.modal = Some(Modal::Palette { input, selected: 0 });
                    }
                    KeyCode::Char(c) => {
                        input.push(c);
                        self.modal = Some(Modal::Palette { input, selected: 0 });
                    }
                    _ => self.modal = Some(Modal::Palette { input, selected }),
                }
            }
        }
    }

    /// Command palette: screens, server profiles and global commands, filtered by text.
    pub fn palette_entries(&self, filter: &str) -> Vec<PaletteEntry> {
        let mut out: Vec<PaletteEntry> = vec![];
        out.push(("Go: Dashboard".into(), Box::new(|a: &mut App| a.open(Screen::Dashboard))));
        for (label, k) in kinds::NAV {
            let k = k.clone();
            out.push((
                format!("Go: {label}"),
                Box::new(move |a: &mut App| {
                    a.history.clear();
                    a.open(Screen::List(k));
                }),
            ));
        }
        out.push(("Go: Help".into(), Box::new(|a: &mut App| a.open(Screen::Help))));
        for name in self.cfg.profiles.keys() {
            let n = name.clone();
            out.push((format!("Connect: {name}"), Box::new(move |a: &mut App| a.connect(&n))));
        }
        out.push((
            "Tasks: dismiss finished".into(),
            Box::new(|a: &mut App| {
                let n = a.tasks.dismiss_finished();
                a.ok(format!("dismissed {n} finished task(s)"));
            }),
        ));
        out.push((
            "Show notifications".into(),
            Box::new(|a: &mut App| a.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::NONE))),
        ));
        let f = filter.to_lowercase();
        out.retain(|(l, _)| f.split_whitespace().all(|w| l.to_lowercase().contains(w)));
        out
    }

    pub fn read_only(&self) -> bool {
        self.conn.as_ref().is_some_and(|c| c.client.read_only())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthMethod, Database, Profile};
    use crate::store::{NotMigrated, PG_MIGRATIONS};
    use crate::tui::form::Field;

    /// A migrated external SQLite CV store (`Store::external` is true).
    async fn external_store(dir: &tempfile::TempDir) -> Arc<Store> {
        let url = format!("sqlite://{}", dir.path().join("cv.db").display());
        Store::migrate_external(&url, "cv.db").await.unwrap();
        Arc::new(Store::open_external(&url, "cv.db").await.unwrap())
    }

    fn lab(url: &str) -> Config {
        let mut cfg = Config::default();
        cfg.profiles.insert("lab".into(), Profile { url: url.into(), auth: AuthMethod::None, ..Default::default() });
        cfg
    }

    #[tokio::test]
    async fn external_scope_never_falls_back_to_the_profile_name() {
        let dir = tempfile::tempdir().unwrap();
        let st = external_store(&dir).await;
        let local = Arc::new(Store::open_in_memory().await.unwrap());
        let mut app = App::new(lab("https://pulp.example.com"), "/dev/null".into(), local.clone(), Ok(st.clone()));
        app.profile = Some("lab".into());
        assert_eq!(app.scope("lab"), Ok("https://pulp.example.com/pulp/#default".into()));
        app.cfg.profiles.clear(); // the active profile was deleted in Servers
        assert_eq!(app.scope("lab"), Err(NO_PROFILE.into()));
        app.open(Screen::List(Kind::Environments));
        assert_eq!(app.list.error.as_deref(), Some(NO_PROFILE));
        assert!(st.environments("lab").await.unwrap().is_empty(), "no Library row under the bare profile name");
        // The local file keeps scoping by profile name.
        let mut app = App::new(Config::default(), "/dev/null".into(), local.clone(), Ok(local));
        app.profile = Some("lab".into());
        assert_eq!(app.scope("lab"), Ok("lab".into()));
    }

    #[tokio::test]
    async fn connected_scope_is_fixed_at_connect_time() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let ok = ResponseTemplate::new(200).set_body_json(serde_json::json!({"versions": []}));
        Mock::given(method("GET")).and(path("/pulp/api/v3/status/")).respond_with(ok).mount(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let st = external_store(&dir).await;
        let mut app = App::new(lab(&server.uri()), "/dev/null".into(), st.clone(), Ok(st));
        let mut rx = app.rx.take().unwrap();
        app.connect("lab");
        while app.conn.is_none() {
            let m = rx.recv().await.unwrap();
            app.handle_msg(m);
        }
        let key = format!("{}/pulp/#default", server.uri());
        // Edit says "reconnect to apply": the client still talks to the old server, so the scope stays.
        app.cfg.profiles.get_mut("lab").unwrap().url = "https://other.example.com".into();
        assert_eq!(app.ctx().unwrap().scope, key);
        assert_eq!(app.scope("lab"), Ok(key));
        assert_eq!(app.ctx().unwrap().actor, crate::store::actor(None), "the mock server names no user");
    }

    fn db(url: &str) -> Option<Database> {
        Some(Database { url: url.into(), secret: None, ca_cert: None })
    }

    async fn pulp() -> wiremock::MockServer {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let ok = ResponseTemplate::new(200).set_body_json(serde_json::json!({"versions": []}));
        Mock::given(method("GET")).and(path("/pulp/api/v3/status/")).respond_with(ok).mount(&server).await;
        server
    }

    async fn settle(app: &mut App, rx: &mut UnboundedReceiver<Msg>) {
        while !app.jobs.is_empty() {
            let m = rx.recv().await.unwrap();
            app.handle_msg(m);
        }
    }

    #[tokio::test]
    async fn profiles_with_one_database_share_one_store() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("cv.db").display());
        Store::migrate_external(&url, "cv.db").await.unwrap();
        let new_url = format!("sqlite://{}", dir.path().join("new.db").display());
        let mut cfg = Config::default();
        for (name, database) in [("lab", db(&url)), ("prod", db(&url)), ("new", db(&new_url)), ("file", None)] {
            let p = Profile { url: "https://pulp.example.com".into(), auth: AuthMethod::None, database, ..Default::default() };
            cfg.profiles.insert(name.into(), p);
        }
        let local = Arc::new(Store::open_in_memory().await.unwrap());
        let mut app = App::new(cfg, "/dev/null".into(), local.clone(), Err(NO_PROFILE.into()));
        let a = app.open_store(app.cfg.database_for("lab")).await.unwrap();
        let b = app.open_store(app.cfg.database_for("prod")).await.unwrap();
        assert!(Arc::ptr_eq(&a, &b) && a.external);
        assert!(Arc::ptr_eq(&app.open_store(app.cfg.database_for("file")).await.unwrap(), &local));
        assert!(app.open_store(app.cfg.database_for("new")).await.unwrap_err().downcast_ref::<NotMigrated>().is_some());
        Store::migrate_external(&new_url, "new.db").await.unwrap();
        assert!(app.open_store(app.cfg.database_for("new")).await.is_ok(), "a failed open is not cached");
        app.profile = Some("lab".into());
        assert!(app.shared());
        app.profile = Some("file".into());
        assert!(!app.shared());
    }

    #[tokio::test]
    async fn migrate_offer_for_an_empty_sqlite_file() {
        let server = pulp().await;
        let dir = tempfile::tempdir().unwrap();
        let local = Arc::new(Store::open_in_memory().await.unwrap());
        // lab on the mock server, its database a 0-byte SQLite file.
        let new_app = |file: &str, read_only: bool| {
            let f = dir.path().join(file);
            std::fs::write(&f, b"").unwrap();
            let mut cfg = lab(&server.uri());
            let p = cfg.profiles.get_mut("lab").unwrap();
            p.database = db(&format!("sqlite://{}", f.display()));
            p.read_only = read_only;
            let mut app = App::new(cfg, "/dev/null".into(), local.clone(), Err(NO_PROFILE.into()));
            let rx = app.rx.take().unwrap();
            (app, rx)
        };
        let (mut app, mut rx) = new_app("cv.db", false);
        app.connect("lab");
        settle(&mut app, &mut rx).await;
        let not = "database not migrated (open Servers, Connect again to migrate, or run pulp-tui db migrate --profile lab)";
        assert!(app.cv_store.as_ref().unwrap_err().contains(not));
        let Some(Modal::Confirm { title, lines, on_yes, .. }) = app.modal.take() else { panic!("no migrate offer") };
        assert_eq!(title, "Migrate database of lab?");
        assert!(lines.contains(&format!("Schema: empty -> {}", PG_MIGRATIONS.len())));
        assert!(lines.iter().any(|l| l.contains(env!("CARGO_PKG_VERSION"))));
        on_yes.unwrap()(&mut app);
        settle(&mut app, &mut rx).await;
        let st = app.cv_store.clone().unwrap();
        assert!(st.external);
        assert_eq!(st.schema_version().await.unwrap(), PG_MIGRATIONS.len() as i64);

        let (mut ro, mut rx) = new_app("ro.db", true);
        ro.connect("lab");
        settle(&mut ro, &mut rx).await;
        assert!(ro.modal.is_none());
        assert!(ro.notices.iter().any(|n| n.text.contains("is read-only, so")));
        assert!(ro.cv_store.as_ref().unwrap_err().contains(not));

        let (mut busy, mut rx) = new_app("busy.db", false);
        busy.connect("lab");
        busy.form(Form::new("t", vec![]), |_, _| Ok(()));
        settle(&mut busy, &mut rx).await;
        assert!(matches!(busy.modal, Some(Modal::Form { .. })), "the offer never replaces an open modal");
    }

    #[tokio::test]
    async fn switching_profiles_never_shows_the_old_store() {
        let server = pulp().await;
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("cv.db").display());
        Store::migrate_external(&url, "cv.db").await.unwrap();
        let mut cfg = lab(&server.uri());
        cfg.profiles.get_mut("lab").unwrap().database = db(&url);
        cfg.profiles.insert("prod".into(), Profile { url: server.uri(), auth: AuthMethod::None, ..Default::default() });
        let local = Arc::new(Store::open_in_memory().await.unwrap());
        let mut app = App::new(cfg, "/dev/null".into(), local.clone(), Ok(local.clone()));
        let mut rx = app.rx.take().unwrap();
        app.connect("lab");
        assert!(app.cv_store.as_ref().is_err_and(|e| e.ends_with(": opening")), "never the previous store");
        app.connect("prod");
        assert!(Arc::ptr_eq(app.cv_store.as_ref().unwrap(), &local));
        settle(&mut app, &mut rx).await;
        assert!(Arc::ptr_eq(app.cv_store.as_ref().unwrap(), &local), "lab's late result is dropped");
        app.connect("lab");
        settle(&mut app, &mut rx).await;
        assert!(app.cv_store.as_ref().unwrap().external);
        assert_eq!(app.ctx().unwrap().scope, format!("{}/pulp/#default", server.uri()));
    }

    #[tokio::test]
    async fn store_errors_hide_the_database_password() {
        unsafe { std::env::set_var("PTUI_TEST_PROFILE_DBPW", "hunter2-profile") };
        let mut cfg = lab("https://pulp.example.com");
        cfg.profiles.get_mut("lab").unwrap().database =
            Some(Database { url: "postgres://u@127.0.0.1:1/x".into(), secret: Some("env:PTUI_TEST_PROFILE_DBPW".into()), ca_cert: None });
        let local = Arc::new(Store::open_in_memory().await.unwrap());
        let mut app = App::new(cfg, "/dev/null".into(), local.clone(), Ok(local));
        let mut rx = app.rx.take().unwrap();
        app.profile = Some("lab".into());
        app.with_store("lab", false, app.cfg.database_for("lab"), |app, r| app.cv_store = r);
        settle(&mut app, &mut rx).await;
        let e = app.cv_store.clone().unwrap_err();
        assert!(e.starts_with("postgres://u@127.0.0.1:1/x: ") && !e.contains("hunter2"), "{e}");
    }

    #[tokio::test]
    async fn shared_follows_the_store_in_use_not_the_edited_config() {
        let server = pulp().await;
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("cv.db").display());
        Store::migrate_external(&url, "cv.db").await.unwrap();
        let mut cfg = lab(&server.uri());
        let p = cfg.profiles.get_mut("lab").unwrap();
        (p.database, p.read_only) = (db(&url), true);
        let local = Arc::new(Store::open_in_memory().await.unwrap());
        let mut app = App::new(cfg, "/dev/null".into(), local, Err(NO_PROFILE.into()));
        let mut rx = app.rx.take().unwrap();
        app.connect("lab");
        settle(&mut app, &mut rx).await;
        // Edit to the local file saves at once, but this session keeps writing to the shared store until Connect.
        app.cfg.profiles.get_mut("lab").unwrap().database = db(crate::config::LOCAL_DATABASE);
        app.list = ListState::new(Kind::ContentViews);
        assert!(app.read_only() && app.shared());
        assert!(!app.available_actions(&Value::Null).contains(&Act::Create));
        super::super::actions::run(&mut app, Act::Create, Value::Null);
        assert!(app.modal.is_none() && app.notices.iter().any(|n| n.text.contains("read-only mode")));
        // A profile edited to a database while it uses the local file does not write to it either.
        app.cv_store = Ok(app.store.clone());
        app.cfg.profiles.get_mut("lab").unwrap().database = db(&url);
        assert!(!app.shared());
    }

    #[tokio::test]
    async fn saving_a_new_database_offers_the_migration() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cv.db"), b"").unwrap();
        let mut app = app_on(Kind::Profiles).await;
        app.cfg_path = dir.path().join("config.toml");
        let mut rx = app.rx.take().unwrap();
        super::super::actions::run(&mut app, Act::Create, Value::Null);
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no create form") };
        let file = dir.path().join("cv.db").display().to_string();
        for (key, value) in [("name", "web"), ("url", "https://pulp.example.com"), ("auth", "none"), ("database", &file)] {
            form.fields.iter_mut().find(|f| f.key == key).unwrap().value = value.into();
        }
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        assert!(matches!(&app.modal, Some(Modal::Confirm { title, .. }) if title == "Migrate database of web?"));
        let saved = Config::load(&app.cfg_path).unwrap().profiles["web"].database.clone().unwrap();
        assert_eq!(saved.url, format!("sqlite://{file}"));

        // The same database again (an edit that leaves the field alone) is not offered a second time.
        app.modal = None;
        super::super::actions::run(&mut app, Act::Edit, serde_json::json!({"name": "web"}));
        let Some(Modal::Form { form, mut submit }) = app.modal.take() else { panic!("no edit form") };
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        assert!(app.modal.is_none());
    }

    fn set(form: &mut Form, key: &str, value: &str) {
        form.fields.iter_mut().find(|f| f.key == key).unwrap().value = value.into();
    }

    /// Edit `lab` on the Servers screen: its database becomes `value`.
    fn edit(app: &mut App, value: &str) {
        super::super::actions::run(app, Act::Edit, serde_json::json!({"name": "lab"}));
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no edit form") };
        set(&mut form, "database", value);
        submit(app, &form).unwrap();
    }

    /// `lab` on the local file with a Content View "web"; `<dir>/cv.db` is a migrated external SQLite file.
    async fn lab_with_data(dir: &tempfile::TempDir) -> (App, UnboundedReceiver<Msg>, Arc<Store>, String) {
        let new = external_store(dir).await;
        let mut app = app_on(Kind::Profiles).await;
        app.cfg = lab("https://pulp.example.com");
        app.cfg_path = dir.path().join("config.toml");
        app.store.create_cv("lab", "web", "").await.unwrap();
        let rx = app.rx.take().unwrap();
        (app, rx, new, dir.path().join("cv.db").display().to_string())
    }

    #[tokio::test]
    async fn database_change_with_data_offers_copy_and_keeps_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let (mut app, mut rx, _new, file) = lab_with_data(&dir).await;
        edit(&mut app, &file);
        settle(&mut app, &mut rx).await;
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no copy offer") };
        assert_eq!(form.title, "Database of lab changes");
        assert!(form.notes.contains(&"1 Content View(s), 0 version(s), 0 environment(s) of this server in the old database.".to_string()));
        assert_eq!(app.cfg.profiles["lab"].database, None, "nothing is saved before the choice");
        set(&mut form, "choice", "copy and switch");
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        let saved = Config::load(&app.cfg_path).unwrap().profiles["lab"].database.clone().unwrap();
        assert_eq!(saved.url, format!("sqlite://{file}"));
        let new = app.open_store(app.cfg.database_for("lab")).await.unwrap();
        assert_eq!(new.content_views(&app.cfg.profiles["lab"].server_key()).await.unwrap()[0].name, "web");
        assert!(new.aliases().await.unwrap().contains(&app.store.db_id().to_string()), "copies keep ownership of the old Pulp objects");
        assert_eq!(app.store.content_views("lab").await.unwrap().len(), 1, "the old database is only read");
    }

    #[tokio::test]
    async fn copy_refusal_leaves_the_profile_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let (mut app, mut rx, new, file) = lab_with_data(&dir).await;
        new.create_cv(&app.cfg.profiles["lab"].server_key(), "web", "").await.unwrap();
        edit(&mut app, &file);
        settle(&mut app, &mut rx).await;
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no copy offer") };
        set(&mut form, "choice", "copy and switch");
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        assert_eq!(app.cfg.profiles["lab"].database, None);
        assert_eq!(Config::load(&app.cfg_path).unwrap_or_default().profiles.get("lab").and_then(|p| p.database.clone()), None);
        assert!(app.notices.iter().any(|n| n.text.contains("never merges")));
        // Read-only: only "switch without copy"; it saves, and the old data stays.
        app.cfg.profiles.get_mut("lab").unwrap().read_only = true;
        edit(&mut app, &file);
        settle(&mut app, &mut rx).await;
        let Some(Modal::Form { form, mut submit }) = app.modal.take() else { panic!("no choice form") };
        assert!(matches!(&form.fields[0].kind, FieldKind::Choice(c) if c == &["switch without copy"]));
        assert!(form.notes.contains(&"Read-only profile: copy is not offered.".to_string()));
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        assert!(app.cfg.profiles["lab"].database.is_some());
        assert_eq!(app.store.content_views("lab").await.unwrap().len(), 1);
    }

    /// Edit `lab` to the database `path`, choose "copy and switch" and submit.
    async fn copy_to(app: &mut App, rx: &mut UnboundedReceiver<Msg>, path: &str) {
        edit(app, path);
        settle(app, rx).await;
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no copy offer") };
        set(&mut form, "choice", "copy and switch");
        submit(app, &form).unwrap();
        settle(app, rx).await;
    }

    #[tokio::test]
    async fn copy_into_a_missing_database_asks_to_migrate_first() {
        let dir = tempfile::tempdir().unwrap();
        let (mut app, mut rx, _new, _file) = lab_with_data(&dir).await;
        let missing = dir.path().join("new.db");
        let path = missing.display().to_string();
        // Esc on the migrate question: the profile is not saved, in memory or in the file.
        copy_to(&mut app, &mut rx, &path).await;
        assert!(matches!(&app.modal, Some(Modal::Confirm { title, lines, .. })
            if title == "Migrate database of lab?" && lines.last().unwrap() == "Esc: do not migrate now."));
        let note = app.notices.iter().find(|n| n.text.contains("not changed")).expect("no notice").text.clone();
        assert!(note.contains("database not migrated") && !note.contains("db migrate"), "{note}");
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        settle(&mut app, &mut rx).await;
        assert!(app.modal.is_none() && !missing.exists());
        assert_eq!(app.cfg.profiles["lab"].database, None);
        assert_eq!(Config::load(&app.cfg_path).unwrap_or_default().profiles.get("lab").and_then(|p| p.database.clone()), None);
        // Yes: migrate, copy, then save.
        copy_to(&mut app, &mut rx, &path).await;
        let Some(Modal::Confirm { on_yes, .. }) = app.modal.take() else { panic!("no migrate offer") };
        on_yes.unwrap()(&mut app);
        settle(&mut app, &mut rx).await;
        assert_eq!(Config::load(&app.cfg_path).unwrap().profiles["lab"].database.as_ref().unwrap().url, format!("sqlite://{path}"));
        let new = app.open_store(app.cfg.database_for("lab")).await.unwrap();
        assert_eq!(new.content_views(&app.cfg.profiles["lab"].server_key()).await.unwrap()[0].name, "web");
        assert!(app.notices.iter().any(|n| n.text.starts_with("copied 1 content view(s), 0 version(s), 0 environment(s) into ")));
    }

    #[tokio::test]
    async fn copy_then_a_failed_save_says_the_rows_were_copied_and_the_profile_was_not_saved() {
        let dir = tempfile::tempdir().unwrap();
        let (mut app, mut rx, _new, file) = lab_with_data(&dir).await;
        app.cfg_path = dir.path().join("cv.db").join("config.toml"); // its parent is a file: the save fails
        copy_to(&mut app, &mut rx, &file).await;
        let e = app.notices.iter().find(|n| n.level == Level::Error).expect("no error").text.clone();
        assert!(e.starts_with("copied 1 content view(s)") && e.contains("but profile lab was not saved: cannot save "), "{e}");
        assert_eq!(app.cfg.profiles["lab"].database, None, "memory still agrees with the file");
    }

    #[tokio::test]
    async fn changing_only_the_database_secret_saves_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let (mut app, mut rx, new, file) = lab_with_data(&dir).await;
        app.cfg.profiles.get_mut("lab").unwrap().database = db(&format!("sqlite://{file}"));
        new.create_cv(&app.cfg.profiles["lab"].server_key(), "web", "").await.unwrap();
        super::super::actions::run(&mut app, Act::Edit, serde_json::json!({"name": "lab"}));
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no edit form") };
        set(&mut form, "db_secret", "env:PTUI_TEST_OTHER_SECRET");
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        assert!(app.modal.is_none(), "the same database is not copied onto itself");
        let saved = Config::load(&app.cfg_path).unwrap().profiles["lab"].database.clone().unwrap();
        assert_eq!((saved.url, saved.secret), (format!("sqlite://{file}"), Some("env:PTUI_TEST_OTHER_SECRET".into())));
    }

    #[tokio::test]
    async fn database_change_without_data_saves_at_once_and_an_unreadable_old_one_offers_no_copy() {
        let dir = tempfile::tempdir().unwrap();
        let (mut app, mut rx, _new, file) = lab_with_data(&dir).await;
        app.store.delete_cv(app.store.content_views("lab").await.unwrap()[0].id).await.unwrap();
        edit(&mut app, &file);
        settle(&mut app, &mut rx).await;
        assert!(app.modal.is_none(), "no data, no question");
        assert_eq!(Config::load(&app.cfg_path).unwrap().profiles["lab"].database.as_ref().unwrap().url, format!("sqlite://{file}"));
        // The old database is an empty file: nothing can be counted or copied.
        let empty = dir.path().join("empty.db");
        std::fs::write(&empty, b"").unwrap();
        app.cfg.profiles.get_mut("lab").unwrap().database = db(&format!("sqlite://{}", empty.display()));
        edit(&mut app, &file);
        settle(&mut app, &mut rx).await;
        let Some(Modal::Form { form, .. }) = app.modal.take() else { panic!("no choice form") };
        assert!(matches!(&form.fields[0].kind, FieldKind::Choice(c) if c == &["switch without copy"]));
        assert!(form.notes.iter().any(|n| n.starts_with("The old database is unavailable (")));
    }

    #[tokio::test]
    async fn store_secret_asks_which_secret_for_an_own_database() {
        let mut app = app_on(Kind::Profiles).await;
        let local = Database { url: crate::config::LOCAL_DATABASE.into(), secret: None, ca_cert: None };
        for (name, database) in [("prod", db("postgres://u@h/x")), ("lab", None), ("file", Some(local))] {
            let p = Profile { url: "https://pulp.example.com".into(), auth: AuthMethod::None, database, ..Default::default() };
            app.cfg.profiles.insert(name.into(), p);
        }
        for (name, asks) in [("prod", true), ("lab", false), ("file", false)] {
            super::super::actions::run(&mut app, Act::StoreSecret, serde_json::json!({ "name": name }));
            let Some(Modal::Form { form, .. }) = app.modal.take() else { panic!("no form for {name}") };
            assert_eq!(form.fields.iter().any(|f| f.key == "target"), asks, "{name}");
            if asks {
                assert_eq!(form.fields[0].key, "target", "the question comes first");
            }
        }
    }

    #[tokio::test]
    async fn load_result_for_closed_form_is_dropped() {
        let st = Arc::new(Store::open_in_memory().await.unwrap());
        let mut app = App::new(Config::default(), "/dev/null".into(), st.clone(), Ok(st));
        let form = Form::new("t", vec![Field::pick("repo", "Repository", Source::Roles)]);
        let id = form.id;
        app.form(form, |_, _| Ok(()));
        app.modal = None; // closed with Esc meanwhile
        app.apply_pick(id, 0, Ok(vec![]));
        assert!(app.modal.is_none());
        // A different open form is not touched.
        let other = Form::new("t", vec![Field::pick("repo", "Repository", Source::Roles)]);
        app.form(other, |_, _| Ok(()));
        app.apply_pick(id, 0, Ok(vec![PickItem { label: "x".into(), value: "x".into(), note: String::new() }]));
        let Some(Modal::Form { form, .. }) = &app.modal else { panic!() };
        assert!(form.fields[0].options.is_none());
    }

    async fn app_on(kind: Kind) -> App {
        let st = Arc::new(Store::open_in_memory().await.unwrap());
        let mut app = App::new(Config::default(), "/dev/null".into(), st.clone(), Ok(st));
        app.profile = Some("lab".into());
        app.screen = Screen::List(kind.clone());
        app.list = ListState::new(kind);
        app
    }

    fn row_with(summary: &str) -> Value {
        crate::session_log::rows(true).into_iter().find(|r| r["summary"] == summary).unwrap_or_else(|| panic!("no log row {summary:?}"))
    }

    fn text_lines(app: &App) -> Vec<String> {
        match &app.modal {
            Some(Modal::Text { lines, .. }) => lines.clone(),
            _ => panic!("no text modal"),
        }
    }

    #[tokio::test]
    async fn notices_and_actions_are_logged_and_their_rows_open_as_text() {
        let mut app = app_on(Kind::Profiles).await;
        app.notify(Level::Warn, "notice-row-needle");
        let r = row_with("[warning] notice-row-needle");
        assert_eq!((r["kind"].as_str(), r["server"].as_str()), (Some("notice"), Some("lab")));
        assert_eq!(crate::session_log::detail(r["id"].as_u64().unwrap()), Some(vec!["notice-row-needle".to_string()]));

        // A profile has `name`; the action row names it. It is not a Logs inspect, so it is recorded.
        super::super::actions::run(&mut app, Act::Inspect, serde_json::json!({"name": "action-row-needle"}));
        let a = row_with("Inspect (full JSON) on action-row-needle");
        assert_eq!(a["kind"], "action");

        // Logs: Enter opens the text view with no menu; empty detail shows the summary; the log is not extended.
        let mut app = app_on(Kind::Logs { polls: false }).await;
        app.profile = Some("logs-inspect-profile".into()); // only this App records under it
        app.list.items = vec![a.clone()];
        app.open_menu();
        assert_eq!(text_lines(&app), ["Inspect (full JSON) on action-row-needle"]);
        app.list.items = vec![r];
        app.open_menu();
        assert_eq!(text_lines(&app), ["notice-row-needle"]);
        // Inspecting a log row is not itself logged (that would add a row for every look).
        let own = |r: &Value| {
            r["server"] == "logs-inspect-profile" && r["summary"].as_str().is_some_and(|s| s.starts_with("Inspect (full JSON)"))
        };
        assert!(!crate::session_log::rows(true).iter().any(own));
        app.list.items = vec![serde_json::json!({"id": u64::MAX, "summary": "gone"})];
        app.open_menu();
        assert_eq!(text_lines(&app), ["This entry is older than the last 1000 and was dropped."]);
    }

    #[tokio::test]
    async fn reload_keeps_the_cursor_on_the_same_item() {
        let row = |id: u64| serde_json::json!({"id": id});
        let mut app = app_on(Kind::Logs { polls: false }).await;
        app.list.items = vec![row(3), row(2), row(1)];
        app.list.selected = 1;
        // A new row on top pushes everything down: the cursor follows id 2.
        app.set_items(vec![row(4), row(3), row(2), row(1)], 4);
        assert_eq!(app.list.selected, 2);
        assert_eq!(item_key(app.list.current().unwrap()), "2");
        // On the first row the cursor keeps following the newest row.
        app.list.selected = 0;
        app.set_items(vec![row(5), row(4), row(3), row(2), row(1)], 5);
        assert_eq!(app.list.selected, 0);
        // The selected item is gone: the old index, clamped.
        app.list.selected = 4;
        app.set_items(vec![row(9), row(8)], 2);
        assert_eq!(app.list.selected, 1);
        // Rows without a key cannot be followed: the old index, clamped.
        app.list.items = vec![serde_json::json!({}), serde_json::json!({})];
        app.list.selected = 1;
        app.set_items(vec![serde_json::json!({}), serde_json::json!({}), serde_json::json!({})], 3);
        assert_eq!(app.list.selected, 1);
    }

    #[tokio::test]
    async fn action_rows_name_a_target_only_for_actions_that_need_an_item() {
        let mut app = app_on(Kind::Profiles).await;
        app.profile = Some("action-rows-server".into());
        let item = serde_json::json!({"name": "target-needle"});
        // Create needs no item: a selected row is not its target.
        super::super::actions::run(&mut app, Act::Create, item.clone());
        super::super::actions::run(&mut app, Act::Inspect, item);
        let mut seen: Vec<String> = crate::session_log::rows(true)
            .into_iter()
            .filter(|r| r["kind"] == "action" && r["server"] == "action-rows-server")
            .map(|r| r["summary"].as_str().unwrap().to_string())
            .collect();
        seen.reverse();
        assert_eq!(seen, ["Create", "Inspect (full JSON) on target-needle"]);
    }

    #[tokio::test]
    async fn logs_screen_reloads_on_new_rows_even_without_a_connection() {
        crate::session_log::record("notice", "lab", "tick-needle".into(), vec![]);
        let mut app = app_on(Kind::Logs { polls: false }).await;
        assert!(app.ctx().is_none());
        app.tick();
        assert!(app.list.loading, "a row newer than logs_seen starts a reload");
        assert!(app.logs_seen > 0);
        // Other screens do not reload.
        let mut other = app_on(Kind::Profiles).await;
        other.tick();
        assert!(!other.list.loading);
    }
}
