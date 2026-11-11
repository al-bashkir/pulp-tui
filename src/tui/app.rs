//! Application state and event handling. Rendering never waits on I/O: every network or
//! disk operation runs as a background job that sends back a closure applied here.

use super::actions::ap;
use super::form::{FieldKind, Form, FormEvent, PickItem, Source, edit_line, move_key};
use super::kinds::{self, Act, Kind};
use super::tasks::{TaskCenter, Tracked};
use super::view::help_lines;
use crate::config::{Config, Database};
use crate::cv::CvService;
use crate::pulp::models::{TASK_FINAL_STATES, Task};
use crate::pulp::{Capabilities, PulpClient};
use crate::store::{NotMigrated, Store};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;
use std::cell::Cell;
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
    /// Task snapshot from a workflow that polls the task itself, and the id of its job.
    Task(Task, Option<u64>),
    TaskPolled {
        href: String,
        result: Result<Task, crate::pulp::PulpError>,
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
    pub job: u64,
    tx: UnboundedSender<Msg>,
}

impl Ctx {
    pub fn progress(&self, text: impl Into<String>) {
        let _ = self.tx.send(Msg::JobProgress { id: self.job, text: text.into() });
    }
    pub fn on_task(&self) -> impl Fn(&Task) + Send + Sync + use<> {
        let (tx, job) = (self.tx.clone(), self.job);
        move |t: &Task| {
            let _ = tx.send(Msg::Task(t.clone(), Some(job)));
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
    /// First visible row; kept by the view between frames (scrolloff).
    pub top: Cell<usize>,
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
            top: Cell::new(0),
        }
    }
    pub fn current(&self) -> Option<&Value> {
        self.items.get(self.selected)
    }
}

pub fn item_key(v: &Value) -> String {
    v.get("pulp_href").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| v.get("id").map(|x| x.to_string()).unwrap_or_default())
}

/// Task Center row of a tracked task.
fn task_row(t: &Tracked) -> Value {
    serde_json::json!({
        "pulp_href": t.href, "label": t.label, "state": t.state, "progress": t.progress(),
        "started": t.task.as_ref().map_or(t.started.clone(), |x| x.started_at.clone().unwrap_or_default()),
        "error": t.error, "task": t.task,
    })
}

