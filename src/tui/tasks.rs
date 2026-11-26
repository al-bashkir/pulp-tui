//! Task Center state. Pulp tasks are the source of truth; this only tracks references,
//! polls each unfinished task on its own adaptive schedule, and remembers results.

use crate::pulp::client::jitter as client_jitter;
use crate::pulp::models::Task;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Tracked {
    pub href: String,
    pub label: String,
    pub task: Option<Task>,
    pub state: String,
    pub error: Option<String>,
    /// When the stored reference was made; shown until Pulp's task is read (a re-attached final row never is).
    pub started: String,
    next_poll: Instant,
    interval: Duration,
    pub in_flight: bool,
    /// A workflow is polling this task itself; skip until this instant.
    driven_until: Option<Instant>,
}

/// State of a task the server does not have (purged, or a reference of another server), or of a task group whose
/// tasks were purged; final.
pub use crate::pulp::models::GONE;

/// State of a task whose poll got 401: final in this session only. The stored reference stays unfinished,
/// so a Connect with good credentials re-attaches the task.
pub const UNAUTHORIZED: &str = "unauthorized";

pub fn is_final_state(state: &str) -> bool {
    crate::pulp::models::TASK_FINAL_STATES.contains(&state) || state == GONE || state == UNAUTHORIZED
}

impl Tracked {
    pub fn is_final(&self) -> bool {
        is_final_state(&self.state)
    }

    /// Phase and numeric progress only when Pulp reports totals; never invented.
    pub fn progress(&self) -> String {
        let Some(t) = &self.task else { return String::new() };
        let r = t.progress_reports.iter().rev().find(|r| r.state == "running").or_else(|| t.progress_reports.last());
        match r {
            Some(r) => match (r.done, r.total) {
                (Some(d), Some(tot)) if tot > 0 => format!("{} {d}/{tot}", r.message),
                (Some(d), _) => format!("{} {d}", r.message),
                _ => r.message.clone(),
            },
            None => String::new(),
        }
    }
}

/// A client operation of many Pulp tasks (a Content View publish): one Task Center row that stays running until the
/// operation ends, while its Pulp tasks complete one by one. Not stored: a restart cannot re-attach a client
/// operation (Resume of the version continues it).
#[derive(Debug, Clone)]
pub struct Workflow {
    pub job: u64,
    pub label: String,
    pub started: String,
    pub flow: Flow,
}

/// The state of a `Workflow` with its text.
#[derive(Debug, Clone, PartialEq)]
pub enum Flow {
    /// The current step.
    Running(String),
    /// The notice of the end: all done (a publish: the version is active in Library).
    Completed(String),
    /// The warning notice of the end: done in part (Library does not serve the version, an auto publish failed).
    Warning(String),
    Failed(String),
    /// The client stopped waiting (Esc, Connect, quit) at this step; submitted Pulp tasks continue.
    Stopped(String),
}

impl Flow {
    pub fn state(&self) -> &'static str {
        match self {
            Flow::Running(_) => "running",
            Flow::Completed(_) => "completed",
            Flow::Warning(_) => "warning",
            Flow::Failed(_) => "failed",
            Flow::Stopped(_) => "stopped",
        }
    }
}

/// Finished tasks beyond this many are dropped, oldest first: every update rebuilds the rows of all tasks.
const MAX_TRACKED: usize = 500;

pub struct TaskCenter {
    /// Newest first.
    pub items: Vec<Tracked>,
    /// Newest first; shown above the Pulp tasks.
    pub workflows: Vec<Workflow>,
    min: Duration,
    max: Duration,
    max_in_flight: usize,
}

impl TaskCenter {
    /// `(min, max)` polling interval, as `Settings::poll_bounds` gives it.
    pub fn new((min, max): (Duration, Duration)) -> Self {
        Self { items: vec![], workflows: vec![], min, max, max_in_flight: 3 }
    }

