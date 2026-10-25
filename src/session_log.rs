//! In-memory log of the session: every Pulp request and every action notice.
//!
//! Keeps the last `MAX_ROWS` rows (hidden poll rows are dropped first); each stored body is cut at
//! `MAX_BODY` bytes. JSON keys `password`, `token`, `secret`, `client_key` and any key ending in
//! `_password`, `_token` or `_secret` are replaced with `•••` at any depth. The Authorization header is
//! never recorded (`Request` has no header field) and upload bodies are replaced by their size. The log
//! lives in memory only and is never written to disk.

use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

pub const MAX_ROWS: usize = 1000;
pub const MAX_BODY: usize = 64 * 1024;

pub enum Body {
    None,
    Json(Value),
    Text(Vec<u8>),
    Upload(u64),
}

pub struct Request<'a> {
    pub server: &'a str,
    pub method: &'a str,
    pub url: &'a str,
    pub status: Result<u16, String>,
    pub elapsed: Duration,
    pub request: Body,
    pub response: Body,
}

struct Entry {
    id: u64,
    at: String,
    kind: &'static str,
    server: String,
    summary: String,
    poll: bool,
    detail: Vec<String>,
}

pub struct Log {
    next_id: u64,
    rows: VecDeque<Entry>,
}

impl Log {
    pub const fn new() -> Log {
        Log { next_id: 1, rows: VecDeque::new() }
    }

    pub fn push(&mut self, kind: &'static str, server: &str, summary: String, poll: bool, detail: Vec<String>) -> u64 {
        if self.rows.len() >= MAX_ROWS {
            // Hidden polls go first: a long wait must not push the rows the user can see out of the log.
            let i = self.rows.iter().position(|e| e.poll).unwrap_or(0);
            self.rows.remove(i);
        }
        let id = self.next_id;
        self.next_id += 1;
        let at = crate::store::now().get(11..19).unwrap_or_default().to_string();
        self.rows.push_back(Entry { id, at, kind, server: server.to_string(), summary, poll, detail });
        id
    }

    /// Newest first; poll rows only when `polls` is set.
    pub fn rows(&self, polls: bool) -> Vec<Value> {
        let row = |e: &Entry| serde_json::json!({"id": e.id, "at": e.at, "kind": e.kind, "server": e.server, "summary": e.summary});
        self.rows.iter().rev().filter(|e| polls || !e.poll).map(row).collect()
    }

    pub fn detail(&self, id: u64) -> Option<Vec<String>> {
        self.rows.iter().find(|e| e.id == id).map(|e| e.detail.clone())
    }

    pub fn last_id(&self) -> u64 {
        self.next_id - 1
    }
}

impl Default for Log {
    fn default() -> Log {
        Log::new()
    }
}

static LOG: Mutex<Log> = Mutex::new(Log::new());

// A panic while the lock was held must not take the UI down with it.
fn lock() -> MutexGuard<'static, Log> {
    LOG.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn record(kind: &'static str, server: &str, summary: String, detail: Vec<String>) {
    lock().push(kind, server, summary, false, detail);
}

pub fn record_request(r: Request) {
    let (summary, detail) = request_lines(&r);
    lock().push("request", r.server, summary, is_poll(r.method, path_and_query(r.url)), detail);
}

pub fn rows(polls: bool) -> Vec<Value> {
    lock().rows(polls)
}

pub fn detail(id: u64) -> Option<Vec<String>> {
    lock().detail(id)
}

pub fn last_id() -> u64 {
    lock().last_id()
}

pub fn redact(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, v) in m.iter_mut() {
                // `headers` of a remote can carry an Authorization value.
                let k = k.to_ascii_lowercase();
                let secret = matches!(k.as_str(), "password" | "token" | "secret" | "client_key" | "authorization" | "headers")
                    || ["_password", "_token", "_secret"].iter().any(|s| k.ends_with(s));
                if secret {
                    *v = Value::String("•••".into());
                } else {
                    redact(v);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(redact),
        _ => {}
    }
}