/// `Text.watch`: href of a task whose lines follow the Task Center (rebuilt on each task message).
pub enum Modal {
    Menu { title: String, items: Vec<Act>, selected: usize, item: Value },
    Form { form: Form, submit: Submit },
    Confirm { title: String, lines: Vec<String>, typed: Option<String>, input: String, on_yes: Option<OnYes>, danger: bool, scroll: usize },
    Text { title: String, lines: Vec<String>, scroll: usize, watch: Option<String> },
    Input { title: String, input: String, cursor: usize, on_done: Option<OnInput> },
    Palette { input: String, selected: usize, filtering: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Nav,
    List,
}

pub const PAGE: u64 = 100;

/// Rows that PgDn and PgUp move in a list.
const LIST_PAGE_ROWS: usize = 15;

/// A list key with Ctrl-D and Ctrl-U as PgDn and PgUp.
fn list_code(k: KeyEvent) -> KeyCode {
    match k.code {
        KeyCode::Char('d') if k.modifiers.contains(KeyModifiers::CONTROL) => KeyCode::PageDown,
        KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => KeyCode::PageUp,
        c => c,
    }
}

/// A dialog that a job opens ignores keys this long: they were typed before it appeared.
const KEY_GUARD: Duration = Duration::from_millis(300);

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
    /// The Screens pane also shows the screens in `settings.hidden_screens` (`.` toggles).
    pub nav_all: bool,
    pub focus: Focus,
    pub list: ListState,
    pub history: Vec<(Screen, ListState)>,
    pub modal: Option<Modal>,
    /// Dialogs that finished jobs opened over another dialog; each opens when the one before it closes.
    pub queued: VecDeque<Modal>,
    guard_until: Option<Instant>,
    /// A job result is being applied: its dialogs never replace an open one.
    applying: bool,
    pub notices: VecDeque<Notice>,
    pub jobs: Vec<Job>,
    pub tasks: TaskCenter,
    /// The last work queued on the stored task references (`task_store`).
    pub(super) last_task_store: Option<tokio::task::JoinHandle<()>>,
    pub dashboard: Vec<String>,
    /// Dashboard values that came in for this connection; shown until a refresh brings new ones.
    pub dashboard_stats: HashMap<super::actions::Stat, String>,
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
        let tasks = TaskCenter::new(cfg.settings.poll_bounds());
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
            nav_all: false,
            focus: Focus::List,
            list: ListState::new(Kind::Profiles),
            history: vec![],
            modal: None,
            queued: VecDeque::new(),
            guard_until: None,
            applying: false,
            notices: VecDeque::new(),
            jobs: vec![],
            tasks,
            last_task_store: None,
            dashboard: vec![],
            dashboard_stats: HashMap::new(),
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
        // The footer, the Logs row and the log file never show the user info of a URL (audit F47).
        let text = crate::session_log::masked(text.into());
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
        self.server_job(label.into(), false, f);
    }

    /// A `job` that only reads: quit and Connect do not warn about it.
    pub fn load_job<F, Fut>(&mut self, label: impl Into<String>, f: F)
    where
        F: FnOnce(Ctx) -> Fut,
        Fut: Future<Output = anyhow::Result<Apply>> + Send + 'static,
    {
        self.server_job(label.into(), true, f);
    }

    fn server_job<F, Fut>(&mut self, label: String, is_load: bool, f: F)
    where
        F: FnOnce(Ctx) -> Fut,
        Fut: Future<Output = anyhow::Result<Apply>> + Send + 'static,
    {
        let Some(mut ctx) = self.ctx() else {
            self.error("not connected to a Pulp server (open Servers and select a profile)");
            return;
        };
        ctx.job = self.next_id;
        self.spawn(label, is_load, f(ctx));
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
            // Also the text view of the error has no user info of a URL (audit F47).
            let result = fut.await.map_err(|e| crate::session_log::masked(format!("{e:#}")));
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

    /// Cancel the running list loads.
    fn abort_loads(&mut self) {
        for id in self.jobs.iter().filter(|j| j.is_load && j.label.starts_with("load ")).map(|j| j.id).collect::<Vec<_>>() {
            self.abort_job(id);
        }
    }

    fn abort_all(&mut self) {
        for j in std::mem::take(&mut self.jobs) {
            j.abort.abort();
            if !j.is_load {
                self.notify(Level::Warn, format!("stopped waiting for '{}' (Pulp tasks already submitted keep running)", j.label));
            }
        }
        // Only the list load that this aborted would clear it.
        self.list.loading = false;
    }

    pub fn handle_msg(&mut self, m: Msg) {
        match m {
            Msg::JobDone { id, result } => {
                let Some(i) = self.jobs.iter().position(|j| j.id == id) else { return }; // aborted
                let job = self.jobs.remove(i);
                match result {
                    Ok(apply) => {
                        self.applying = true;
                        apply(self);
                        self.applying = false;
                    }
                    Err(e) => {
                        self.error(format!("{}: {e}", job.label));
                        // A notice shows one line: an error of several (a list of failures) opens in the text view too,
                        // as a job result (`applying`): behind an open dialog. Not for a read: a reload fails again
                        // after each finished task.
                        if !job.is_load && e.contains('\n') {
                            self.applying = true;
                            self.text(format!("{} failed", job.label), e.lines().map(str::to_string).collect());
                            self.applying = false;
                        }
                        // A failed operation can have changed state (a version is `failed` now): show it, as after a task.
                        if !job.is_load && matches!(self.screen, Screen::List(_)) {
                            self.reload();
                        }
                    }
                }
                self.next_queued();
            }
            Msg::JobProgress { id, text } => {
                if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
                    j.progress = crate::session_log::masked(text); // a progress line can hold an error (audit F64)
                }
                return; // no task changed: the task rows and a watch stay as they are
            }
            // Sent by a job of the previous server that the Connect aborted.
            Msg::Task(..) if self.connecting => {}
            Msg::Task(t, job) => self.task_update(t, true, job),
            Msg::TaskPolled { href, result } => match result {
                // A poll sent before a Connect answers for the previous server.
                _ if !self.tasks.items.iter().any(|t| t.href == href) => {}
                Ok(t) => self.task_update(t, false, None),
                Err(e) if e.is_not_found() => self.task_gone(&href),
                Err(e) if e.status() == Some(401) => self.task_unauthorized(&href),
                Err(e) => self.tasks.poll_failed(&href, crate::session_log::masked(e.to_string())),
            },
        }
        if self.list.kind == Kind::Tasks && self.screen == Screen::List(Kind::Tasks) {
            let items = self.task_items();
            let n = items.len() as u64;
            self.set_items(items, n);
        }
        if let Some(Modal::Text { watch: Some(h), .. }) = &self.modal {
            let new = self.watch_lines(h);
            if let Some(Modal::Text { lines, .. }) = &mut self.modal {
                *lines = new;
            }
        }
    }

    /// Task Center rows from the in-memory tracker (no I/O).
    pub fn task_items(&self) -> Vec<Value> {
        self.tasks.items.iter().map(task_row).collect()
    }

    // ---- tasks ---------------------------------------------------------------------

    /// Run `work` on the stored task references after all work queued before it, so the writes of a task apply
    /// in the order they were made. It is not a job: a Connect stops only the job that waits for its result.
    fn task_store<T: Send + 'static>(
        &mut self,
        work: impl Future<Output = anyhow::Result<T>> + Send + 'static,
    ) -> impl Future<Output = anyhow::Result<T>> + Send + 'static {
        let (prev, (done, result)) = (self.last_task_store.take(), tokio::sync::oneshot::channel());
        self.last_task_store = Some(tokio::spawn(async move {
            if let Some(p) = prev {
                let _ = p.await;
            }
            let _ = done.send(work.await);
        }));
        async move { result.await? }
    }

    /// Queue a write of task references (`task_store`); the job `label` shows it and its error.
    fn persist_task(&mut self, label: &str, write: impl Future<Output = anyhow::Result<()>> + Send + 'static) {
        let written = self.task_store(write);
        self.spawn(label.into(), true, async move {
            written.await?;
            ap(|_| {})
        });
    }

    /// Remember a Pulp task (persisted so it is re-attached after a restart). `known`: the task as the
    /// server listed it; one that ended before keeps its state and is not reported as just ended.
    pub fn track_task(&mut self, href: &str, label: &str, known: Option<Task>) {
        let state = known.as_ref().map_or("waiting", |t| t.state.as_str()).to_string();
        self.tasks.track(href, label, &state);
        if let Some(t) = known {
            self.task_update(t, false, None);
        }
        if let Some(p) = self.profile.clone() {
            let (st, href, label) = (self.store.clone(), href.to_string(), label.to_string());
            self.persist_task("persist task reference", async move { st.track_task(&p, &href, &label, &state).await });
        }
    }

    /// Remove the finished tasks from the Task Center and from the store, so a restart does not bring them back.
    pub fn dismiss_finished_tasks(&mut self) -> usize {
        let n = self.tasks.dismiss_finished();
        if let Some(p) = self.profile.clone() {
            let st = self.store.clone();
            self.persist_task("forget finished tasks", async move { st.forget_finished_tasks(&p).await });
        }
        n
    }

    /// Track the task if needed (`known` as in `track_task`) and open a text view that follows its state until it ends.
    pub fn watch_task(&mut self, href: &str, label: &str, known: Option<Task>) {
        if !self.tasks.items.iter().any(|t| t.href == href) {
            self.track_task(href, label, known);
        }
        let lines = self.watch_lines(href);
        self.show(Modal::Text { title: format!("Watch task {label}"), lines, scroll: 0, watch: Some(href.into()) });
    }

    fn watch_lines(&self, href: &str) -> Vec<String> {
        self.tasks.items.iter().find(|t| t.href == href).map(|t| super::actions::task_lines(&task_row(t))).unwrap_or_default()
    }

    /// The server has no such task: final, and stored as such so the next start does not re-attach it.
    fn task_gone(&mut self, href: &str) {
        self.tasks.stop(href, super::tasks::GONE, "not found on the server (purged?)");
        let label = self.tasks.items.iter().find(|x| x.href == href).map(|x| x.label.clone()).unwrap_or_default();
        self.notify(Level::Warn, format!("task not found on the server (purged?): {label}"));
        if let Some(p) = self.profile.clone() {
            let (st, href) = (self.store.clone(), href.to_string());
            self.persist_task("persist task state", async move { st.finish_task(&p, &href, super::tasks::GONE).await });
        }
    }

    /// A poll got 401: the credentials of this session no longer work (an own password change, for example).
    /// Polling stops with one notice; the stored reference stays unfinished, so a Connect re-attaches the task.
    fn task_unauthorized(&mut self, href: &str) {
        use super::tasks::UNAUTHORIZED;
        let first = !self.tasks.items.iter().any(|t| t.state == UNAUTHORIZED);
        self.tasks.stop(href, UNAUTHORIZED, "401: the credentials of this session no longer work; Connect again to re-attach");
        if first {
            self.error(
                "task polls get 401: the credentials of this session no longer work. Polling stopped; update the \
                 profile's secret, then Connect again to re-attach the tasks.",
            );
        }
    }

    fn task_update(&mut self, t: Task, driven: bool, job: Option<u64>) {
        let href = t.pulp_href.clone();
        let state = t.state.clone();
        let err = t.error_message();
        // A task that a workflow reports is not tracked yet: store it too, so a restart re-attaches it.
        let new = !self.tasks.items.iter().any(|x| x.href == href);
        // The operation names the row better than Pulp's task name (`pulpcore.app.tasks.base.general_update`).
        if new && let Some(j) = job.and_then(|id| self.jobs.iter().find(|j| j.id == id)) {
            self.tasks.track(&href, &j.label, "waiting");
        }
        let finished = self.tasks.update(t, driven);
        let label = self.tasks.items.iter().find(|x| x.href == href).map(|x| x.label.clone()).unwrap_or_default();
        if let Some(p) = self.profile.clone()
            && (new || finished)
        {
            let (st, href, state, label) = (self.store.clone(), href.clone(), state.clone(), label.clone());
            self.persist_task("persist task state", async move {
                if new {
                    st.track_task(&p, &href, &label, &state).await?;
                }
                if finished {
                    st.finish_task(&p, &href, &state).await?;
                }
                Ok(())
            });
        }
        if finished {
            self.task_finished(&state, &label, err);
        }
    }

    /// The notice of a task that just ended in `state`, with Pulp's error `err`.
    fn task_finished(&mut self, state: &str, label: &str, err: Option<String>) {
        let text = format!("task {state}: {label}{}", err.map(|e| format!(": {e}")).unwrap_or_default());
        match state {
            "completed" => self.ok(format!("task completed: {label}")),
            // Not a success, and not an error that Pulp reports (`GONE`: a group whose tasks were purged).
            "canceled" | "skipped" | super::tasks::GONE => self.notify(Level::Warn, text),
            _ => self.error(text),
        }
        // Refresh the visible list or Dashboard once when a task finishes (not on every tick). A
        // newer load replaces one in flight, which may hold the state from before the task.
        if matches!(self.screen, Screen::List(_) | Screen::Dashboard) {
            self.reload();
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
                let result = crate::pulp::ops::get_task(&c, &href).await;
                let _ = tx.send(Msg::TaskPolled { href, result });
            });
        }
        let now = Instant::now();
        while self.notices.front().is_some_and(|n| now.duration_since(n.at) > Duration::from_secs(600)) {
            self.notices.pop_front();
        }
    }

    // ---- connection ----------------------------------------------------------------

    /// Connect to a profile. Running operations stop waiting, so the operator confirms that first.
    pub fn connect(&mut self, name: &str) {
        let busy: Vec<String> = self.jobs.iter().filter(|j| !j.is_load).map(|j| format!("  {}", j.label)).collect();
        if busy.is_empty() {
            return self.connect_now(name);
        }
        let mut lines = vec![format!("{} operation(s) are still running in this client:", busy.len())];
        lines.extend(busy);
        lines.push("Connecting stops waiting for them. Pulp tasks already submitted keep running on the".into());
        lines.push("server; Content View publishes can be resumed from their checkpoints.".into());
        let n = name.to_string();
        self.confirm(format!("Connect to {name}?"), lines, move |app| app.connect_now(&n));
    }

    fn connect_now(&mut self, name: &str) {
        let Some(p) = self.cfg.profiles.get(name).cloned() else {
            self.error(format!("unknown profile {name}"));
            return;
        };
        self.abort_all();
        // Their actions would run against the new server.
        self.queued.clear();
        self.conn = None;
        self.profile = Some(name.to_string());
        self.connecting = true;
        self.tasks = TaskCenter::new(self.cfg.settings.poll_bounds());
        self.dashboard.clear();
        self.dashboard_stats.clear();
        self.history.clear();
        let db = self.cfg.database_for(name);
        if let Some((d, _)) = &db {
            self.cv_store = Err(format!("{}: opening", d.display_url())); // never the previous profile's store
        }
        self.leave_server_screens();
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
            let apply: Apply = Box::new(move |app: &mut App| app.connected(client, caps, scope));
            anyhow::Ok(apply)
        };
        let label = format!("connect {}", self.profile.as_deref().unwrap_or_default());
        self.spawn(label, true, async move {
            let e = match fut.await {
                Ok(apply) => return Ok(apply),
                Err(e) => format!("{e:#}"),
            };
            ap(move |app| {
                app.connecting = false;
                app.error(format!("connection failed: {e}"));
                app.reload(); // "connecting..." becomes "not connected"
            })
        });
    }

    /// Close the dialog and the screens of the previous server.
    fn leave_server_screens(&mut self) {
        // A form, menu or confirm acts on objects of the previous server.
        self.modal = None;
        // The rows and marks belong to the previous server, and a sub-list names one of its objects (users/2/ is
        // another account on the next server); Servers and Logs are local and keep theirs.
        match self.screen.clone() {
            Screen::List(k) if k.names_an_object() => self.open(Screen::Dashboard),
            Screen::List(k) if !matches!(k, Kind::Profiles | Kind::Logs { .. }) => self.replace(k),
            _ => {}
        }
    }

    /// The connect to `client`'s profile succeeded, unless the operator switched to another profile meanwhile.
    fn connected(&mut self, client: PulpClient, caps: Capabilities, scope: String) {
        if self.profile.as_deref() != Some(client.profile()) {
            return; // switched again meanwhile
        }
        self.connecting = false;
        let user = caps.user.as_ref().map(|u| u.username.clone()).unwrap_or_else(|| "anonymous".into());
        self.ok(format!("connected to {} as {user}", client.profile()));
        if let Some(e) = &caps.openapi_error {
            self.notify(Level::Warn, format!("OpenAPI schema unavailable ({e}); capability checks are limited"));
        }
        self.conn = Some(Conn { client, caps: Arc::new(caps), scope });
        self.reattach_tasks();
        self.open(Screen::Dashboard);
    }

    fn reattach_tasks(&mut self) {
        let Some(p) = self.profile.clone() else { return };
        let st = self.store.clone();
        // After the writes queued before it: a task that ended is read as ended.
        let refs = self.task_store(async move { st.tasks(&p, 50).await });
        // Not named "load ...": list reloads cancel those.
        self.spawn("reattach tasks".into(), true, async move {
            let refs = refs.await?;
            ap(move |app| {
                let mut n = 0;
                for r in refs.iter().rev() {
                    app.tasks.track(&r.href, &r.label, &r.state);
                    if let Some(t) = app.tasks.items.iter_mut().find(|t| t.href == r.href) {
                        t.started = r.created_at.clone();
                    }
                    n += (!super::tasks::is_final_state(&r.state)) as usize;
                }
                if n > 0 {
                    app.notify(Level::Info, format!("re-attached to {n} unfinished task(s) from a previous session"));
                }
                // A final row is never polled: read Pulp's start times once (at most 50 refs, one page).
                let done: Vec<String> =
                    refs.into_iter().filter(|r| TASK_FINAL_STATES.contains(&r.state.as_str())).map(|r| r.href).collect();
                if !done.is_empty() && app.conn.is_some() {
                    app.load_job("read task start times", move |ctx| async move {
                        let c = &ctx.client;
                        // Matched by href below, so a server that ignores the filter cannot give a wrong time.
                        let q = [
                            ("pulp_href__in", done.join(",")),
                            ("fields", "pulp_href,started_at".into()),
                            ("limit", done.len().to_string()),
                        ];
                        let page = match c.list::<Value>(&c.api("tasks/"), &q).await {
                            Ok(p) => p,
                            // Not worth a dialog: the stored times stay.
                            Err(e) => {
                                crate::session_log::record("notice", c.profile(), format!("task start times not read: {e}"), vec![]);
                                return ap(|_| {});
                            }
                        };
                        ap(move |app| {
                            for v in page.results {
                                let (Some(h), Some(s)) = (v["pulp_href"].as_str(), v["started_at"].as_str()) else { continue };
                                if let Some(t) = app.tasks.items.iter_mut().find(|t| t.href == h) {
                                    t.started = s.into();
                                }
                            }
                        })
                    });
                }
            })
        });
    }

    // ---- navigation ----------------------------------------------------------------

    pub fn open(&mut self, s: Screen) {
        if let Screen::List(k) = &s {
            if let Some(i) = kinds::NAV.iter().position(|(_, _, n)| n == k) {
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

    /// Show `k` in place of the current list; Esc still returns to the screen before it.
    pub fn replace(&mut self, k: Kind) {
        self.list = ListState::new(k.clone());
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
            Screen::List(Kind::Tasks) => {
                let items = self.task_items();
                let n = items.len() as u64;
                self.set_items(items, n);
                self.list.loading = false;
                self.list.error = None;
            }
            Screen::List(k) => {
                // Cancel a still-running load of the same list; a newer generation wins.
                self.abort_loads();
                self.list.generation += 1;
                match self.list_source(&k) {
                    Ok((ctx, scope)) => self.load_list(k, ctx, scope),
                    Err(e) => {
                        self.list.loading = false;
                        self.list.error = Some(e);
                    }
                }
            }
        }
    }

    /// What a load of list `k` reads with (the connection, the Content View scope), or why it cannot load now.
    fn list_source(&self, k: &Kind) -> Result<(Option<Ctx>, String), String> {
        if let Some(e) = self.no_connection(k) {
            return Err(e);
        }
        let ctx = self.ctx();
        if let Some(e) = ctx.as_ref().and_then(|c| k.unsupported(&c.caps)) {
            return Err(e);
        }
        match self.scope(self.profile.as_deref().unwrap_or_default()) {
            Ok(s) => Ok((ctx, s)),
            Err(e) if matches!(k, Kind::ContentViews | Kind::CvVersions { .. } | Kind::Environments) => Err(e),
            Err(_) => Ok((ctx, String::new())), // other screens never read the CV store
        }
    }

    /// Load the current page of list `k` in the background; a newer load of the list wins.
    fn load_list(&mut self, k: Kind, ctx: Option<Ctx>, scope: String) {
        self.list.loading = true;
        self.list.error = None;
        let generation = self.list.generation;
        let (offset, filter) = (self.list.offset, self.list.filter.clone());
        let (profile, cv, cfg) = (self.profile.clone().unwrap_or_default(), self.cv_store.clone(), self.cfg.clone());
        let fut = async move {
            let cv = cv.as_deref().map_err(String::as_str);
            let (items, count) = kinds::load(&k, ctx.as_ref(), cv, &scope, &profile, &cfg, offset, &filter).await?;
            let apply: Apply = Box::new(move |app: &mut App| app.apply_rows(generation, items, count));
            anyhow::Ok(apply)
        };
        self.spawn("load list".into(), true, async move {
            let e = match fut.await {
                Ok(apply) => return Ok(apply),
                Err(e) => crate::session_log::masked(format!("{e:#}")),
            };
            ap(move |app| {
                if app.list.generation == generation {
                    app.list.loading = false;
                    app.list.error = Some(e);
                }
            })
        });
    }

    /// The rows of load `generation` of the list, unless a newer load started.
    fn apply_rows(&mut self, generation: u64, items: Vec<Value>, count: u64) {
        if self.list.generation != generation {
            return; // stale response
        }
        // Rows were deleted after paging: show the last page that still has rows.
        // Only on the list's own screen: the Dashboard keeps the list but reloads itself.
        if items.is_empty() && self.list.offset > 0 && self.list.offset >= count && self.screen == Screen::List(self.list.kind.clone()) {
            self.list.offset = count.saturating_sub(1) / PAGE * PAGE;
            return self.reload();
        }
        self.list.loading = false;
        self.set_items(items, count);
    }

    /// Replace the rows of the list. A cursor below the first row stays on the same item when rows move
    /// (new rows at the top of Logs); on the first row it keeps following the top.
    fn set_items(&mut self, items: Vec<Value>, count: u64) {
        // New tasks are inserted at the top: on Tasks the first row is followed too, so Cancel keeps its target.
        let follow = self.list.selected > 0 || self.list.kind == Kind::Tasks;
        let keep = if follow { self.list.current().map(item_key).filter(|k| !k.is_empty()) } else { None };
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
                    let o = cell_cmp(&get(a), &get(b));
                    if desc { o.reverse() } else { o }
                });
            }
        }
    }

    // ---- modals --------------------------------------------------------------------

    /// Open a dialog. One that a finished job opens never replaces an open dialog, where the next key
    /// typed for that dialog would answer it: it waits in `queued`.
    fn show(&mut self, m: Modal) {
        if !self.applying {
            self.modal = Some(m);
        } else if self.modal.is_some() {
            self.queued.push_back(m);
        } else {
            self.modal = Some(m);
            self.guard_until = Some(Instant::now() + KEY_GUARD);
        }
    }

    fn next_queued(&mut self) {
        if self.modal.is_none()
            && let Some(m) = self.queued.pop_front()
        {
            self.modal = Some(m);
            self.guard_until = Some(Instant::now() + KEY_GUARD);
        }
    }

    pub fn form(&mut self, form: Form, submit: impl FnMut(&mut App, &Form) -> Result<(), String> + 'static) {
        self.show(Modal::Form { form, submit: Box::new(submit) });
    }

    /// Load the options of a pick field in the background. The answer, also a load error, goes into the popup.
    pub fn load_pick(&mut self, form_id: u64, field: usize, source: Source, role: String) {
        if self.conn.is_none() {
            return self.apply_pick(form_id, field, &role, Err("not connected".into()));
        }
        // Not named "load ...": list reloads cancel those.
        self.load_job("read choices", move |ctx| async move {
            let result = super::pick::load_options(&ctx, &source, &role).await.map_err(|e| crate::session_log::masked(format!("{e:#}")));
            super::actions::ap(move |app| app.apply_pick(form_id, field, &role, result))
        });
    }

    /// Hand loaded options to the form, if that form is still the open one and its role (if any) did not change.
    pub fn apply_pick(&mut self, form_id: u64, field: usize, role: &str, result: Result<Vec<PickItem>, String>) {
        if let Some(Modal::Form { form, .. }) = &mut self.modal
            && form.id == form_id
            && form.get("role") == role
        {
            form.set_options(field, result);
        }
    }

    pub fn confirm(&mut self, title: impl Into<String>, lines: Vec<String>, on_yes: impl FnOnce(&mut App) + 'static) {
        self.show(Modal::Confirm {
            title: title.into(),
            lines,
            typed: None,
            input: String::new(),
            on_yes: Some(Box::new(on_yes)),
            danger: false,
            scroll: 0,
        });
    }

    fn production(&self) -> bool {
        self.profile.as_ref().and_then(|p| self.cfg.profiles.get(p)).is_some_and(|p| p.production)
    }

    /// `confirm`, but on a production profile the operator types `typed` (a name), as in `confirm_danger`.
    pub fn confirm_prod(&mut self, title: impl Into<String>, lines: Vec<String>, typed: &str, on_yes: impl FnOnce(&mut App) + 'static) {
        if self.production() { self.confirm_danger(title, lines, typed, on_yes) } else { self.confirm(title, lines, on_yes) }
    }

    /// Destructive confirmation: the operator types `typed` (a name) to proceed.
    pub fn confirm_danger(
        &mut self,
        title: impl Into<String>,
        mut lines: Vec<String>,
        typed: &str,
        on_yes: impl FnOnce(&mut App) + 'static,
    ) {
        if self.production() {
            lines.insert(0, format!("!!! PRODUCTION SERVER: {} !!!", self.profile.clone().unwrap_or_default()));
        }
        self.show(Modal::Confirm {
            title: title.into(),
            lines,
            typed: Some(typed.to_string()),
            input: String::new(),
            on_yes: Some(Box::new(on_yes)),
            danger: true,
            scroll: 0,
        });
    }

    /// A text view. Each line loses the user info of its URLs (audit F64): results that jobs build hold error texts.
    pub fn text(&mut self, title: impl Into<String>, lines: Vec<String>) {
        let lines = lines.into_iter().map(crate::session_log::masked).collect();
        self.show(Modal::Text { title: title.into(), lines, scroll: 0, watch: None });
    }

    pub fn input(&mut self, title: impl Into<String>, initial: &str, on_done: impl FnOnce(&mut App, String) + 'static) {
        let cursor = initial.chars().count();
        self.show(Modal::Input { title: title.into(), input: initial.into(), cursor, on_done: Some(Box::new(on_done)) });
    }

    // ---- keys ----------------------------------------------------------------------

    pub fn handle_key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.modal.is_some() {
            if self.guard_until.is_none_or(|g| Instant::now() >= g) {
                self.modal_key(k);
                self.next_queued();
            }
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
            KeyCode::Char('?') => self.text("Help", help_lines()),
            KeyCode::Char(':') => self.modal = Some(Modal::Palette { input: String::new(), selected: 0, filtering: true }),
            KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.modal = Some(Modal::Palette { input: String::new(), selected: 0, filtering: true })
            }
            KeyCode::Char('N') => {
                let lines = self.notices.iter().rev().map(|n| format!("{:?}: {}", n.level, n.text)).collect();
                self.text("Notifications", lines);
            }
            KeyCode::Tab => self.focus = if self.focus == Focus::Nav { Focus::List } else { Focus::Nav },
            _ if self.focus == Focus::Nav => self.nav_key(k),
            _ => match self.screen.clone() {
                Screen::List(_) => self.list_key(k),
                Screen::Dashboard => match k.code {
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

    /// Screen `i` of the Screens pane (0 is Dashboard, then `NAV`) is in `settings.hidden_screens`.
    /// Dashboard and Servers are never hidden.
    pub fn nav_hidden(&self, i: usize) -> bool {
        i > 0
            && kinds::NAV[i - 1].2 != Kind::Profiles
            && self.cfg.settings.hidden_screens.iter().any(|h| h.eq_ignore_ascii_case(kinds::NAV[i - 1].1))
    }

    /// The Screens pane shows screen `i`: it is not hidden, `.` shows all, or the cursor is on it.
    pub fn nav_shown(&self, i: usize) -> bool {
        self.nav_all || i == self.nav || !self.nav_hidden(i)
    }

    /// Hide the screen under the cursor of the Screens pane, or show it again, and save the config.
    fn toggle_hidden(&mut self) {
        let i = self.nav;
        if i == 0 || kinds::NAV[i - 1].2 == Kind::Profiles {
            return self.notify(Level::Warn, "Dashboard and Servers cannot be hidden");
        }
        let label = kinds::NAV[i - 1].1;
        let hide = !self.nav_hidden(i);
        let mut next = self.cfg.clone();
        next.settings.hidden_screens.retain(|h| !h.eq_ignore_ascii_case(label));
        if hide {
            next.settings.hidden_screens.push(label.into());
        }
        match super::actions::save_config(self, &next) {
            Ok(()) => {
                self.cfg = next;
                self.ok(if hide { format!("{label} hidden (. shows hidden screens)") } else { format!("{label} shown") });
            }
            Err(e) => self.error(e),
        }
    }

    fn nav_key(&mut self, k: KeyEvent) {
        let n = kinds::NAV.len() + 1; // + Dashboard
        // The nearest shown screen in steps of `step` (n - 1 goes up); Dashboard is always shown.
        let next = |app: &App, from: usize, step: usize| (1..=n).map(|d| (from + d * step) % n).find(|&i| app.nav_shown(i)).unwrap_or(0);
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.nav = next(self, self.nav, n - 1),
            KeyCode::Down | KeyCode::Char('j') => self.nav = next(self, self.nav, 1),
            KeyCode::Char('g') => self.nav = 0,
            KeyCode::Char('G') => self.nav = next(self, 0, n - 1),
            KeyCode::Char('x') => self.toggle_hidden(),
            KeyCode::Char('.') => self.nav_all = !self.nav_all,
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                let s = match self.nav {
                    0 => Screen::Dashboard,
                    i => Screen::List(kinds::NAV[i - 1].2.clone()),
                };
                self.history.clear();
                self.open(s);
                self.focus = Focus::List;
            }
            KeyCode::Esc => self.focus = Focus::List,
            _ => {}
        }
    }

    /// Each handler takes only its own keys. No action uses a list key (`kinds` test
    /// `action_keys_are_unique_per_screen_and_avoid_list_keys`), so every other character goes to the actions.
    fn list_key(&mut self, k: KeyEvent) {
        let code = list_code(k);
        self.move_in_list(code);
        self.page_key(code);
        self.list_command(code);
        if let KeyCode::Char(c) = code
            && !k.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.action_key(c);
        }
    }

    /// j k, PgDn PgUp (Ctrl-D Ctrl-U), g G: move the selection; h: go to the screen list.
    fn move_in_list(&mut self, code: KeyCode) {
        let (s, last) = (self.list.selected, self.list.items.len().saturating_sub(1));
        self.list.selected = match code {
            KeyCode::Down | KeyCode::Char('j') => (s + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => s.saturating_sub(1),
            KeyCode::PageDown => (s + LIST_PAGE_ROWS).min(last),
            KeyCode::PageUp => s.saturating_sub(LIST_PAGE_ROWS),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => last,
            KeyCode::Left | KeyCode::Char('h') => {
                self.focus = Focus::Nav;
                s
            }
            _ => s,
        };
    }

    /// n p: the next or previous page of the server's list; r: reload; /: search.
    fn page_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('n') if self.list.offset + PAGE < self.list.count => self.turn_page(self.list.offset + PAGE),
            KeyCode::Char('p') if self.list.offset > 0 => self.turn_page(self.list.offset.saturating_sub(PAGE)),
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char('/') => self.search(),
            _ => {}
        }
    }

    /// s S: sort; Space: mark; t: the other type of the screen; Esc; Enter: the action menu.
    fn list_command(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char(c @ ('s' | 'S')) => self.cycle_sort(c == 'S'),
            KeyCode::Char(' ') => self.toggle_mark(),
            KeyCode::Char('t') => self.toggle_type(),
            KeyCode::Esc => self.list_escape(),
            KeyCode::Enter => self.open_menu(),
            _ => {}
        }
    }

    fn turn_page(&mut self, offset: u64) {
        (self.list.offset, self.list.selected) = (offset, 0);
        self.reload();
    }

    fn search(&mut self) {
        // The server has no filter for it: a search would show every row under "filter: ...".
        if !self.list.kind.searchable() {
            return self.error("this list has no search");
        }
        let cur = self.list.filter.clone();
        self.input("Search (server-side filter; empty clears)", &cur, |app, s| {
            app.list.filter = s;
            app.list.offset = 0;
            app.list.selected = 0;
            app.reload();
        });
    }

    /// s: sort by the next column; S (`reverse`): reverse the order.
    fn cycle_sort(&mut self, reverse: bool) {
        let ncols = self.list.kind.columns().len();
        self.list.sort = Some(match (self.list.sort, reverse) {
            (Some((c, d)), true) => (c, !d),
            (Some((c, _)), false) => ((c + 1) % ncols, false),
            (None, _) => (0, false),
        });
        self.apply_sort();
    }

    /// Mark or unmark the selected row, then select the next one.
    fn toggle_mark(&mut self) {
        let Some(v) = self.list.current() else { return };
        let key = item_key(v);
        if !self.list.marked.remove(&key) {
            self.list.marked.insert(key);
        }
        self.list.selected = (self.list.selected + 1).min(self.list.items.len().saturating_sub(1));
    }

    fn toggle_type(&mut self) {
        let Some(next) = self.list.kind.toggle() else { return };
        if self.history.is_empty() {
            self.open(Screen::List(next));
        } else {
            self.replace(next);
        }
    }

    /// Esc: stop a load, else clear the marks, else go back.
    fn list_escape(&mut self) {
        if self.list.loading {
            self.abort_loads();
            self.list.loading = false;
            self.list.error = Some("request cancelled".into());
        } else if !self.list.marked.is_empty() {
            self.list.marked.clear();
        } else {
            self.back();
        }
    }

    /// The action of key `c` on the selected row. On a screen the server does not support, its keys say why.
    fn action_key(&mut self, c: char) {
        let item = self.list.current().cloned().unwrap_or(Value::Null);
        if let Some(a) = self.available_actions(&item).into_iter().find(|a| a.key() == c) {
            super::actions::run(self, a, item);
        } else if let Some(why) = self.unsupported().or_else(|| self.no_connection(&self.list.kind))
            && self.list.kind.actions().iter().any(|a| a.key() == c)
        {
            self.error(why);
        }
    }

    /// Why list `k` cannot be read and changes nothing: it needs a server and none is connected. Also while connecting:
    /// a form opened then would go to the server that answers next, with no capability check.
    fn no_connection(&self, k: &Kind) -> Option<String> {
        (self.conn.is_none() && k.needs_connection())
            .then(|| if self.connecting { "connecting..." } else { "not connected: select a server profile in Servers" }.into())
    }

    /// Why the server cannot serve the current screen (`Kind::unsupported`); None while not connected.
    fn unsupported(&self) -> Option<String> {
        self.conn.as_ref().and_then(|c| self.list.kind.unsupported(&c.caps))
    }

    pub fn available_actions(&self, item: &Value) -> Vec<Act> {
        let ro = self.read_only() || self.no_connection(&self.list.kind).is_some();
        let shared = self.shared();
        let unsupported = self.unsupported().is_some();
        self.list
            .kind
            .item_actions(item)
            .into_iter()
            .filter(|a| !(ro && a.mutates(&self.list.kind, shared)))
            .filter(|a| item.is_object() || !a.needs_item())
            // The fallback replication needs no native replication: the unsupported message names it.
            .filter(|a| !unsupported || *a == Act::FallbackReplicate)
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
        let ro = self.read_only();
        let title = if ro { "Actions (read-only mode)".to_string() } else { "Actions".to_string() };
        self.modal = Some(Modal::Menu { title, items, selected: 0, item });
    }

    fn modal_key(&mut self, k: KeyEvent) {
        let Some(modal) = self.modal.take() else { return };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
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
                KeyCode::Home | KeyCode::Char('g') | KeyCode::End | KeyCode::Char('G') => {
                    selected = if matches!(k.code, KeyCode::End | KeyCode::Char('G')) { items.len() - 1 } else { 0 };
                    self.modal = Some(Modal::Menu { title, items, selected, item });
                }
                KeyCode::Enter => super::actions::run(self, items[selected], item),
                KeyCode::Char(c) if !ctrl => match items.iter().find(|a| a.key() == c) {
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
                    None => k.code == KeyCode::Enter || (k.code == KeyCode::Char('y') && !ctrl),
                };
                if ok {
                    if let Some(f) = on_yes.take() {
                        f(self);
                    }
                    return;
                }
                let max = lines.len().saturating_sub(1);
                match (k.code, &typed) {
                    (KeyCode::Esc, _) => return,
                    (KeyCode::Char('n'), None) if !ctrl => return,
                    (KeyCode::Backspace, Some(_)) => {
                        input.pop();
                    }
                    (KeyCode::Down, _) | (KeyCode::Char('j'), None) => scroll = (scroll + 1).min(max),
                    (KeyCode::Up, _) | (KeyCode::Char('k'), None) => scroll = scroll.saturating_sub(1),
                    (KeyCode::PageDown, _) => scroll = (scroll + 20).min(max),
                    (KeyCode::Char('d'), _) if ctrl => scroll = (scroll + 20).min(max),
                    (KeyCode::PageUp, _) => scroll = scroll.saturating_sub(20),
                    (KeyCode::Char('u'), _) if ctrl => scroll = scroll.saturating_sub(20),
                    (KeyCode::Home, _) | (KeyCode::Char('g'), None) => scroll = 0,
                    (KeyCode::End, _) | (KeyCode::Char('G'), None) => scroll = max,
                    (KeyCode::Char(c), Some(_)) if !ctrl => input.push(c),
                    _ => {}
                }
                self.modal = Some(Modal::Confirm { title, lines, typed, input, on_yes, danger, scroll });
            }
            Modal::Text { title, lines, mut scroll, watch } => {
                let max = lines.len().saturating_sub(1);
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => return,
                    KeyCode::Down | KeyCode::Char('j') => scroll = (scroll + 1).min(max),
                    KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                    KeyCode::PageDown | KeyCode::Char(' ') => scroll = (scroll + 20).min(max),
                    KeyCode::Char('d') if ctrl => scroll = (scroll + 20).min(max),
                    KeyCode::PageUp => scroll = scroll.saturating_sub(20),
                    KeyCode::Char('u') if ctrl => scroll = scroll.saturating_sub(20),
                    KeyCode::Home | KeyCode::Char('g') => scroll = 0,
                    KeyCode::End | KeyCode::Char('G') => scroll = max,
                    _ => {}
                }
                self.modal = Some(Modal::Text { title, lines, scroll, watch });
            }
            Modal::Input { title, mut input, mut cursor, mut on_done } => match k.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    if let Some(f) = on_done.take() {
                        f(self, input.trim().to_string());
                    }
                }
                _ => {
                    edit_line(&mut input, &mut cursor, k);
                    self.modal = Some(Modal::Input { title, input, cursor, on_done });
                }
            },
            Modal::Palette { mut input, mut selected, mut filtering } => {
                let entries = self.palette_entries(&input);
                if let Some(to) = move_key(k, filtering, selected, entries.len()) {
                    selected = to;
                } else {
                    match k.code {
                        KeyCode::Esc if !filtering => return,
                        KeyCode::Esc => filtering = false, // the filter text and the selection stay
                        KeyCode::Enter => {
                            if let Some((_, f)) = entries.into_iter().nth(selected) {
                                f(self);
                            }
                            return;
                        }
                        KeyCode::Backspace => {
                            input.pop();
                            selected = 0;
                        }
                        KeyCode::Char('/') if !filtering && !ctrl => filtering = true,
                        KeyCode::Char(c) if filtering && !ctrl => {
                            input.push(c);
                            selected = 0;
                        }
                        _ => {}
                    }
                }
                self.modal = Some(Modal::Palette { input, selected, filtering });
            }
        }
    }

    /// Command palette: screens, server profiles and global commands, filtered by text.
    pub fn palette_entries(&self, filter: &str) -> Vec<PaletteEntry> {
        let mut out: Vec<PaletteEntry> = vec![];
        out.push((
            "Go: Dashboard".into(),
            Box::new(|a: &mut App| {
                a.history.clear();
                a.open(Screen::Dashboard);
            }),
        ));
        for (_, label, k) in kinds::NAV {
            let k = k.clone();
            out.push((
                format!("Go: {label}"),
                Box::new(move |a: &mut App| {
                    a.history.clear();
                    a.open(Screen::List(k));
                }),
            ));
        }
        out.push(("Go: Help".into(), Box::new(|a: &mut App| a.text("Help", help_lines()))));
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

    /// The connected client's flag; without a connection (connecting, or it failed) the profile's: the
    /// Content View screens still write to the store then.
    pub fn read_only(&self) -> bool {
        match &self.conn {
            Some(c) => c.client.read_only(),
            None => self.profile.as_ref().and_then(|p| self.cfg.profiles.get(p)).is_some_and(|p| p.read_only),
        }
    }
}