    pub fn start_workflow(&mut self, job: u64, label: &str, started: String) {
        self.workflows.insert(0, Workflow { job, label: label.into(), started, flow: Flow::Running(String::new()) });
    }

    /// Set the current step of the running workflow of `job`. False when `job` has none.
    pub fn workflow_step(&mut self, job: u64, step: &str) -> bool {
        match self.workflows.iter_mut().find(|w| w.job == job) {
            Some(Workflow { flow: Flow::Running(s), .. }) => {
                *s = step.into();
                true
            }
            _ => false,
        }
    }

    /// End the running workflow of `job`, if it has one: `end` gets its current step.
    pub fn end_workflow(&mut self, job: u64, end: impl FnOnce(String) -> Flow) {
        if let Some(w) = self.workflows.iter_mut().find(|w| w.job == job)
            && let Flow::Running(s) = &w.flow
        {
            w.flow = end(s.clone());
        }
    }

    /// A row that exists keeps its label: the operation names it better than a later Track of Pulp's task name.
    pub fn track(&mut self, href: &str, label: &str, state: &str) {
        if self.items.iter().any(|t| t.href == href) {
            return;
        }
        self.items.insert(
            0,
            Tracked {
                href: href.into(),
                label: label.into(),
                task: None,
                state: state.into(),
                error: None,
                started: String::new(),
                next_poll: Instant::now(),
                interval: self.min,
                in_flight: false,
                driven_until: None,
            },
        );
        while self.items.len() > MAX_TRACKED
            && let Some(i) = self.items.iter().rposition(Tracked::is_final)
        {
            self.items.remove(i);
        }
    }

    /// Apply a task snapshot. Returns true when the task just reached a final state, also when
    /// a workflow reports it for the first time already final.
    pub fn update(&mut self, task: Task, driven: bool) -> bool {
        let now = Instant::now();
        self.track(&task.pulp_href, &task.name, "waiting");
        let (min, max) = (self.min, self.max);
        let Some(t) = self.items.iter_mut().find(|t| t.href == task.pulp_href) else { return false };
        let was_final = t.is_final();
        // An older snapshot (a stale Server tasks row, a late poll answer): a task never leaves a final state.
        if was_final && !is_final_state(&task.state) {
            return false;
        }
        let changed = t.task.as_ref() != Some(&task);
        t.state = task.state.clone();
        t.error = task.error_message();
        t.task = Some(task);
        t.in_flight = false;
        if driven {
            t.driven_until = Some(now + max * 2);
        }
        t.interval = if changed { min } else { (t.interval * 3 / 2).min(max) };
        t.next_poll = now + Duration::from_millis(client_jitter(t.interval.as_millis() as u64, 20));
        !was_final && t.is_final()
    }

    pub fn poll_failed(&mut self, href: &str, err: String) {
        let max = self.max;
        if let Some(t) = self.items.iter_mut().find(|t| t.href == href) {
            t.in_flight = false;
            t.error = Some(err);
            t.interval = (t.interval * 2).min(max);
            t.next_poll = Instant::now() + t.interval;
        }
    }

    /// The server cannot answer for this task (`GONE`, `UNAUTHORIZED`): mark it final with `why`, so it is no longer polled.
    pub fn stop(&mut self, href: &str, state: &str, why: &str) {
        if let Some(t) = self.items.iter_mut().find(|t| t.href == href) {
            t.in_flight = false;
            t.state = state.into();
            t.error = Some(why.into());
        }
    }

    /// Tasks due for a poll now (bounded); marks them in flight.
    pub fn due(&mut self) -> Vec<String> {
        let now = Instant::now();
        let mut budget = self.max_in_flight.saturating_sub(self.items.iter().filter(|t| t.in_flight).count());
        let mut out = vec![];
        for t in self.items.iter_mut() {
            if budget == 0 {
                break;
            }
            if t.is_final() || t.in_flight || t.next_poll > now || t.driven_until.is_some_and(|d| d > now) {
                continue;
            }
            t.in_flight = true;
            budget -= 1;
            out.push(t.href.clone());
        }
        out
    }