/// A GET of one task or task group: the polling a running job does every moment.
pub fn is_poll(method: &str, path: &str) -> bool {
    let mut parts = path.split('?').next().unwrap_or_default().trim_end_matches('/').rsplit('/');
    method == "GET" && parts.next().is_some_and(|id| !id.is_empty()) && matches!(parts.next(), Some("tasks" | "task-groups"))
}

fn clip(s: String) -> String {
    let total = s.len();
    clip_of(s, total)
}

/// Cut `s` at `MAX_BODY`; `total` is the size of the original, which `s` may be only the head of.
fn clip_of(mut s: String, total: usize) -> String {
    if total <= MAX_BODY {
        return s;
    }
    let mut end = MAX_BODY;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    format!("{s}\n(cut, {total} bytes total)")
}

/// The URL without scheme and host.
fn path_and_query(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.find('/').map_or("/", |i| &rest[i..])
}

fn body_lines(b: &Body) -> Vec<String> {
    let text = match b {
        Body::None => String::new(),
        Body::Json(v) => {
            let mut v = v.clone();
            redact(&mut v);
            clip(serde_json::to_string_pretty(&v).unwrap_or_default())
        }
        // Responses can be tens of MB: convert only what is kept (+4 bytes, so a char at the cut is whole).
        Body::Text(t) => clip_of(String::from_utf8_lossy(&t[..t.len().min(MAX_BODY + 4)]).into_owned(), t.len()),
        Body::Upload(n) => format!("<upload, {n} bytes>"),
    };
    text.lines().map(String::from).collect()
}