/// Sort order of two cells: numbers first, by value, then text. A total order: `sort_by` panics without one.
fn cell_cmp(x: &str, y: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::{Greater, Less};
    match (x.parse::<f64>(), y.parse::<f64>()) {
        (Ok(p), Ok(q)) => p.total_cmp(&q),
        (Ok(_), Err(_)) => Less,
        (Err(_), Ok(_)) => Greater,
        _ => x.to_lowercase().cmp(&y.to_lowercase()),
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
        // Edit applies the URL at the next Connect: the client still talks to the old server, so the scope stays.
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

    #[test]
    fn mixed_cells_sort_without_a_panic() {
        // Numbers and text mixed in one column (versions) made a cycle: 9 < 10 < "2.0-1" < "9".
        let base = ["9", "10", "2.0-1", "1.2-3", "100", "3", "1:2.0", "nan", "inf", "7.1", "Beta", "alpha"];
        let mut v: Vec<&str> = (0..300).map(|i| base[(i * 7) % base.len()]).collect();
        v.sort_by(|a, b| cell_cmp(a, b));
        v.dedup();
        assert_eq!(v, ["3", "7.1", "9", "10", "100", "inf", "nan", "1.2-3", "1:2.0", "2.0-1", "alpha", "Beta"]);
    }

    #[tokio::test]
    async fn read_only_holds_without_a_connection() {
        let dir = tempfile::tempdir().unwrap();
        let st = external_store(&dir).await;
        let local = Arc::new(Store::open_in_memory().await.unwrap());
        let mut cfg = lab("https://pulp.example.com");
        cfg.profiles.get_mut("lab").unwrap().read_only = true;
        let mut app = App::new(cfg, "/dev/null".into(), local, Ok(st));
        app.profile = Some("lab".into()); // the Pulp connection failed or still runs: no `conn`
        app.list = ListState::new(Kind::ContentViews);
        assert!(app.read_only() && app.shared());
        assert!(!app.available_actions(&Value::Null).contains(&Act::Create));
        super::super::actions::run(&mut app, Act::Create, Value::Null);
        assert!(app.modal.is_none() && app.notices.iter().any(|n| n.text.contains("read-only mode")));
    }

    /// The list keys of the help: move, page, sort, mark, search, other type, Esc, Enter and an action key.
    #[tokio::test]
    async fn list_keys_move_page_sort_mark_and_go_back() {
        let mut app = app_on(Kind::PulpExporters).await;
        app.list.items = (0..3).map(|i| serde_json::json!({ "pulp_href": format!("/e/{i}/"), "name": format!("e{i}") })).collect();
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let moves = [
            (ch('j'), 1),
            (key(KeyCode::Down), 2),
            (ch('j'), 2),
            (ch('k'), 1),
            (key(KeyCode::Up), 0),
            (ch('k'), 0),
            (ctrl('d'), 2),
            (ctrl('u'), 0),
            (ch('G'), 2),
            (ch('g'), 0),
            (key(KeyCode::End), 2),
            (key(KeyCode::Home), 0),
        ];
        for (k, at) in moves {
            app.handle_key(k);
            assert_eq!(app.list.selected, at, "{k:?}");
        }
        // Space marks and moves on; on a marked row it unmarks.
        app.handle_key(ch(' '));
        app.handle_key(ch(' '));
        app.handle_key(ch('k'));
        app.handle_key(ch(' '));
        assert_eq!((app.list.marked.iter().map(String::as_str).collect::<Vec<_>>(), app.list.selected), (vec!["/e/0/"], 2));
        app.handle_key(ch(' '));
        assert_eq!((app.list.marked.len(), app.list.selected), (2, 2), "the last row stays selected");
        // Sort: next column, then the same column reversed.
        for (k, sort) in [(ch('s'), (0, false)), (ch('s'), (1, false)), (ch('S'), (1, true))] {
            app.handle_key(k);
            assert_eq!(app.list.sort, Some(sort));
        }
        // Pages of the server's list (300 rows): n while there is a next page, p back; a new page starts at its
        // first row.
        app.list.count = 300;
        let pages = [(ch('n'), 100, 0), (ch('n'), 200, 0), (ch('n'), 200, 2), (ch('p'), 100, 0), (ch('p'), 0, 0), (ch('p'), 0, 2)];
        for (k, offset, at) in pages.into_iter().chain([(ch('r'), 0, 2)]) {
            app.list.selected = 2;
            app.handle_key(k);
            assert_eq!((app.list.offset, app.list.selected), (offset, at), "{k:?}");
        }
        app.list.offset = 1;
        app.handle_key(ch('p'));
        assert_eq!(app.list.offset, 0, "p from any offset goes back to the first page");
        app.handle_key(ch('/'));
        assert!(matches!(app.modal.take(), Some(Modal::Input { .. })), "search");
        app.handle_key(key(KeyCode::Esc));
        assert!(app.list.marked.is_empty(), "Esc clears the marks first");
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.modal.take(), Some(Modal::Menu { .. })), "Enter: the action menu");
        app.handle_key(ch('c'));
        assert!(app.modal.is_none(), "c: no Create without a connection (F98)");
        assert_eq!(app.notices.back().map(|n| n.text.as_str()), Some("not connected: select a server profile in Servers"));
        app.handle_key(ch('t'));
        assert_eq!(app.list.kind, Kind::FsExporters, "t: the other exporter type");
        app.push(Kind::PulpExporters);
        app.handle_key(ch('t'));
        assert_eq!((app.list.kind.clone(), app.history.len()), (Kind::FsExporters, 1), "a sub-list is replaced, not stacked");
        app.handle_key(ch('h'));
        assert_eq!(app.focus, Focus::Nav);
        app.focus = Focus::List;
        app.handle_key(key(KeyCode::Esc));
        assert!(app.history.is_empty(), "Esc goes back");
        let mut app = app_on(Kind::Publications(crate::pulp::RepoType::File)).await;
        app.handle_key(ch('/'));
        assert_eq!(app.notices.back().map(|n| n.text.as_str()), Some("this list has no search"));
        app.list.loading = true;
        app.handle_key(key(KeyCode::Esc));
        assert_eq!((app.list.loading, app.list.error.as_deref()), (false, Some("request cancelled")));
    }

    /// A screen the server does not support (`Kind::unsupported`) offers no actions; its key says why.
    #[tokio::test]
    async fn an_unsupported_screen_offers_no_create_and_says_why() {
        let server = pulp().await; // status without `domain_enabled: true`, no pulpcore version
        let local = Arc::new(Store::open_in_memory().await.unwrap());
        let mut app = App::new(lab(&server.uri()), "/dev/null".into(), local.clone(), Ok(local));
        let mut rx = app.rx.take().unwrap();
        app.connect("lab");
        settle(&mut app, &mut rx).await;
        app.open(Screen::List(Kind::Domains));
        settle(&mut app, &mut rx).await;
        let why = "not supported by this server: domains (domains/, with DOMAIN_ENABLED on the server)";
        assert_eq!(app.list.error.as_deref(), Some(why));
        assert_eq!(app.available_actions(&Value::Null), []);
        app.handle_key(ch('c'));
        assert!(app.modal.is_none() && app.jobs.is_empty());
        assert_eq!(app.notices.back().map(|n| (n.level, n.text.as_str())), Some((Level::Error, why)));
        let shown = app.notices.len();
        app.handle_key(ch('z'));
        assert_eq!(app.notices.len(), shown, "a key that is no action of the screen says nothing");
        // Upstream Pulps without native replication keep the fallback, which the message names.
        app.open(Screen::List(Kind::UpstreamPulps));
        settle(&mut app, &mut rx).await;
        assert_eq!(app.available_actions(&Value::Null), [Act::FallbackReplicate]);
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
        tokio::time::sleep(KEY_GUARD).await; // a job opened the question
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
        app.apply_pick(id, 0, "", Ok(vec![]));
        assert!(app.modal.is_none());
        // A different open form is not touched.
        let other = Form::new("t", vec![Field::pick("repo", "Repository", Source::Roles)]);
        app.form(other, |_, _| Ok(()));
        app.apply_pick(id, 0, "", Ok(vec![PickItem { label: "x".into(), value: "x".into(), note: String::new() }]));
        let Some(Modal::Form { form, .. }) = &app.modal else { panic!() };
        assert!(form.fields[0].options.is_none());
    }

    #[tokio::test]
    async fn object_options_for_an_old_role_are_dropped() {
        let mut app = app_on(Kind::Profiles).await;
        let fields = vec![Field::pick("role", "Role", Source::Roles).value("r1"), Field::pick("obj", "Object", Source::RoleObjects)];
        let form = Form::new("t", fields);
        let id = form.id;
        app.form(form, |_, _| Ok(()));
        let Some(Modal::Form { form, .. }) = &mut app.modal else { panic!() };
        form.fields[0].value = "r2".into(); // the role changed while the objects of r1 loaded
        let item = || vec![PickItem { label: "x".into(), value: "x".into(), note: String::new() }];
        app.apply_pick(id, 1, "r1", Ok(item()));
        let Some(Modal::Form { form, .. }) = &app.modal else { panic!() };
        assert!(form.fields[1].options.is_none());
        app.apply_pick(id, 1, "r2", Ok(item()));
        let Some(Modal::Form { form, .. }) = &app.modal else { panic!() };
        assert!(form.fields[1].options.is_some());
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
    async fn a_watch_from_a_job_waits_for_the_open_dialog() {
        let mut app = app_on(Kind::Tasks).await;
        app.input("Search", "half typ", |_, _| {});
        app.applying = true; // the result of a job (CvVersions W) runs
        app.watch_task("/t/1/", "copy", None);
        app.applying = false;
        assert!(matches!(&app.modal, Some(Modal::Input { .. })), "the open dialog keeps its text");
        assert_eq!(app.queued.len(), 1);
    }

    #[tokio::test]
    async fn watch_follows_task_updates() {
        let mut app = app_on(Kind::Tasks).await;
        app.watch_task("/t/1/", "copy", None);
        app.watch_task("/t/1/", "copy", None);
        assert_eq!(app.tasks.items.len(), 1);
        assert!(matches!(&app.modal, Some(Modal::Text { watch: Some(h), .. }) if h == "/t/1/"));
        let task = |state: &str| {
            serde_json::from_value::<Task>(serde_json::json!({ "pulp_href": "/t/1/", "state": state, "name": "copy",
                "progress_reports": [{ "message": "Associating Content", "code": "c", "state": state, "total": 9, "done": 3 }] }))
            .unwrap()
        };
        app.handle_msg(Msg::TaskPolled { href: "/t/1/".into(), result: Ok(task("running")) });
        assert!(text_lines(&app).contains(&"  [running] Associating Content 3/9".to_string()), "{:?}", text_lines(&app));
        app.handle_msg(Msg::TaskPolled { href: "/t/1/".into(), result: Ok(task("completed")) });
        assert!(text_lines(&app).contains(&"state: completed".to_string()), "{:?}", text_lines(&app));
    }

    #[tokio::test]
    async fn task_rows_keep_the_selection_and_ignore_other_servers() {
        let mut app = app_on(Kind::Tasks).await;
        let task =
            |h: &str| serde_json::from_value::<Task>(serde_json::json!({ "pulp_href": h, "state": "running", "name": "t" })).unwrap();
        app.track_task("/t/1/", "a", None);
        app.reload();
        assert_eq!(app.list.current().unwrap()["pulp_href"], "/t/1/");
        // A workflow reports a new task: it goes on top, and the cursor stays on /t/1/ (Cancel keeps its target).
        app.handle_msg(Msg::Task(task("/t/2/"), None));
        assert_eq!((app.list.items.len(), app.list.current().unwrap()["pulp_href"].as_str()), (2, Some("/t/1/")));
        // A poll answer sent to the previous server before a Connect is dropped.
        app.handle_msg(Msg::TaskPolled { href: "/t/9/".into(), result: Ok(task("/t/9/")) });
        assert_eq!(app.tasks.items.len(), 2);
        // A task the server does not have is final: no more polls, not re-attached at the next start.
        let gone = crate::pulp::PulpError::Api { status: 404, message: String::new() };
        app.handle_msg(Msg::TaskPolled { href: "/t/1/".into(), result: Err(gone) });
        assert_eq!(app.tasks.items.iter().find(|t| t.href == "/t/1/").unwrap().state, super::super::tasks::GONE);
        assert_eq!(app.tasks.active(), 1);
    }

    #[tokio::test]
    async fn a_job_dialog_waits_for_the_open_one_and_ignores_early_keys() {
        let mut app = app_on(Kind::Profiles).await;
        let mut rx = app.rx.take().unwrap();
        app.form(Form::new("typing", vec![super::super::form::Field::text("name", "Name")]), |_, _| Ok(()));
        app.spawn("rollback preview".into(), false, async { ap(|app| app.confirm("Roll back", vec![], |app| app.quit = true)) });
        settle(&mut app, &mut rx).await;
        app.handle_key(ch('y'));
        assert!(matches!(app.modal, Some(Modal::Form { .. })) && !app.quit, "the key belongs to the form");
        assert_eq!(app.queued.len(), 1);
        app.handle_key(key(KeyCode::Esc)); // ends editing the field
        app.handle_key(key(KeyCode::Esc)); // closes the form
        assert!(matches!(&app.modal, Some(Modal::Confirm { title, .. }) if title == "Roll back"));
        app.handle_key(ch('y'));
        assert!(!app.quit, "keys right after it opens are dropped");
        tokio::time::sleep(KEY_GUARD).await;
        app.handle_key(ch('y'));
        assert!(app.quit && app.queued.is_empty());
    }

    #[tokio::test]
    async fn a_failed_operation_reloads_the_list_and_a_failed_read_does_not() {
        let mut app = app_on(Kind::Profiles).await;
        let mut rx = app.rx.take().unwrap();
        let loads = |app: &App| app.jobs.iter().filter(|j| j.label == "load list").count();
        app.spawn("read x".into(), true, async { anyhow::bail!("gone") });
        let m = rx.recv().await.unwrap();
        app.handle_msg(m);
        assert_eq!(loads(&app), 0, "a failed read (also a failed list load) does not reload: no loop");
        app.spawn("delete x".into(), false, async { anyhow::bail!("refused") });
        let m = rx.recv().await.unwrap();
        app.handle_msg(m);
        assert_eq!(loads(&app), 1, "the list shows what the failed operation left");
    }

    /// A task group whose tasks were purged ends `gone` in the Task Center: final, stored so, a warning, never a success.
    #[tokio::test]
    async fn a_task_group_whose_tasks_were_purged_ends_gone() {
        use super::super::tasks::GONE;
        let mut app = app_on(Kind::Profiles).await;
        let mut rx = app.rx.take().unwrap();
        app.track_task("/g/1/", "import dvd", None);
        let group = serde_json::json!({ "pulp_href": "/g/1/", "description": "Import of /x-toc.json", "tasks": [] });
        let task = serde_json::from_value::<crate::pulp::models::TaskGroup>(group).unwrap().as_task();
        app.handle_msg(Msg::TaskPolled { href: "/g/1/".into(), result: Ok(task) });
        settle(&mut app, &mut rx).await;
        let row = app.tasks.items.iter().find(|t| t.href == "/g/1/").unwrap();
        assert!(row.is_final() && row.state == GONE, "{}", row.state);
        let notices: Vec<(Level, &str)> = app.notices.iter().map(|n| (n.level, n.text.as_str())).collect();
        assert_eq!(notices, [(Level::Warn, "task gone: import dvd: its tasks were purged")]);
        assert_eq!(app.store.tasks("lab", 10).await.unwrap()[0].state, GONE, "a restart does not re-attach it");
    }

    /// A notice shows one line: a job error of several lines (the failure list of an ImportRepos) also opens in the
    /// text view, one line each (audit F40).
    #[tokio::test]
    async fn a_job_error_of_several_lines_opens_in_the_text_view() {
        let mut app = app_on(Kind::Profiles).await;
        let mut rx = app.rx.take().unwrap();
        let lines = ["1 not created (created 1, kept 0):", "  RPM remote os: bad", "warning: RPM remote os: set password again"];
        let error = lines.join("\n");
        app.spawn("import repos.json".into(), false, async move { anyhow::bail!(error) });
        app.handle_msg(rx.recv().await.unwrap());
        assert!(matches!(&app.modal, Some(Modal::Text { title, .. }) if title == "import repos.json failed"), "no text view");
        assert_eq!(text_lines(&app), lines);
        assert!(app.notices.back().is_some_and(|n| n.level == Level::Error && n.text.starts_with("import repos.json: 1 not created")));
        app.modal = None;
        app.spawn("delete x".into(), false, async { anyhow::bail!("refused") });
        app.handle_msg(rx.recv().await.unwrap());
        assert!(app.modal.is_none(), "one line: the notice only");
    }

    /// Audit F49: a read that fails with an error of several lines (a reload after each finished task, with a Pulp
    /// error body in plain text) gives one notice and no text view.
    #[tokio::test]
    async fn a_failed_read_of_several_lines_opens_no_text_view() {
        let mut app = app_on(Kind::Profiles).await;
        let mut rx = app.rx.take().unwrap();
        for _ in 0..2 {
            app.spawn("load dashboard".into(), true, async { anyhow::bail!("502 Bad Gateway\n<html>\n</html>") });
            app.handle_msg(rx.recv().await.unwrap());
            assert!(app.modal.is_none() && app.queued.is_empty(), "no text view");
        }
        assert_eq!(texts(&app), ["load dashboard: 502 Bad Gateway\n<html>\n</html>"; 2]);
    }

    /// Audit F47: the footer notice, its Logs row and the text view of an error never show the user info of a URL,
    /// of each URL in the text.
    #[tokio::test]
    async fn an_error_shows_no_user_info_of_a_url() {
        let mut app = app_on(Kind::Profiles).await;
        let mut rx = app.rx.take().unwrap();
        app.spawn("replicate f47".into(), false, async {
            anyhow::bail!("2 failed:\n  http://u:p@h/a/: 502\n  http://h/ and https://v@h2/")
        });
        app.handle_msg(rx.recv().await.unwrap());
        let masked = ["2 failed:", "  http://•••@h/a/: 502", "  http://h/ and https://•••@h2/"];
        assert_eq!(text_lines(&app), masked);
        app.error("refused f47: http://u:p@h/");
        let notices = [format!("replicate f47: {}", masked.join("\n")), "refused f47: http://•••@h/".into()];
        assert_eq!(texts(&app), notices);
        for n in notices {
            let id = row_with(&format!("[error] {n}"))["id"].as_u64().unwrap();
            assert_eq!(crate::session_log::detail(id), Some(vec![n]));
        }
    }

    /// Audit F47: Pulp's error of a task (pulp_rpm names the remote URL in a sync error) shows without user info in the
    /// notice, the Task Center row (its ERROR line) and task Inspect.
    #[tokio::test]
    async fn a_task_error_shows_no_user_info_of_a_url() {
        let mut app = app_on(Kind::Tasks).await;
        let error = serde_json::json!({ "description": "Cannot download http://u:p@h/repomd.xml", "traceback": "at https://v@h2/x" });
        let t = serde_json::json!({ "pulp_href": "/t/1/", "state": "failed", "name": "sync os", "error": error });
        app.handle_msg(Msg::Task(serde_json::from_value(t).unwrap(), None));
        assert_eq!(texts(&app), ["task failed: sync os: Cannot download http://•••@h/repomd.xml"]);
        let row = app.task_items().remove(0);
        assert_eq!(row["error"], "Cannot download http://•••@h/repomd.xml");
        let inspect = super::super::actions::task_lines(&row).join("\n");
        assert!(inspect.contains("http://•••@h/repomd.xml") && inspect.contains("https://•••@h2/x"), "{inspect}");
        assert!(!inspect.contains("u:p@") && !inspect.contains("v@h2"), "{inspect}");
    }

    /// The text view of a failed job waits behind dialogs only for itself: a dialog that the operator opens after it
    /// takes the keys typed at once.
    #[tokio::test]
    async fn after_a_job_error_view_a_dialog_of_the_operator_takes_keys_at_once() {
        let mut app = app_on(Kind::Profiles).await;
        let mut rx = app.rx.take().unwrap();
        app.spawn("replicate up".into(), false, async { anyhow::bail!("2 failed:\n  a\n  b") });
        app.handle_msg(rx.recv().await.unwrap());
        assert!(matches!(&app.modal, Some(Modal::Text { title, .. }) if title == "replicate up failed"));
        (app.modal, app.guard_until) = (None, None); // the operator read it and closed it
        app.form(Form::new("next", vec![super::super::form::Field::text("name", "Name")]), |_, _| Ok(()));
        app.handle_key(ch('x'));
        let Some(Modal::Form { form, .. }) = &app.modal else { panic!("no form") };
        assert_eq!(form.get("name"), "x");
    }

    /// The text view of a failed job never replaces the dialog that the operator fills in: it waits behind it.
    #[tokio::test]
    async fn a_job_error_view_waits_for_the_open_dialog() {
        let mut app = app_on(Kind::Profiles).await;
        let mut rx = app.rx.take().unwrap();
        app.form(Form::new("typing", vec![super::super::form::Field::text("name", "Name")]), |_, _| Ok(()));
        app.spawn("replicate up".into(), false, async { anyhow::bail!("2 failed:\n  a\n  b") });
        app.handle_msg(rx.recv().await.unwrap());
        assert!(matches!(app.modal, Some(Modal::Form { .. })), "the form stays open");
        assert_eq!(app.queued.len(), 1);
        app.handle_key(key(KeyCode::Esc)); // ends editing the field
        app.handle_key(key(KeyCode::Esc)); // closes the form
        assert!(matches!(&app.modal, Some(Modal::Text { title, .. }) if title == "replicate up failed"), "then the error view");
        assert!(app.guard_until.is_some(), "it ignores the keys typed before it opened");
    }

    #[tokio::test]
    async fn a_finished_task_says_how_it_ended_and_reloads_the_list() {
        let mut app = app_on(Kind::Profiles).await;
        let task = |n: u32, state: &str| {
            let t = serde_json::json!({ "pulp_href": format!("/t/{n}/"), "state": state, "name": "sync os", "error": { "description": "stop" } });
            Msg::Task(serde_json::from_value(t).unwrap(), None)
        };
        app.handle_msg(task(1, "running"));
        assert!(app.notices.is_empty() && app.list.generation == 0, "a running task: no notice, no reload");
        for (n, state, level, text) in [
            (2, "completed", Level::Ok, "task completed: sync os"),
            (3, "canceled", Level::Warn, "task canceled: sync os: stop"),
            (4, "skipped", Level::Warn, "task skipped: sync os: stop"),
            (5, "failed", Level::Error, "task failed: sync os: stop"),
        ] {
            let generation = app.list.generation;
            app.handle_msg(task(n, state));
            let last = app.notices.back().unwrap();
            assert_eq!((last.level, last.text.as_str()), (level, text));
            assert_eq!(app.list.generation, generation + 1, "{state}: the list reloads once");
        }
        app.screen = Screen::Dashboard;
        app.handle_msg(task(6, "completed"));
        assert_eq!(app.list.generation, 4, "the Dashboard reloads itself, not the list");
    }

    fn texts(app: &App) -> Vec<String> {
        app.notices.iter().map(|n| n.text.clone()).collect()
    }

    #[tokio::test]
    async fn tracking_an_ended_server_task_neither_notifies_nor_reloads() {
        let mut app = app_on(Kind::ServerTasks(vec![])).await;
        let row = |n: u32, state: &str| serde_json::json!({ "pulp_href": format!("/t/{n}/"), "name": "export", "state": state, "error": { "description": "disk full" } });
        super::super::actions::run(&mut app, Act::TrackTask, row(1, "failed"));
        super::super::actions::run(&mut app, Act::WatchTask, row(2, "canceled"));
        // Pulp's answer to each poll that is sent.
        for href in app.tasks.due() {
            let t = [row(1, "failed"), row(2, "canceled")].into_iter().find(|r| r["pulp_href"] == href.as_str()).unwrap();
            app.handle_msg(Msg::TaskPolled { href, result: Ok(serde_json::from_value(t).unwrap()) });
        }
        assert_eq!(texts(&app), ["tracking task"], "the tasks ended before: they did not just end");
        assert_eq!(app.list.generation, 0, "no reload");
        let rows: Vec<_> = app.tasks.items.iter().map(|t| (t.href.as_str(), t.state.as_str())).collect();
        assert_eq!(rows, [("/t/2/", "canceled"), ("/t/1/", "failed")]);
        assert!(app.tasks.due().is_empty(), "an ended task is never polled");
        assert!(text_lines(&app).contains(&"state: canceled".to_string()), "{:?}", text_lines(&app));
        assert!(text_lines(&app).iter().any(|l| l.contains("disk full")), "Pulp's error: {:?}", text_lines(&app));
    }

    #[tokio::test]
    async fn an_ended_task_tracked_again_is_not_re_attached_as_unfinished() {
        let mut app = app_on(Kind::ServerTasks(vec![])).await;
        let mut rx = app.rx.take().unwrap();
        // Tracked and ended in an earlier session; no longer in the Task Center (it re-attaches the newest only).
        app.store.track_task("lab", "/t/1/", "export", "running").await.unwrap();
        app.store.finish_task("lab", "/t/1/", "failed").await.unwrap();
        for (h, state) in [("/t/1/", "failed"), ("/t/2/", "completed")] {
            super::super::actions::run(&mut app, Act::TrackTask, serde_json::json!({ "pulp_href": h, "name": "x", "state": state }));
        }
        settle(&mut app, &mut rx).await;
        // A restart or a Connect re-attaches the stored references.
        (app.tasks, app.notices) = (TaskCenter::new(app.cfg.settings.poll_bounds()), VecDeque::new());
        app.reattach_tasks();
        settle(&mut app, &mut rx).await;
        assert!(app.notices.is_empty(), "no unfinished task to re-attach: {:?}", texts(&app));
        let rows: Vec<_> = app.tasks.items.iter().map(|t| (t.href.as_str(), t.state.as_str())).collect();
        assert_eq!(rows.len(), 2);
        assert!(rows.contains(&("/t/1/", "failed")) && rows.contains(&("/t/2/", "completed")), "{rows:?}");
        assert!(app.tasks.due().is_empty(), "not polled again");
    }

    fn polled(h: &str, state: &str) -> Msg {
        let t = serde_json::json!({ "pulp_href": h, "name": "x", "state": state });
        Msg::TaskPolled { href: h.into(), result: Ok(serde_json::from_value(t).unwrap()) }
    }

    /// A restart or a Connect: a new Task Center that re-attaches the stored references.
    async fn restart(app: &mut App, rx: &mut UnboundedReceiver<Msg>) {
        (app.tasks, app.notices) = (TaskCenter::new(app.cfg.settings.poll_bounds()), VecDeque::new());
        app.reattach_tasks();
        settle(app, rx).await;
    }

    #[tokio::test]
    async fn dismissed_tasks_stay_dismissed_after_a_restart() {
        let mut app = app_on(Kind::Tasks).await;
        let mut rx = app.rx.take().unwrap();
        app.track_task("/t/1/", "sync", None);
        let failed = serde_json::from_value(serde_json::json!({ "pulp_href": "/t/2/", "name": "x", "state": "failed" })).unwrap();
        app.track_task("/t/2/", "export", Some(failed));
        app.track_task("/t/3/", "publish", None);
        app.handle_msg(polled("/t/1/", "completed"));
        super::super::actions::run(&mut app, Act::DismissTasks, Value::Null);
        settle(&mut app, &mut rx).await;
        restart(&mut app, &mut rx).await;
        let rows: Vec<_> = app.tasks.items.iter().map(|t| t.href.as_str()).collect();
        assert_eq!(rows, ["/t/3/"], "the dismissed tasks do not come back");
    }

    #[tokio::test]
    async fn a_connect_between_the_track_and_the_end_of_a_task_keeps_both_writes_in_order() {
        let mut app = app_on(Kind::Tasks).await;
        let mut rx = app.rx.take().unwrap();
        app.track_task("/t/1/", "sync", None);
        app.abort_all(); // a Connect stops the jobs
        app.handle_msg(polled("/t/1/", "completed"));
        app.abort_all();
        assert_eq!(texts(&app), ["task completed: sync"], "a store write is no operation: Connect does not warn");
        restart(&mut app, &mut rx).await;
        let refs = app.store.tasks("lab", 10).await.unwrap();
        assert_eq!(refs.iter().map(|r| (r.href.as_str(), r.state.as_str())).collect::<Vec<_>>(), [("/t/1/", "completed")]);
        assert!(app.notices.is_empty(), "not re-attached as unfinished: {:?}", texts(&app));
    }

    /// After a change of the own password every poll gets 401: polling stops with one notice, and the stored
    /// references stay unfinished, so a Connect with good credentials re-attaches them.
    #[tokio::test]
    async fn a_401_on_task_polls_stops_them_with_one_notice_and_a_connect_re_attaches_them() {
        let mut app = app_on(Kind::Tasks).await;
        let mut rx = app.rx.take().unwrap();
        app.track_task("/t/1/", "sync", None);
        app.track_task("/t/2/", "publish", None);
        settle(&mut app, &mut rx).await;
        for href in app.tasks.due() {
            let denied = crate::pulp::PulpError::Api { status: 401, message: "Invalid username/password.".into() };
            app.handle_msg(Msg::TaskPolled { href, result: Err(denied) });
            assert_eq!(app.notices.len(), 1, "the first 401 tells it, the next ones do not: {:?}", texts(&app));
        }
        let rows: Vec<_> = app.tasks.items.iter().map(|t| (t.href.as_str(), t.state.as_str())).collect();
        assert_eq!(rows, [("/t/2/", "unauthorized"), ("/t/1/", "unauthorized")], "a final state, visible");
        assert!(app.tasks.items.iter().all(|t| t.error.as_deref().is_some_and(|e| e.contains("Connect again"))));
        assert!(app.tasks.items.iter().all(|t| !t.in_flight), "a stopped poll frees its slot");
        assert_eq!(app.tasks.active(), 0);
        assert!(app.tasks.due().is_empty(), "no more polls");
        assert_eq!(app.notices.len(), 1, "one notice: {:?}", texts(&app));
        assert_eq!(app.notices[0].level, Level::Error);
        assert!(texts(&app)[0].contains("401") && texts(&app)[0].contains("Connect again"), "{:?}", texts(&app));
        settle(&mut app, &mut rx).await;
        restart(&mut app, &mut rx).await;
        assert_eq!(texts(&app), ["re-attached to 2 unfinished task(s) from a previous session"]);
        assert_eq!(app.tasks.due().len(), 2, "polled again");
    }

    #[tokio::test]
    async fn every_unfinished_task_is_re_attached() {
        let mut app = app_on(Kind::Tasks).await;
        let mut rx = app.rx.take().unwrap();
        for i in 0..120 {
            app.store.track_task("lab", &format!("/t/{i}/"), "sync", if i < 60 { "running" } else { "completed" }).await.unwrap();
        }
        restart(&mut app, &mut rx).await;
        assert_eq!(texts(&app), ["re-attached to 60 unfinished task(s) from a previous session"]);
        assert_eq!((app.tasks.active(), app.tasks.items.len()), (60, 110), "and the 50 newest finished tasks");
    }

    #[tokio::test]
    async fn a_tracked_running_task_and_a_submitted_task_notify_when_they_end() {
        let mut app = app_on(Kind::ServerTasks(vec![])).await;
        app.tasks = TaskCenter::new((Duration::ZERO, Duration::from_secs(60)));
        let poll = |h: &str, state: &str| {
            let t = serde_json::json!({ "pulp_href": h, "name": "pulpcore.app.tasks.base.general_delete", "state": state });
            Msg::TaskPolled { href: h.into(), result: Ok(serde_json::from_value(t).unwrap()) }
        };
        super::super::actions::run(
            &mut app,
            Act::TrackTask,
            serde_json::json!({ "pulp_href": "/t/1/", "name": "sync", "state": "running" }),
        );
        app.track_task("/t/2/", "delete x", None); // as `submit` tracks the task of an answer 202
        assert_eq!(app.tasks.due(), ["/t/2/", "/t/1/"], "the Task Center polls both");
        app.handle_msg(poll("/t/1/", "running"));
        assert_eq!((texts(&app).len(), app.list.generation), (1, 0), "still running: no notice, no reload");
        app.handle_msg(poll("/t/1/", "completed"));
        app.handle_msg(poll("/t/2/", "completed")); // final at its first poll
        assert_eq!(texts(&app), ["tracking task", "task completed: sync", "task completed: delete x"]);
        assert_eq!(app.list.generation, 2, "a reload per task that ended");
    }

    #[tokio::test]
    async fn a_final_task_row_never_goes_back_to_running() {
        let mut app = app_on(Kind::ServerTasks(vec![])).await;
        let snapshot =
            |state: &str| serde_json::json!({ "pulp_href": "/t/1/", "name": "pulpcore.app.tasks.base.general_update", "state": state });
        let row = |app: &App| app.tasks.items.iter().find(|t| t.href == "/t/1/").map(|t| (t.state.clone(), t.label.clone()));
        app.track_task("/t/1/", "update repository web", None);
        app.handle_msg(Msg::Task(serde_json::from_value(snapshot("completed")).unwrap(), None));
        // Track on a Server tasks row that was loaded while the task ran.
        super::super::actions::run(&mut app, Act::TrackTask, snapshot("running"));
        assert_eq!(row(&app), Some(("completed".into(), "update repository web".into())));
        // A poll answer that was in flight when the workflow reported the end.
        app.handle_msg(Msg::TaskPolled { href: "/t/1/".into(), result: Ok(serde_json::from_value(snapshot("running")).unwrap()) });
        app.handle_msg(Msg::TaskPolled { href: "/t/1/".into(), result: Ok(serde_json::from_value(snapshot("completed")).unwrap()) });
        assert_eq!(row(&app), Some(("completed".into(), "update repository web".into())));
        assert_eq!(texts(&app), ["task completed: update repository web", "tracking task"], "one completed notice");
        assert!(app.tasks.due().is_empty(), "a final row is not polled");
    }

    #[tokio::test]
    async fn a_finished_task_reloads_the_dashboard_counts_once() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let ok = |body: Value| ResponseTemplate::new(200).set_body_json(body);
        let count = |n: u64| ok(serde_json::json!({ "count": n, "next": null, "results": [] }));
        let status = ok(serde_json::json!({ "versions": [] }));
        Mock::given(method("GET")).and(path("/pulp/api/v3/status/")).respond_with(status).mount(&server).await;
        let repos = || path("/pulp/api/v3/repositories/rpm/rpm/");
        Mock::given(method("GET")).and(repos()).respond_with(count(7)).up_to_n_times(1).mount(&server).await;
        Mock::given(method("GET")).and(repos()).respond_with(count(8)).mount(&server).await;
        let mut app = on_rpm_repos(&server).await;
        let mut rx = app.rx.take().unwrap();
        let line = |app: &App| app.dashboard.iter().find(|l| l.starts_with("  RPM repositories")).cloned().unwrap_or_default();
        let loads = |app: &App| app.jobs.iter().filter(|j| j.label == "load dashboard").count();
        let task = |state: &str| {
            Msg::Task(serde_json::from_value(serde_json::json!({ "pulp_href": "/t/1/", "state": state, "name": "sync" })).unwrap(), None)
        };
        app.open(Screen::Dashboard);
        settle(&mut app, &mut rx).await;
        assert!(line(&app).starts_with(&format!("  RPM repositories   {:>8}   ", 7)), "{:?}", app.dashboard);
        app.handle_msg(task("running"));
        assert_eq!(loads(&app), 0, "a running task: no reload");
        app.handle_msg(task("completed"));
        assert!(loads(&app) > 0, "the finished task reloads the Dashboard");
        settle(&mut app, &mut rx).await;
        assert!(line(&app).starts_with(&format!("  RPM repositories   {:>8}   ", 8)), "{:?}", app.dashboard);
        app.handle_msg(task("completed"));
        assert_eq!(loads(&app), 0, "once per finished task, not on every report");
    }

    #[tokio::test]
    async fn connect_asks_first_while_operations_run() {
        let st = Arc::new(Store::open_in_memory().await.unwrap());
        let mut app = App::new(lab("http://127.0.0.1:9"), "/dev/null".into(), st.clone(), Ok(st));
        app.spawn("publish web".into(), false, std::future::pending());
        app.connect("lab");
        assert!(matches!(&app.modal, Some(Modal::Confirm { title, lines, .. })
            if title == "Connect to lab?" && lines.contains(&"  publish web".to_string())));
        assert_eq!((app.jobs.len(), app.profile.as_deref()), (1, None), "nothing stopped before the answer");
        let Some(Modal::Confirm { on_yes, .. }) = app.modal.take() else { unreachable!() };
        on_yes.unwrap()(&mut app);
        assert!(app.notices.iter().any(|n| n.text.starts_with("stopped waiting for 'publish web'")));
        assert_eq!(app.profile.as_deref(), Some("lab"));
    }

    #[tokio::test]
    async fn tasks_that_a_workflow_reports_are_stored_for_the_next_start() {
        let mut app = app_on(Kind::Profiles).await;
        let mut rx = app.rx.take().unwrap();
        let task = |state: &str| {
            serde_json::from_value::<Task>(serde_json::json!({ "pulp_href": "/t/9/", "state": state, "name": "publish" })).unwrap()
        };
        app.handle_msg(Msg::Task(task("running"), None));
        settle(&mut app, &mut rx).await;
        let refs = app.store.tasks("lab", 10).await.unwrap();
        assert_eq!((refs.len(), refs[0].href.as_str(), refs[0].state.as_str()), (1, "/t/9/", "running"));
        app.handle_msg(Msg::Task(task("completed"), None));
        settle(&mut app, &mut rx).await;
        assert_eq!(app.store.tasks("lab", 10).await.unwrap()[0].state, "completed");
    }

    #[tokio::test]
    async fn re_attached_final_rows_show_the_start_time_of_pulp() {
        use wiremock::matchers::{method, path, query_param, query_param_contains};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let mut app = on_rpm_repos(&server).await;
        app.screen = Screen::List(Kind::Tasks);
        app.list = ListState::new(Kind::Tasks);
        let mut rx = app.rx.take().unwrap();
        for h in ["/t/1/", "/t/3/"] {
            app.store.track_task("lab", h, "sync os", "waiting").await.unwrap();
            app.store.finish_task("lab", h, "completed").await.unwrap();
        }
        app.store.track_task("lab", "/t/2/", "sync os", "running").await.unwrap();
        let refs = app.store.tasks("lab", 10).await.unwrap();
        let stored = |h: &str| refs.iter().find(|r| r.href == h).unwrap().created_at.clone();
        let started = |app: &App, h: &str| app.task_items().into_iter().find(|r| r["pulp_href"] == h).unwrap()["started"].clone();
        // The read fails (no answer mounted): the stored times stay, and no dialog or error notice opens.
        app.reattach_tasks();
        settle(&mut app, &mut rx).await;
        assert_eq!(started(&app, "/t/1/"), stored("/t/1/").as_str());
        assert!(app.modal.is_none() && !app.notices.iter().any(|n| n.level == Level::Error));
        // One list read for the final rows; /t/3/ was purged, so Pulp does not list it.
        let page = serde_json::json!({ "count": 1, "next": null,
            "results": [{ "pulp_href": "/t/1/", "started_at": "2026-01-02T03:04:05Z" }] });
        Mock::given(method("GET"))
            .and(path("/pulp/api/v3/tasks/"))
            .and(query_param_contains("pulp_href__in", "/t/1/"))
            .and(query_param_contains("pulp_href__in", "/t/3/"))
            .and(query_param("fields", "pulp_href,started_at"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page))
            .expect(1)
            .mount(&server)
            .await;
        app.reattach_tasks();
        settle(&mut app, &mut rx).await;
        assert_eq!(started(&app, "/t/1/"), "2026-01-02T03:04:05Z", "a final row is never polled: Pulp's started_at, read once");
        assert_eq!(app.list.items.iter().find(|r| r["pulp_href"] == "/t/1/").unwrap()["started"], "2026-01-02T03:04:05Z");
        assert_eq!(started(&app, "/t/3/"), stored("/t/3/").as_str(), "a purged task keeps the stored time");
        let polled = serde_json::json!({ "pulp_href": "/t/2/", "state": "running", "name": "s", "started_at": "2026-01-02T03:04:05Z" });
        app.handle_msg(Msg::TaskPolled { href: "/t/2/".into(), result: Ok(serde_json::from_value(polled).unwrap()) });
        assert_eq!(started(&app, "/t/2/"), "2026-01-02T03:04:05Z", "once Pulp is read, its started_at");
    }

    #[tokio::test]
    async fn a_task_that_a_job_reports_has_the_label_of_the_job() {
        let server = wiremock::MockServer::start().await;
        let mut app = on_rpm_repos(&server).await;
        let mut rx = app.rx.take().unwrap();
        let task = |h: &str| {
            serde_json::from_value::<Task>(serde_json::json!({ "pulp_href": h, "state": "running",
                "name": "pulp_file.app.tasks.publishing.publish" }))
            .unwrap()
        };
        app.track_task("/t/1/", "sync web", None);
        app.job("publish web", move |ctx| async move {
            let on = ctx.on_task();
            on(&task("/t/1/"));
            on(&task("/t/2/"));
            ap(|_| {})
        });
        settle(&mut app, &mut rx).await;
        let label = |h: &str| app.tasks.items.iter().find(|t| t.href == h).map(|t| t.label.clone());
        assert_eq!(label("/t/2/").as_deref(), Some("publish web"), "a new task: the operation, not Pulp's task name");
        assert_eq!(label("/t/1/").as_deref(), Some("sync web"), "a tracked task keeps its label");
        let refs = app.store.tasks("lab", 10).await.unwrap();
        assert_eq!(refs.iter().find(|r| r.href == "/t/2/").map(|r| r.label.as_str()), Some("publish web"), "stored for the next start");
    }

    #[tokio::test]
    async fn task_messages_during_connect_are_dropped() {
        let mut app = app_on(Kind::Profiles).await;
        let mut rx = app.rx.take().unwrap();
        app.connecting = true;
        let task = serde_json::json!({ "pulp_href": "/t/9/", "state": "running", "name": "publish" });
        app.handle_msg(Msg::Task(serde_json::from_value(task).unwrap(), None));
        settle(&mut app, &mut rx).await;
        assert!(app.tasks.items.is_empty());
        assert!(app.store.tasks("lab", 10).await.unwrap().is_empty(), "nothing stored for the new profile");
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

    /// `app_on` RPM Repositories, connected to `server` (a Pulp with the RPM plugin).
    async fn on_rpm_repos(server: &wiremock::MockServer) -> App {
        let mut app = app_on(Kind::Repos(crate::pulp::RepoType::Rpm, false)).await;
        let p = Profile { url: server.uri(), auth: AuthMethod::None, ..Default::default() };
        let client = PulpClient::new("lab", &p, &app.cfg.settings).unwrap();
        let caps = Capabilities { versions: [("rpm".into(), "3.39.3".into())].into(), ..Default::default() };
        app.conn = Some(Conn { client, caps: Arc::new(caps), scope: "lab".into() });
        app
    }

    /// Audit F47: the error line of a failed list load and a failed task poll show Pulp's error text without the user
    /// info of a URL in it.
    #[tokio::test]
    async fn a_failed_load_or_poll_shows_no_user_info_of_a_url() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let refused = ResponseTemplate::new(400).set_body_json(serde_json::json!({ "detail": "proxy http://u:p@h/ refused" }));
        Mock::given(method("GET")).and(path("/pulp/api/v3/repositories/rpm/rpm/")).respond_with(refused).mount(&server).await;
        let mut app = on_rpm_repos(&server).await;
        let mut rx = app.rx.take().unwrap();
        app.reload();
        settle(&mut app, &mut rx).await;
        assert!(app.list.error.as_deref().is_some_and(|e| e.contains("proxy http://•••@h/ refused")), "{:?}", app.list.error);
        app.track_task("/t/1/", "sync", None);
        let failed = crate::pulp::PulpError::Api { status: 502, message: "proxy http://u:p@h/ refused".into() };
        app.handle_msg(Msg::TaskPolled { href: "/t/1/".into(), result: Err(failed) });
        let error = app.tasks.items[0].error.clone().unwrap_or_default();
        assert!(error.contains("proxy http://•••@h/ refused"), "{error}");
    }

    /// Audit F64: a picker load error, a text view (as a replication result makes it) and the job progress in the
    /// footer show no user info of a URL.
    #[tokio::test]
    async fn picker_errors_text_views_and_job_progress_show_no_user_info_of_a_url() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let refused = ResponseTemplate::new(400).set_body_json(serde_json::json!({ "detail": "proxy http://u:p@h/ refused" }));
        Mock::given(method("GET")).and(path("/pulp/api/v3/roles/")).respond_with(refused).mount(&server).await;
        let mut app = on_rpm_repos(&server).await;
        let mut rx = app.rx.take().unwrap();
        let draw = |app: &App| {
            let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
            t.draw(|f| super::super::view::draw(f, app)).unwrap();
            t.backend().to_string()
        };
        app.form(Form::new("Grant", vec![Field::pick("role", "Role", Source::Roles)]), |_, _| Ok(()));
        app.handle_key(key(KeyCode::Enter));
        settle(&mut app, &mut rx).await;
        let s = draw(&app);
        assert!(s.contains("Could not load choices") && s.contains("http://•••@h/") && !s.contains("u:p@"), "{s}");
        app.modal = None;
        app.text("Replication of up FAILED", vec!["sync of http://u:p@h/ failed".into()]);
        let s = draw(&app);
        assert!(s.contains("sync of http://•••@h/ failed") && !s.contains("u:p@"), "{s}");
        app.modal = None;
        app.spawn("replicate up".into(), false, std::future::pending());
        let id = app.jobs.last().unwrap().id;
        app.handle_msg(Msg::JobProgress { id, text: "retry http://u:p@h/".into() });
        let s = draw(&app);
        assert!(s.contains("replicate up retry http://•••@h/") && !s.contains("u:p@"), "{s}");
    }

    #[tokio::test]
    async fn the_dashboard_head_shows_health_user_and_domains() {
        let server = wiremock::MockServer::start().await;
        let mut app = on_rpm_repos(&server).await;
        let status = serde_json::json!({ "versions": [], "database_connection": { "connected": true },
            "redis_connection": { "connected": true }, "domain_enabled": true });
        let caps = Arc::make_mut(&mut app.conn.as_mut().unwrap().caps);
        caps.set_status(serde_json::from_value(status).unwrap());
        caps.user = Some(crate::pulp::caps::CurrentUser { href: String::new(), username: "admin".into(), is_staff: Some(true) });
        app.open(Screen::Dashboard);
        for want in [
            "User          admin (staff/admin)",
            "Health        database: ok   redis: ok   workers: 0   content apps: 0",
            "Domains       enabled",
        ] {
            assert!(app.dashboard.iter().any(|l| l == want), "{want:?} in {:?}", app.dashboard);
        }
    }

    #[tokio::test]
    async fn the_dashboard_shows_at_once_and_fills_in_the_counts() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let status = serde_json::json!({ "versions": [{ "component": "rpm", "version": "3.40.0" }] });
        let ok = |body: Value| ResponseTemplate::new(200).set_body_json(body);
        Mock::given(method("GET")).and(path("/pulp/api/v3/status/")).respond_with(ok(status)).mount(&server).await;
        let repos = serde_json::json!({ "count": 7, "next": null, "results": [] });
        Mock::given(method("GET")).and(path("/pulp/api/v3/repositories/rpm/rpm/")).respond_with(ok(repos)).mount(&server).await;
        let mut app = on_rpm_repos(&server).await;
        let mut rx = app.rx.take().unwrap();
        let line = |app: &App, start: &str| app.dashboard.iter().find(|l| l.starts_with(start)).cloned().unwrap_or_default();
        let seven = format!("  RPM repositories   {:>8}   ", 7);
        app.open(Screen::Dashboard);
        // Before any answer: what the connect detected, and placeholders for the counts.
        assert_eq!(line(&app, "  rpm"), format!("  {:<14} 3.39.3", "rpm"), "{:?}", app.dashboard);
        assert!(line(&app, "  RPM repositories").contains("..."), "{:?}", app.dashboard);
        settle(&mut app, &mut rx).await;
        assert_eq!(line(&app, "  rpm"), format!("  {:<14} 3.40.0", "rpm"), "status is read again");
        assert!(line(&app, "  RPM repositories").starts_with(&seven), "{:?}", app.dashboard);
        // A refresh keeps the values until the new ones come.
        app.reload();
        assert!(line(&app, "  RPM repositories").starts_with(&seven), "{:?}", app.dashboard);
        settle(&mut app, &mut rx).await;
        let paths: Vec<String> = server.received_requests().await.unwrap().iter().map(|r| r.url.path().to_string()).collect();
        assert!(!paths.iter().any(|p| p.contains("login/") || p.contains("docs/")), "capabilities detected again: {paths:?}");
    }

    #[tokio::test]
    async fn a_list_result_for_another_screen_is_dropped() {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        let repo = serde_json::json!({ "pulp_href": "/pulp/api/v3/repositories/rpm/rpm/1/", "name": "os" });
        let body = serde_json::json!({ "count": 1, "next": null, "results": [repo] });
        wiremock::Mock::given(method("GET"))
            .and(path("/pulp/api/v3/repositories/rpm/rpm/"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let mut app = on_rpm_repos(&server).await;
        let mut rx = app.rx.take().unwrap();
        app.reload();
        // Tasks opens before the load answers; it does not cancel loads.
        for h in ["/t/1/", "/t/2/", "/t/3/"] {
            app.tasks.track(h, "t", "running");
        }
        app.open(Screen::List(Kind::Tasks));
        app.list.selected = 2;
        settle(&mut app, &mut rx).await;
        // The repositories never replace the rows, so the cursor stays on its task (the rows are rebuilt after each message).
        assert_eq!(app.list.kind, Kind::Tasks);
        assert_eq!(app.list.items, app.task_items());
        assert_eq!(app.list.current().unwrap()["pulp_href"], "/t/1/");
    }

    #[tokio::test]
    async fn a_page_past_the_end_goes_back_to_the_last_page() {
        use wiremock::matchers::{method, path, query_param};
        let server = wiremock::MockServer::start().await;
        let repos: Vec<Value> = (0..100).map(|i| serde_json::json!({ "pulp_href": format!("/r/{i}/"), "name": format!("r{i}") })).collect();
        // The rows on the last page were deleted meanwhile.
        for (offset, results) in [("100", vec![]), ("0", repos)] {
            let body = serde_json::json!({ "count": 100, "next": null, "results": results });
            wiremock::Mock::given(method("GET"))
                .and(path("/pulp/api/v3/repositories/rpm/rpm/"))
                .and(query_param("offset", offset))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
        }
        let mut app = on_rpm_repos(&server).await;
        let mut rx = app.rx.take().unwrap();
        app.list.offset = 100;
        app.reload();
        settle(&mut app, &mut rx).await;
        assert_eq!((app.list.offset, app.list.items.len(), app.list.loading), (0, 100, false));
    }

    #[tokio::test]
    async fn a_page_past_the_end_goes_back_only_on_its_own_screen() {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        let body = serde_json::json!({ "count": 100, "next": null, "results": [] });
        wiremock::Mock::given(method("GET"))
            .and(path("/pulp/api/v3/repositories/rpm/rpm/"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let mut app = on_rpm_repos(&server).await;
        let mut rx = app.rx.take().unwrap();
        app.list.offset = 100;
        app.reload();
        let m = rx.recv().await.unwrap();
        // The operator opens the Dashboard before the list result is handled.
        app.open(Screen::Dashboard);
        app.handle_msg(m);
        let jobs: Vec<&str> = app.jobs.iter().map(|j| j.label.as_str()).filter(|l| *l != "load dashboard").collect();
        let list_gets =
            server.received_requests().await.unwrap().iter().filter(|r| r.url.path() == "/pulp/api/v3/repositories/rpm/rpm/").count();
        assert_eq!((jobs, app.list.loading, list_gets), (vec![], false, 1));
    }

    #[tokio::test]
    async fn an_empty_first_page_does_not_reload() {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        let body = serde_json::json!({ "count": 0, "next": null, "results": [] });
        wiremock::Mock::given(method("GET"))
            .and(path("/pulp/api/v3/repositories/rpm/rpm/"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let mut app = on_rpm_repos(&server).await;
        let mut rx = app.rx.take().unwrap();
        app.reload();
        // One message, not `settle`: a reload here would repeat without end.
        let m = rx.recv().await.unwrap();
        app.handle_msg(m);
        let gets = server.received_requests().await.unwrap().len();
        assert_eq!((app.list.offset, app.jobs.len(), app.list.loading, gets), (0, 0, false, 1));
    }

    /// A profile "dead" on a server that answers 404 to everything: its Connect fails at once
    /// (a closed port fails too, but only after the read retries).
    async fn add_dead_profile(app: &mut App) -> wiremock::MockServer {
        let dead = wiremock::MockServer::start().await;
        app.cfg.profiles.insert("dead".into(), Profile { url: dead.uri(), auth: AuthMethod::None, ..Default::default() });
        dead
    }

    #[tokio::test]
    async fn a_failed_connect_drops_the_rows_of_the_previous_server() {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        let rows: Vec<Value> = (0..2).map(|i| serde_json::json!({ "pulp_href": format!("/r/{i}/"), "name": format!("r{i}") })).collect();
        let body = serde_json::json!({ "count": 2, "next": null, "results": rows });
        wiremock::Mock::given(method("GET"))
            .and(path("/pulp/api/v3/repositories/rpm/rpm/"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let mut app = on_rpm_repos(&server).await;
        let mut rx = app.rx.take().unwrap();
        app.reload();
        settle(&mut app, &mut rx).await;
        assert_eq!(app.list.items.len(), 2);
        app.list.marked.insert("/r/1/".into());
        app.list.selected = 1;
        let _dead = add_dead_profile(&mut app).await;
        app.connect("dead");
        assert!(app.list.items.is_empty() && app.list.marked.is_empty(), "the rows belong to the previous server");
        assert_eq!(app.list.error.as_deref(), Some("connecting..."));
        settle(&mut app, &mut rx).await;
        assert!(app.conn.is_none() && !app.connecting);
        assert!(app.notices.iter().any(|n| n.level == Level::Error && n.text.starts_with("connection failed: ")));
        assert_eq!((app.list.items.len(), app.list.marked.len(), app.list.selected, app.list.loading), (0, 0, 0, false));
        assert_eq!(app.list.error.as_deref(), Some("not connected: select a server profile in Servers"));
    }

    /// F98: from "Roles of user a8-x1" on server A, Connect to B. Pulp takes the target of a role assignment only from
    /// the URL, and users/2/ on B is another account. So the form of A closes, the sub-list goes, and while the client
    /// connects no action opens a form. B gets no write.
    #[tokio::test]
    async fn a_connect_leaves_the_forms_and_sub_lists_of_the_previous_server() {
        let (a, b) = (pulp().await, pulp().await);
        let mut app = on_rpm_repos(&a).await;
        app.cfg.profiles.insert("b".into(), Profile { url: b.uri(), auth: AuthMethod::None, ..Default::default() });
        let mut rx = app.rx.take().unwrap();
        app.push(Kind::UserRoles { user: "/pulp/api/v3/users/2/".into(), name: "a8-x1".into() });
        app.handle_key(ch('c'));
        assert!(matches!(app.modal, Some(Modal::Form { .. })), "Assign role");
        app.connect("b");
        assert!(app.modal.is_none(), "the form of A is closed");
        assert_eq!(app.screen, Screen::Dashboard, "the roles of a user of A are no list of B");
        app.open(Screen::List(Kind::Users));
        app.handle_key(ch('c'));
        assert!(app.modal.is_none(), "no form before the connection");
        assert_eq!(app.notices.back().map(|n| (n.level, n.text.as_str())), Some((Level::Error, "connecting...")));
        settle(&mut app, &mut rx).await;
        assert_eq!(app.conn.as_ref().map(|c| c.client.profile()), Some("b"));
        let writes = b.received_requests().await.unwrap().into_iter().filter(|r| r.method.as_str() != "GET").count();
        assert_eq!(writes, 0);
    }

    /// F98: each sub-list of one object goes on Connect; a list of all objects stays, without its rows.
    #[tokio::test]
    async fn a_connect_leaves_every_sub_list_of_one_object() {
        let (href, name) = (String::from("/pulp/api/v3/x/1/"), String::from("x"));
        let subs = [
            Kind::Versions(Some((href.clone(), name.clone()))),
            Kind::Content { version: format!("{href}versions/1/"), ctype: 0 },
            Kind::CvVersions { cv: 1, name: name.clone() },
            Kind::Exports { exporter: href.clone(), name: name.clone() },
            Kind::Imports { importer: href.clone(), name: name.clone() },
            Kind::UserRoles { user: href.clone(), name: name.clone() },
            Kind::GroupUsers { group: href.clone(), name: name.clone() },
            Kind::GroupRoles { group: href.clone(), name: name.clone() },
        ];
        for (kind, stays) in subs.into_iter().map(|k| (k, false)).chain([(Kind::Versions(None), true), (Kind::Users, true)]) {
            let mut app = app_on(kind.clone()).await;
            let _dead = add_dead_profile(&mut app).await;
            app.connect("dead");
            let want = if stays { Screen::List(kind.clone()) } else { Screen::Dashboard };
            assert_eq!(app.screen, want, "{kind:?}");
        }
    }

    #[tokio::test]
    async fn a_connect_never_leaves_an_aborted_load_loading() {
        // Local lists keep their rows and cursor (Servers is where `o` connects).
        for kind in [Kind::Profiles, Kind::Logs { polls: false }] {
            let mut app = app_on(kind.clone()).await;
            let mut rx = app.rx.take().unwrap();
            let _dead = add_dead_profile(&mut app).await;
            app.cfg.profiles.insert("other".into(), Profile::default());
            for s in ["row-a", "row-b"] {
                crate::session_log::record("notice", "lab", s.into(), vec![]);
            }
            app.reload();
            settle(&mut app, &mut rx).await;
            app.list.selected = 1;
            // A Servers row has a name, a Logs row an id.
            let id = |v: &Value| (v["name"].clone(), v["id"].clone());
            let (rows, cursor) = (app.list.items.clone(), id(&app.list.items[1]));
            app.reload(); // still in flight when the Connect aborts it
            assert!(app.list.loading);
            app.connect("dead");
            assert!(!app.list.loading, "{kind:?}: the aborted load left the list loading");
            assert_eq!((&app.list.items, app.list.selected), (&rows, 1), "{kind:?}");
            settle(&mut app, &mut rx).await;
            assert!(!app.list.loading, "{kind:?}");
            assert_eq!(app.list.current().map(id), Some(cursor), "{kind:?}: the cursor stays on its row");
        }
    }

    #[tokio::test]
    async fn picker_loads_are_load_jobs() {
        let server = wiremock::MockServer::start().await;
        let mut app = on_rpm_repos(&server).await;
        app.load_pick(1, 0, Source::Roles, String::new());
        // Not "load ...": a list reload must not abort it.
        let job = app.jobs.last().unwrap();
        assert!(job.is_load && !job.label.starts_with("load "), "{} is_load={}", job.label, job.is_load);
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

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    /// A character key like crossterm delivers it: uppercase letters come with SHIFT.
    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), if c.is_uppercase() { KeyModifiers::SHIFT } else { KeyModifiers::NONE })
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn palette(app: &App) -> (String, usize, bool) {
        match &app.modal {
            Some(Modal::Palette { input, selected, filtering }) => (input.clone(), *selected, *filtering),
            _ => panic!("no palette"),
        }
    }

    #[tokio::test]
    async fn palette_opens_in_filter_mode_and_moves_without_arrows() {
        let mut app = app_on(Kind::Profiles).await;
        ":go".chars().for_each(|c| app.handle_key(ch(c)));
        assert_eq!(palette(&app), ("go".into(), 0, true));
        let keys = [ctrl('j'), ctrl('n'), ctrl('k'), ctrl('p'), key(KeyCode::Down), key(KeyCode::Up)];
        let seen: Vec<usize> = keys
            .into_iter()
            .map(|k| {
                app.handle_key(k);
                palette(&app).1
            })
            .collect();
        assert_eq!(seen, [1, 2, 1, 0, 1, 0]);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(palette(&app), ("go".into(), 0, false));
        app.handle_key(ch('j'));
        assert_eq!(palette(&app).1, 1);
        app.handle_key(ch('G'));
        assert_eq!(palette(&app).1, app.palette_entries("go").len() - 1);
        app.handle_key(ch('g'));
        assert_eq!(palette(&app).1, 0);
        "/x".chars().for_each(|c| app.handle_key(ch(c)));
        assert_eq!(palette(&app), ("gox".into(), 0, true));
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        app.handle_key(ctrl('p'));
        assert!(palette(&app).2, "Ctrl-P opens in filter mode too");
    }

    #[tokio::test]
    async fn palette_dashboard_clears_the_history() {
        let mut app = app_on(Kind::Profiles).await;
        app.open(Screen::List(Kind::Repos(crate::pulp::RepoType::Rpm, false)));
        app.push(Kind::Versions(Some(("/r/1/".into(), "os".into()))));
        let (label, go) = app.palette_entries("go: dashboard").into_iter().next().unwrap();
        assert_eq!(label, "Go: Dashboard");
        go(&mut app);
        assert!(app.history.is_empty());
        app.back();
        assert_eq!(app.screen, Screen::Dashboard);
    }

    #[tokio::test]
    async fn list_pages_with_ctrl_d_ctrl_u_and_ctrl_letters_never_run_actions() {
        let mut app = app_on(Kind::Profiles).await;
        app.cfg.profiles.insert("p0".into(), Profile::default());
        app.list.items = (0..40).map(|i| serde_json::json!({ "name": format!("p{i}") })).collect();
        app.handle_key(ctrl('d'));
        assert_eq!((app.list.selected, app.modal.is_none()), (15, true), "Ctrl-D pages, it is not Delete");
        app.handle_key(ctrl('u'));
        assert_eq!(app.list.selected, 0);
        app.handle_key(ctrl('e'));
        assert!(app.modal.is_none(), "Ctrl-E is not Edit");
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.list.selected, 15);
        app.handle_key(ch('g'));
        app.handle_key(ch('e'));
        assert!(matches!(app.modal, Some(Modal::Form { .. })), "plain e still edits");
    }

    #[tokio::test]
    #[allow(non_snake_case)]
    async fn menu_moves_with_g_and_G_and_ignores_ctrl_letters() {
        let mut app = app_on(Kind::Profiles).await;
        app.list.items = vec![serde_json::json!({ "name": "p0" })];
        app.handle_key(key(KeyCode::Enter));
        let menu = |app: &App| match &app.modal {
            Some(Modal::Menu { selected, items, .. }) => (*selected, items.len()),
            _ => panic!("no menu"),
        };
        app.handle_key(ch('G'));
        assert_eq!(menu(&app).0, menu(&app).1 - 1);
        app.handle_key(ch('g'));
        app.handle_key(ctrl('d'));
        assert_eq!(menu(&app).0, 0, "Ctrl-D is not Delete");
    }

    #[tokio::test]
    async fn text_and_confirm_scroll_with_ctrl_d_ctrl_u() {
        let mut app = app_on(Kind::Profiles).await;
        let lines: Vec<String> = (0..50).map(|i| format!("line {i}")).collect();
        let scroll = |app: &App| match &app.modal {
            Some(Modal::Text { scroll, .. }) | Some(Modal::Confirm { scroll, .. }) => *scroll,
            _ => panic!("no scrolling modal"),
        };
        app.text("t", lines.clone());
        for (k, want) in [(ctrl('d'), 20), (ctrl('u'), 0), (ch('j'), 1), (key(KeyCode::Down), 2)] {
            app.handle_key(k);
            assert_eq!(scroll(&app), want);
        }
        // Typed name: letters type (also j), Ctrl keys scroll and type nothing.
        app.confirm_danger("t", lines, "repo-j", |app| app.quit = true);
        "repo-j".chars().for_each(|c| app.handle_key(ch(c)));
        app.handle_key(ctrl('d'));
        let Some(Modal::Confirm { input, .. }) = &app.modal else { panic!() };
        assert_eq!((input.as_str(), scroll(&app)), ("repo-j", 20));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.quit);
    }

    #[tokio::test]
    #[allow(non_snake_case)]
    async fn confirm_goes_to_top_and_bottom_with_g_G_home_end_but_a_typed_name_keeps_the_letters() {
        let mut app = app_on(Kind::Profiles).await;
        let lines: Vec<String> = (0..50).map(|i| format!("line {i}")).collect();
        let scroll = |app: &App| match &app.modal {
            Some(Modal::Confirm { scroll, .. }) => *scroll,
            _ => panic!("no confirm"),
        };
        app.confirm("t", lines.clone(), |_| {});
        for (k, want) in [(ch('G'), 49), (ch('g'), 0), (key(KeyCode::End), 49), (key(KeyCode::Home), 0)] {
            app.handle_key(k);
            assert_eq!(scroll(&app), want);
        }
        app.confirm_danger("t", lines, "gG", |_| {});
        app.handle_key(key(KeyCode::End));
        assert_eq!(scroll(&app), 49);
        app.handle_key(ch('g'));
        app.handle_key(ch('G'));
        let Some(Modal::Confirm { input, .. }) = &app.modal else { panic!() };
        assert_eq!((input.as_str(), scroll(&app)), ("gG", 49), "letters type, they do not scroll");
    }

    #[tokio::test]
    async fn help_is_a_scrollable_text_view_that_reaches_its_last_line() {
        let last = help_lines().last().unwrap().trim().to_string();
        for (w, h) in [(80, 24), (100, 30)] {
            let mut app = app_on(Kind::Profiles).await;
            app.handle_key(ch('?'));
            assert!(matches!(&app.modal, Some(Modal::Text { title, .. }) if title == "Help"));
            let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            t.draw(|f| super::super::view::draw(f, &app)).unwrap();
            let s = t.backend().to_string();
            assert!(!s.contains(&last), "{w}x{h}: the last line is below the fold at first");
            assert!(s.contains("shown)"), "{w}x{h}: long lines wrap, they are not cut\n{s}");
            app.handle_key(ch('G'));
            t.draw(|f| super::super::view::draw(f, &app)).unwrap();
            let s = t.backend().to_string();
            assert!(s.contains(&last), "{w}x{h}\n{s}");
            app.handle_key(key(KeyCode::Esc));
            assert!(app.modal.is_none());
        }
    }

    #[tokio::test]
    #[allow(non_snake_case)]
    async fn nav_jumps_with_g_and_G() {
        let mut app = app_on(Kind::Profiles).await;
        app.focus = Focus::Nav;
        app.handle_key(ch('G'));
        assert_eq!(app.nav, kinds::NAV.len());
        app.handle_key(ch('g'));
        assert_eq!(app.nav, 0);
    }

    #[tokio::test]
    async fn screens_pane_has_groups_and_scrolls_to_the_cursor() {
        let mut app = app_on(Kind::Profiles).await;
        app.focus = Focus::Nav;
        let draw = |app: &App| {
            let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
            t.draw(|f| super::super::view::draw(f, app)).unwrap();
            t.backend().to_string()
        };
        // 30 rows (26 screens, 4 headers) in 19 lines.
        let s = draw(&app);
        assert!(s.contains("│ CONTENT ") && s.contains("│  File Repositories │") && s.contains("▼ 10 more"), "{s}");
        app.handle_key(ch('G'));
        let s = draw(&app);
        assert!(s.contains("▶ Logs") && s.contains("│ SYSTEM ") && s.contains("▲ 10") && !s.contains("▼"), "{s}");
    }

    #[tokio::test]
    async fn screens_pane_hides_screens_and_saves_the_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_on(Kind::Profiles).await;
        app.cfg_path = dir.path().join("config.toml");
        app.focus = Focus::Nav;
        let at = |l: &str| 1 + kinds::NAV.iter().position(|(_, n, _)| *n == l).unwrap();
        let saved = |app: &App| Config::load(&app.cfg_path).unwrap().settings.hidden_screens;
        app.nav = at("Deb Repositories");
        app.handle_key(ch('x'));
        assert_eq!(saved(&app), ["Deb Repositories"]);
        // The cursor leaves it, and then j and k skip it.
        app.handle_key(ch('k'));
        assert_eq!(app.nav, at("File Repositories"));
        app.handle_key(ch('j'));
        assert_eq!(app.nav, at("Remotes"));
        // `.` shows it, and `x` on it shows it again.
        app.handle_key(ch('.'));
        app.handle_key(ch('k'));
        assert_eq!(app.nav, at("Deb Repositories"));
        app.handle_key(ch('x'));
        assert!(saved(&app).is_empty());
        app.nav = at("Servers");
        app.handle_key(ch('x'));
        assert!(saved(&app).is_empty() && !app.nav_hidden(app.nav), "Servers cannot be hidden");

        // A hand-written list: any case; a group with no screen left loses its header.
        app.nav_all = false;
        app.cfg.settings.hidden_screens =
            ["tasks", "Task Schedules", "Uploads", "Artifacts", "Logs", "No Such Screen"].map(String::from).to_vec();
        app.handle_key(ch('G'));
        assert_eq!(app.nav, at("Access Policies"));
        let draw = |app: &App| {
            let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).unwrap();
            t.draw(|f| super::super::view::draw(f, app)).unwrap();
            t.backend().to_string()
        };
        let s = draw(&app);
        assert!(!s.contains("SYSTEM") && !s.contains("Uploads") && s.contains(" Screens "), "{s}");
        app.handle_key(ch('.'));
        let s = draw(&app);
        assert!(s.contains("│ SYSTEM ") && s.contains("│ ×Uploads ") && s.contains(" All screens "), "{s}");
    }

    #[tokio::test]
    async fn input_modal_ignores_ctrl_letters() {
        let mut app = app_on(Kind::Profiles).await;
        app.input("t", "ab", |_, _| {});
        app.handle_key(ctrl('j'));
        app.handle_key(ctrl('d'));
        app.handle_key(ch('c'));
        let Some(Modal::Input { input, .. }) = &app.modal else { panic!() };
        assert_eq!(input, "abc", "Ctrl letters type nothing");
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        app.handle_key(ch('X'));
        app.handle_key(ctrl('a'));
        app.handle_key(ch('>'));
        let Some(Modal::Input { input, .. }) = &app.modal else { panic!() };
        assert_eq!(input, ">abXc", "keys edit at the cursor");
    }
}