    /// Poll the task at the next tick, also when a workflow polls it itself (after a cancel request).
    pub fn poll_now(&mut self, href: &str) {
        if let Some(t) = self.items.iter_mut().find(|t| t.href == href) {
            t.driven_until = None;
            t.next_poll = Instant::now();
        }
    }

    pub fn dismiss_finished(&mut self) -> usize {
        let before = self.items.len() + self.workflows.len();
        self.items.retain(|t| !t.is_final());
        self.workflows.retain(|w| matches!(w.flow, Flow::Running(_)));
        before - self.items.len() - self.workflows.len()
    }

    pub fn active(&self) -> usize {
        self.items.iter().filter(|t| !t.is_final()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(href: &str, state: &str) -> Task {
        serde_json::from_value(serde_json::json!({ "pulp_href": href, "state": state, "name": "t" })).unwrap()
    }

    #[test]
    fn polling_is_bounded_and_adaptive() {
        let mut tc = TaskCenter::new((Duration::from_millis(0), Duration::from_millis(50)));
        for i in 0..5 {
            tc.track(&format!("/t/{i}/"), "x", "waiting");
        }
        let due = tc.due();
        assert_eq!(due.len(), 3, "max 3 polls in flight");
        assert!(tc.due().is_empty());
        assert!(!tc.update(task("/t/0/", "running"), false));
        assert!(tc.update(task("/t/1/", "completed"), false));
        assert_eq!(tc.active(), 4);
        // Driven tasks are not polled by the center.
        tc.update(task("/t/0/", "running"), true);
        std::thread::sleep(Duration::from_millis(5));
        assert!(!tc.due().contains(&"/t/0/".to_string()));
        assert_eq!(tc.dismiss_finished(), 1);
        tc.poll_failed("/t/2/", "network".into());
        assert_eq!(tc.items.iter().find(|t| t.href == "/t/2/").unwrap().error.as_deref(), Some("network"));
        tc.stop("/t/2/", GONE, "not found");
        assert_eq!(tc.active(), 3, "a task the server does not have is final");
        assert_eq!(tc.dismiss_finished(), 1);
        assert!(tc.update(task("/t/new/", "canceled"), true), "a workflow reports it final at once: it just ended");
    }

    #[test]
    fn finished_tasks_are_dropped_oldest_first_above_the_cap() {
        let mut tc = TaskCenter::new((Duration::ZERO, Duration::from_millis(50)));
        tc.track("/t/running/", "x", "running");
        for i in 0..MAX_TRACKED + 100 {
            tc.update(task(&format!("/t/{i}/"), "completed"), false);
        }
        assert_eq!(tc.items.len(), MAX_TRACKED);
        assert_eq!(tc.items[0].href, format!("/t/{}/", MAX_TRACKED + 99), "the newest stays");
        assert!(tc.items.iter().any(|t| t.href == "/t/running/"), "an unfinished task is never dropped");
        assert!(!tc.items.iter().any(|t| t.href == "/t/0/"));
    }

    #[test]
    fn progress_is_never_invented() {
        let mut t = Tracked {
            href: "h".into(),
            label: "l".into(),
            task: None,
            state: "running".into(),
            error: None,
            started: String::new(),
            next_poll: Instant::now(),
            interval: Duration::ZERO,
            in_flight: false,
            driven_until: None,
        };
        assert_eq!(t.progress(), "");
        t.task = Some(
            serde_json::from_value(serde_json::json!({ "pulp_href": "h", "state": "running",
            "progress_reports": [{"message": "Downloading", "code": "d", "state": "running", "total": null, "done": 3}] }))
            .unwrap(),
        );
        assert_eq!(t.progress(), "Downloading 3");
    }
}
