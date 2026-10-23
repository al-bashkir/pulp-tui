//! Application state and event handling. Rendering never waits on I/O: every network or
//! disk operation runs as a background job that sends back a closure applied here.

use super::form::{FieldKind, Form, FormEvent, PickItem, Source};
use super::kinds::{self, Act, Kind};
use super::tasks::TaskCenter;
use crate::config::{Config, Settings};
use crate::cv::CvService;
use crate::pulp::models::Task;
use crate::pulp::{Capabilities, PulpClient};
use crate::store::Store;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;
use std::collections::{BTreeSet, VecDeque};
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
}

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
    pub store: Arc<Store>,
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
    pub fn cv(&self) -> CvService {
        CvService { client: self.client.clone(), store: self.store.clone(), prefix: self.settings.cv_base_path_prefix.clone() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Ok,
    Warn,
    Error,
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
    next_id: u64,
}

impl App {
    pub fn new(cfg: Config, cfg_path: PathBuf, store: Arc<Store>) -> Self {
        let (tx, rx) = unbounded_channel();
        let s = &cfg.settings;
        let tasks = TaskCenter::new(Duration::from_millis(s.poll_min_ms.max(100)), Duration::from_millis(s.poll_max_ms.max(s.poll_min_ms)));
        Self {
            cfg,
            cfg_path,
            store,
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
            settings: self.cfg.settings.clone(),
            job: 0,
            tx: self.tx.clone(),
        })
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
        if let Some(p) = &self.profile
            && let Err(e) = self.store.track_task(p, href, label, "waiting")
        {
            self.error(format!("cannot persist task reference: {e:#}"));
        }
    }

    fn task_update(&mut self, t: Task, driven: bool) {
        let href = t.pulp_href.clone();
        let state = t.state.clone();
        let err = t.error_message();
        if self.tasks.update(t, driven) {
            let label = self.tasks.items.iter().find(|x| x.href == href).map(|x| x.label.clone()).unwrap_or_default();
            if let Some(p) = &self.profile {
                let _ = self.store.finish_task(p, &href, &state);
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
                app.conn = Some(Conn { client, caps: Arc::new(caps) });
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
        match self.store.tasks(&p, 50) {
            Ok(refs) => {
                let mut n = 0;
                for r in refs.iter().rev() {
                    self.tasks.track(&r.href, &r.label, &r.state);
                    n += (!crate::pulp::models::TASK_FINAL_STATES.contains(&r.state.as_str())) as usize;
                }
                if n > 0 {
                    self.notify(Level::Info, format!("re-attached to {n} unfinished task(s) from a previous session"));
                }
            }
            Err(e) => self.error(format!("cannot load task references: {e:#}")),
        }
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
                let store = self.store.clone();
                let profile = self.profile.clone().unwrap_or_default();
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
                let fut = async move {
                    let (items, count) = kinds::load(&k, ctx.as_ref(), &store, &profile, &cfg, offset, &filter).await?;
                    let apply: Apply = Box::new(move |app: &mut App| {
                        if app.list.generation != generation {
                            return; // stale response
                        }
                        app.list.loading = false;
                        app.list.items = items;
                        app.list.count = count;
                        app.apply_sort();
                        app.list.selected = app.list.selected.min(app.list.items.len().saturating_sub(1));
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
        self.list
            .kind
            .actions()
            .into_iter()
            .filter(|a| !(ro && a.mutates(&self.list.kind)))
            .filter(|a| item.is_object() || !a.needs_item())
            .collect()
    }

    fn open_menu(&mut self) {
        let item = self.list.current().cloned().unwrap_or(Value::Null);
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
    use crate::tui::form::Field;

    #[test]
    fn load_result_for_closed_form_is_dropped() {
        let mut app = App::new(Config::default(), "/dev/null".into(), Arc::new(Store::open_in_memory().unwrap()));
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
}