/// The one-line summary and the detail lines of a request.
pub fn request_lines(r: &Request) -> (String, Vec<String>) {
    let ms = r.elapsed.as_millis();
    let (outcome, status_line) = match &r.status {
        Ok(code) => (code.to_string(), format!("status: {code}")),
        Err(e) => (e.clone(), format!("error: {e}")),
    };
    let summary = format!("{} {} {outcome} {ms}ms", r.method, path_and_query(r.url));
    let mut detail =
        vec![format!("{} {}", r.method, r.url), format!("server: {}   duration: {ms} ms", r.server), status_line, String::new()];
    if !matches!(r.request, Body::None) {
        detail.push("── request body ──".into());
        detail.extend(body_lines(&r.request));
    }
    detail.push("── response body ──".into());
    detail.extend(body_lines(&r.response));
    (summary, detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn redact_nested_secrets() {
        let mut v = json!({"name": "r", "password": "p", "proxy_password": "q", "client_key": "k",
            "nested": [{"token": "t", "db_password": "d", "keep": "ok", "sles_auth_token": "sl"}], "secret": "s", "x_secret": "xs"});
        redact(&mut v);
        let s = v.to_string();
        for bad in ["\"p\"", "\"q\"", "\"k\"", "\"t\"", "\"d\"", "\"s\"", "\"sl\"", "\"xs\""] {
            assert!(!s.contains(bad), "{s}");
        }
        assert_eq!(v["nested"][0]["keep"], "ok");
        assert_eq!(v["password"], "•••");

        // Key case does not matter; a remote's headers can carry a bearer token.
        let mut v = json!({"Password": "p", "X_Token": "t", "headers": [{"Authorization": "Bearer b"}]});
        redact(&mut v);
        assert_eq!(v, json!({"Password": "•••", "X_Token": "•••", "headers": "•••"}));
    }

    #[test]
    fn clip_is_char_safe() {
        // The 1-byte prefix puts every char boundary on an odd offset, so the cut at MAX_BODY (even) is inside a char.
        let s = format!("a{}", "é".repeat(MAX_BODY));
        assert!(!s.is_char_boundary(MAX_BODY));
        let total = s.len();
        let c = clip(s);
        assert!(c.len() <= MAX_BODY + 64);
        assert!(c.ends_with("total)"), "{}", &c[c.len() - 40..]);
        assert!(c.starts_with("aé"));
        assert!(!c.contains('\u{FFFD}'));
        assert!(c.ends_with(&format!("é\n(cut, {total} bytes total)")), "{}", &c[c.len() - 40..]);
        assert_eq!(clip("short".into()), "short");
    }

    #[test]
    fn huge_text_body_is_cut_and_reports_the_original_size() {
        let total = MAX_BODY * 3;
        let lines = body_lines(&Body::Text(vec![b'a'; total]));
        let len: usize = lines.iter().map(String::len).sum();
        assert!(len <= MAX_BODY + 64, "{len}");
        assert!(lines.last().unwrap().contains(&format!("{total} bytes total")), "{:?}", lines.last());
    }

    #[test]
    fn ring_keeps_last_1000() {
        let mut l = Log::new();
        let first = l.push("notice", "s", "first".into(), false, vec![]);
        for i in 0..MAX_ROWS {
            l.push("notice", "s", format!("n{i}"), false, vec![]);
        }
        assert_eq!(l.rows(true).len(), MAX_ROWS);
        assert!(l.detail(first).is_none());
        assert_eq!(l.rows(true)[0]["summary"], format!("n{}", MAX_ROWS - 1));
    }

    #[test]
    fn polls_are_evicted_before_visible_rows() {
        let mut l = Log::new();
        for i in 0..MAX_ROWS - 1 {
            l.push("notice", "s", format!("n{i}"), false, vec![]);
        }
        for i in 0..5000 {
            l.push("request", "s", format!("GET poll{i}"), true, vec![]);
        }
        assert_eq!(l.rows(false).len(), MAX_ROWS - 1);
        assert!(l.rows(true).len() <= MAX_ROWS);
    }

    #[test]
    fn polls_hidden_unless_asked() {
        let mut l = Log::new();
        l.push("request", "s", "GET poll".into(), true, vec![]);
        l.push("request", "s", "POST x".into(), false, vec![]);
        assert_eq!(l.rows(false).len(), 1);
        assert_eq!(l.rows(true).len(), 2);
    }

    #[test]
    fn poll_detection() {
        assert!(is_poll("GET", "/pulp/api/v3/tasks/0190ab/"));
        assert!(is_poll("GET", "/pulp/api/v3/task-groups/0190ab/"));
        assert!(!is_poll("GET", "/pulp/api/v3/tasks/"));
        assert!(!is_poll("POST", "/pulp/api/v3/tasks/0190ab/"));
        assert!(!is_poll("GET", "/pulp/api/v3/repositories/rpm/rpm/"));
    }

    #[test]
    fn request_detail_for_error_and_text() {
        let r = Request {
            server: "prod",
            method: "GET",
            url: "https://h/pulp/api/v3/x/?a=1",
            status: Ok(502),
            elapsed: Duration::from_millis(84),
            request: Body::None,
            response: Body::Text(b"<html>bad gateway</html>".to_vec()),
        };
        let (summary, detail) = request_lines(&r);
        assert_eq!(summary, "GET /pulp/api/v3/x/?a=1 502 84ms");
        assert!(detail.iter().any(|l| l.contains("bad gateway")));
        assert!(!detail.iter().any(|l| l.contains("request body")));
        let r = Request { status: Err("connection refused".into()), response: Body::None, request: Body::Upload(42), ..r };
        let (summary, detail) = request_lines(&r);
        assert!(summary.contains("connection refused"));
        assert!(detail.iter().any(|l| l == "<upload, 42 bytes>"));
    }

    #[test]
    fn request_lines_redacts_bodies() {
        let r = Request {
            server: "prod",
            method: "POST",
            url: "https://h/pulp/api/v3/x/",
            status: Ok(201),
            elapsed: Duration::from_millis(5),
            request: Body::Json(json!({"password": "hunter2", "name": "r"})),
            response: Body::Json(json!({"results": [{"token": "abc"}]})),
        };
        let (_, detail) = request_lines(&r);
        assert!(!detail.iter().any(|l| l.contains("hunter2") || l.contains("abc")), "{detail:?}");
        assert!(detail.iter().any(|l| l.contains("•••")), "{detail:?}");
        assert!(detail.iter().any(|l| l.contains("\"r\"")), "{detail:?}");
    }
}
