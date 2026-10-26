//! Pulp REST transport: auth, TLS, bounded concurrency, retries (reads only),
//! pagination, size limits, error mapping and safe HREF handling.

use crate::config::{AuthMethod, Profile, Settings, parse_secret_ref, resolve_secret};
use crate::session_log::{self, Body};
use reqwest::header::{AUTHORIZATION, HeaderValue, RETRY_AFTER};
use reqwest::{Method, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

#[derive(Debug, Clone, thiserror::Error)]
pub enum PulpError {
    #[error("read-only mode: {0} blocked")]
    ReadOnly(String),
    #[error("{}", api_message(*status, message))]
    Api { status: u16, message: String },
    #[error("network error: {0}")]
    Network(String),
    #[error("request timed out")]
    Timeout,
    #[error("invalid response: {0}")]
    Decode(String),
    #[error("response larger than {0} MiB rejected")]
    TooLarge(u64),
    #[error("unsafe href rejected: {0}")]
    BadHref(String),
    #[error("not supported by this server: {0}")]
    Unsupported(String),
    #[error("task {state}: {message}")]
    TaskFailed { href: String, state: String, message: String },
    #[error("could not read task {href}, it may still be running (see Tasks): {message}")]
    TaskUnknown { href: String, message: String },
    #[error("{0}")]
    Config(String),
}

fn api_message(status: u16, message: &str) -> String {
    let what = match status {
        400 => "rejected",
        401 => "authentication failed - check username/secret",
        403 => "permission denied",
        404 => "not found",
        409 => "conflict",
        429 => "rate limited",
        500..=599 => "server error",
        _ => "HTTP error",
    };
    if message.is_empty() { format!("{what} [{status}]") } else { format!("{what} [{status}]: {message}") }
}

impl PulpError {
    pub fn status(&self) -> Option<u16> {
        match self {
            PulpError::Api { status, .. } => Some(*status),
            _ => None,
        }
    }
    pub fn is_not_found(&self) -> bool {
        self.status() == Some(404)
    }
}

pub type Result<T> = std::result::Result<T, PulpError>;

/// Result of a mutating call: Pulp answers either with the object (sync) or a task (202).
#[derive(Debug, Clone)]
pub enum Mutation {
    Task(String),
    TaskGroup(String),
    Object(Value),
    Empty,
}

impl Mutation {
    fn from_response(status: StatusCode, body: Value) -> Self {
        if status == StatusCode::ACCEPTED {
            if let Some(t) = body.get("task").and_then(Value::as_str) {
                return Mutation::Task(t.into());
            }
            if let Some(g) = body.get("task_group").and_then(Value::as_str) {
                return Mutation::TaskGroup(g.into());
            }
        }
        if body.is_null() { Mutation::Empty } else { Mutation::Object(body) }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Page<T> {
    pub count: u64,
    pub next: Option<String>,
    #[serde(default)]
    pub previous: Option<String>,
    pub results: Vec<T>,
}

#[derive(Clone)]
pub struct PulpClient {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    base: url::Url,
    /// Path prefix of every API href, e.g. `/pulp/api/v3/` or `/pulp/<domain>/api/v3/`.
    api_prefix: String,
    /// Configured API root (`/pulp/`); every href must stay below it.
    api_root: String,
    auth: Option<HeaderValue>,
    /// Basic-auth username; identifies the current user on servers without `login/`.
    username: Option<String>,
    sem: Semaphore,
    read_only: bool,
    max_bytes: u64,
    poll: (Duration, Duration),
    upload_chunk: usize,
    pub profile: String,
}

impl std::fmt::Debug for PulpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the auth header.
        f.debug_struct("PulpClient").field("profile", &self.inner.profile).field("base", &self.inner.base.as_str()).finish()
    }
}

impl PulpClient {
    pub fn new(profile_name: &str, p: &Profile, settings: &Settings) -> Result<Self> {
        p.validate().map_err(|e| PulpError::Config(format!("{e:#}")))?;
        let base = url::Url::parse(&p.url).map_err(|e| PulpError::Config(e.to_string()))?;
        let mut b = reqwest::Client::builder()
            .user_agent(concat!("pulp-tui/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(p.timeout_secs.max(1)))
            .redirect(reqwest::redirect::Policy::none());
        if let Some(ca) = &p.ca_cert {
            let pem = std::fs::read(ca).map_err(|e| PulpError::Config(format!("ca_cert {}: {e}", ca.display())))?;
            let certs = reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| PulpError::Config(format!("ca_cert: {e}")))?;
            b = b.tls_certs_merge(certs);
        }
        if let (Some(c), Some(k)) = (&p.client_cert, &p.client_key) {
            let mut pem = std::fs::read(c).map_err(|e| PulpError::Config(format!("client_cert: {e}")))?;
            pem.extend(std::fs::read(k).map_err(|e| PulpError::Config(format!("client_key: {e}")))?);
            b = b.identity(reqwest::Identity::from_pem(&pem).map_err(|e| PulpError::Config(format!("client identity: {e}")))?);
        }
        if let Some(px) = &p.proxy {
            b = b.proxy(reqwest::Proxy::all(px).map_err(|e| PulpError::Config(format!("proxy: {e}")))?);
        }
        let http = b.build().map_err(|e| PulpError::Config(e.to_string()))?;

        let auth = match p.auth {
            AuthMethod::None => None,
            m => {
                let r = parse_secret_ref(p.secret.as_deref().unwrap_or_default()).map_err(|e| PulpError::Config(e.to_string()))?;
                let secret = resolve_secret(profile_name, &r).map_err(|e| PulpError::Config(format!("{e:#}")))?;
                let raw = if m == AuthMethod::Basic {
                    use std::io::Write;
                    let mut buf = Vec::new();
                    let _ = write!(buf, "{}:{}", p.username.as_deref().unwrap_or_default(), secret);
                    format!("Basic {}", base64_encode(&buf))
                } else {
                    format!("Bearer {secret}")
                };
                let mut v = HeaderValue::from_str(&raw).map_err(|_| PulpError::Config("secret contains invalid characters".into()))?;
                v.set_sensitive(true);
                Some(v)
            }
        };
        let api_prefix = match &p.domain {
            Some(d) => format!("{}{}/api/v3/", p.api_root, d),
            None => format!("{}api/v3/", p.api_root),
        };
        Ok(Self {
            inner: Arc::new(Inner {
                http,
                base,
                api_prefix,
                api_root: p.api_root.clone(),
                auth,
                username: p.username.clone().filter(|_| p.auth == AuthMethod::Basic),
                sem: Semaphore::new(settings.max_concurrent_requests.max(1)),
                read_only: p.read_only,
                max_bytes: settings.max_response_mb.max(1) * 1024 * 1024,
                poll: settings.poll_bounds(),
                upload_chunk: (settings.upload_chunk_mb.max(1) * 1024 * 1024) as usize,
                profile: profile_name.into(),
            }),
        })
    }

    pub fn profile(&self) -> &str {
        &self.inner.profile
    }
    pub fn read_only(&self) -> bool {
        self.inner.read_only
    }
    pub fn username(&self) -> Option<&str> {
        self.inner.username.as_deref()
    }
    pub fn api_prefix(&self) -> &str {
        &self.inner.api_prefix
    }
    /// Task polling interval bounds (min, max).
    pub fn poll(&self) -> (Duration, Duration) {
        self.inner.poll
    }
    pub fn upload_chunk(&self) -> usize {
        self.inner.upload_chunk
    }

    /// Absolute href for an API-relative path such as `repositories/rpm/rpm/`.
    pub fn api(&self, path: &str) -> String {
        format!("{}{}", self.inner.api_prefix, path.trim_start_matches('/'))
    }

    /// The OpenAPI document is served below the API root, not the domain.
    pub fn docs_href(&self) -> String {
        format!("{}api/v3/docs/api.json", self.inner.api_root)
    }

    /// Only hrefs on this server's API root are accepted. Next-page links are absolute
    /// URLs and must point to the same origin.
    fn url_for(&self, href: &str) -> Result<url::Url> {
        let url = self.inner.base.join(href).map_err(|_| PulpError::BadHref(href.into()))?;
        // `join` also resolves `//host` and `\\host` to another host.
        // Checked after URL normalization so `..` segments cannot escape the API root.
        if url.origin() != self.inner.base.origin() || !url.path().starts_with(&self.inner.api_root) || href.contains("/../") {
            return Err(PulpError::BadHref(href.into()));
        }
        Ok(url)
    }

    pub async fn get<T: DeserializeOwned>(&self, href: &str, query: &[(&str, String)]) -> Result<T> {
        let v = self.request(Method::GET, href, query, None::<&()>).await?.1;
        serde_json::from_value(v).map_err(|e| PulpError::Decode(e.to_string()))
    }

    pub async fn list<T: DeserializeOwned>(&self, href: &str, query: &[(&str, String)]) -> Result<Page<T>> {
        self.get(href, query).await
    }

    /// Follow `next` links. `cap` bounds memory use for huge collections.
    pub async fn list_all<T: DeserializeOwned>(&self, href: &str, query: &[(&str, String)], cap: usize) -> Result<Vec<T>> {
        let mut q: Vec<(&str, String)> = query.to_vec();
        if !q.iter().any(|(k, _)| *k == "limit") {
            q.push(("limit", "500".into()));
        }
        let mut page: Page<T> = self.get(href, &q).await?;
        let mut out = std::mem::take(&mut page.results);
        while let Some(next) = page.next.take() {
            if out.len() >= cap {
                return Err(PulpError::Unsupported(format!("collection larger than {cap} items")));
            }
            page = self.get(&next, &[]).await?;
            out.append(&mut page.results);
        }
        Ok(out)
    }

    pub async fn post(&self, href: &str, body: &impl Serialize) -> Result<Mutation> {
        self.mutate(Method::POST, href, Some(body)).await
    }
    pub async fn patch(&self, href: &str, body: &impl Serialize) -> Result<Mutation> {
        self.mutate(Method::PATCH, href, Some(body)).await
    }
    pub async fn delete(&self, href: &str) -> Result<Mutation> {
        self.mutate(Method::DELETE, href, None::<&()>).await
    }

    async fn mutate(&self, m: Method, href: &str, body: Option<&impl Serialize>) -> Result<Mutation> {
        if self.inner.read_only {
            return Err(PulpError::ReadOnly(format!("{m} {href}")));
        }
        let (status, v) = self.request(m, href, &[], body).await?;
        Ok(Mutation::from_response(status, v))
    }

    /// POST as multipart form with text fields (content creation endpoints reject JSON).
    /// `file` is an optional (file name, bytes) part sent as field `file`.
    pub async fn post_form(&self, href: &str, fields: &[(&str, String)], file: Option<(String, Vec<u8>)>) -> Result<Mutation> {
        if self.inner.read_only {
            return Err(PulpError::ReadOnly(format!("POST {href}")));
        }
        let url = self.url_for(href)?;
        let mut form = fields.iter().fold(reqwest::multipart::Form::new(), |f, (k, v)| f.text(k.to_string(), v.clone()));
        let mut sent = 0;
        if let Some((name, bytes)) = file {
            sent = bytes.len() as u64;
            form = form.part("file", reqwest::multipart::Part::bytes(bytes).file_name(name));
        }
        let _permit = self.inner.sem.acquire().await.map_err(|_| PulpError::Network("client closed".into()))?;
        let shown = display_url(&url, &[]);
        let mut rb = self.inner.http.post(url).multipart(form);
        if let Some(a) = &self.inner.auth {
            rb = rb.header(AUTHORIZATION, a.clone());
        }
        let (status, _, body) = self.exchange(rb, self.inner.max_bytes, "POST", &shown, Body::Upload(sent), json_or_text).await?;
        if !status.is_success() {
            return Err(PulpError::Api { status: status.as_u16(), message: error_text(&body) });
        }
        let v = if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).map_err(|e| PulpError::Decode(e.to_string()))? };
        Ok(Mutation::from_response(status, v))
    }

    /// One chunk of the chunked upload API (`PUT {upload_href}` with Content-Range).
    pub async fn put_chunk(&self, href: &str, range: (u64, u64, u64), data: Vec<u8>) -> Result<()> {
        if self.inner.read_only {
            return Err(PulpError::ReadOnly(format!("PUT {href}")));
        }
        let url = self.url_for(href)?;
        let sent = data.len() as u64;
        let part = reqwest::multipart::Part::bytes(data).file_name("chunk");
        let form = reqwest::multipart::Form::new().part("file", part);
        let _permit = self.inner.sem.acquire().await.map_err(|_| PulpError::Network("client closed".into()))?;
        let shown = display_url(&url, &[]);
        let mut rb = self.inner.http.put(url).header("Content-Range", format!("bytes {}-{}/{}", range.0, range.1, range.2)).multipart(form);
        if let Some(a) = &self.inner.auth {
            rb = rb.header(AUTHORIZATION, a.clone());
        }
        let (status, _, body) = self.exchange(rb, self.inner.max_bytes, "PUT", &shown, Body::Upload(sent), json_or_text).await?;
        if !status.is_success() {
            return Err(PulpError::Api { status: status.as_u16(), message: error_text(&body) });
        }
        Ok(())
    }

    /// Plain GET of served content (distribution base URLs). No API auth header is sent
    /// to content URLs on other origins.
    pub async fn fetch_content(&self, url: &str, max_bytes: u64) -> Result<(u16, Vec<u8>)> {
        let u = url::Url::parse(url).map_err(|_| PulpError::BadHref(url.into()))?;
        if !matches!(u.scheme(), "http" | "https") {
            return Err(PulpError::BadHref(url.into()));
        }
        let mut rb = self.inner.http.get(u.clone());
        if let Some(a) = &self.inner.auth
            && u.origin() == self.inner.base.origin()
        {
            rb = rb.header(AUTHORIZATION, a.clone());
        }
        // Served files are not stored in the log, only their size.
        let served = |b: &[u8]| Body::Text(format!("<content, {} bytes>", b.len()).into_bytes());
        let (status, _, body) = self.exchange(rb, max_bytes, "GET", &display_url(&u, &[]), Body::None, served).await?;
        Ok((status.as_u16(), body))
    }

    /// Send `rb`, read at most `max` bytes of the answer and record the exchange in the session log
    /// (`shown` makes the logged response body). Headers are never logged.
    async fn exchange(
        &self,
        rb: reqwest::RequestBuilder,
        max: u64,
        method: &str,
        url: &str,
        request: Body,
        shown: fn(&[u8]) -> Body,
    ) -> Result<(StatusCode, Option<Duration>, Vec<u8>)> {
        let started = Instant::now();
        let log = |status: std::result::Result<u16, String>, response: Body| {
            let server = &self.inner.profile;
            session_log::record_request(session_log::Request {
                server,
                method,
                url,
                status,
                elapsed: started.elapsed(),
                request,
                response,
            });
        };
        let resp = match rb.send().await {
            Ok(r) => r,
            Err(e) => {
                let e = map_reqwest(e);
                log(Err(e.to_string()), Body::None);
                return Err(e);
            }
        };
        let status = resp.status();
        // Written before the body read, so a body that is too large or fails to read still reaches the file log.
        let path = url::Url::parse(url).map(|u| u.path().to_string()).unwrap_or_default();
        tracing::debug!(%method, path, status = status.as_u16(), "pulp api");
        let retry_after = resp
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .map(|s| Duration::from_secs(s.min(30)));
        match read_limited(resp, max).await {
            Ok(bytes) => {
                log(Ok(status.as_u16()), shown(&bytes));
                Ok((status, retry_after, bytes))
            }
            Err(e) => {
                log(Err(e.to_string()), Body::None);
                Err(e)
            }
        }
    }

    async fn request(
        &self,
        method: Method,
        href: &str,
        query: &[(&str, String)],
        body: Option<&impl Serialize>,
    ) -> Result<(StatusCode, Value)> {
        let url = self.url_for(href)?;
        let idempotent = method == Method::GET;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let res = self.send_once(method.clone(), url.clone(), query, body).await;
            let retry_after = match &res {
                // Only reads are retried: a mutation might have been applied before the failure.
                Err((e, ra)) if idempotent && attempt < 4 && retryable(e) => ra.unwrap_or_else(|| backoff(attempt)),
                _ => return res.map_err(|(e, _)| e),
            };
            tracing::debug!(%method, path = url.path(), attempt, "retrying read");
            tokio::time::sleep(retry_after).await;
        }
    }

    async fn send_once(
        &self,
        method: Method,
        url: url::Url,
        query: &[(&str, String)],
        body: Option<&impl Serialize>,
    ) -> std::result::Result<(StatusCode, Value), (PulpError, Option<Duration>)> {
        let _permit = self.inner.sem.acquire().await.map_err(|_| (PulpError::Network("client closed".into()), None))?;
        let shown = display_url(&url, query);
        let mut rb = self.inner.http.request(method.clone(), url).query(query);
        if let Some(a) = &self.inner.auth {
            rb = rb.header(AUTHORIZATION, a.clone());
        }
        let mut logged = Body::None;
        if let Some(b) = body {
            rb = rb.json(b);
            logged = serde_json::to_value(b).map_or(Body::None, Body::Json);
        }
        let (status, retry_after, bytes) =
            self.exchange(rb, self.inner.max_bytes, method.as_str(), &shown, logged, json_or_text).await.map_err(|e| (e, None))?;
        if !status.is_success() {
            return Err((PulpError::Api { status: status.as_u16(), message: error_text(&bytes) }, retry_after));
        }
        if bytes.is_empty() {
            return Ok((status, Value::Null));
        }
        let v = serde_json::from_slice(&bytes).map_err(|e| (PulpError::Decode(e.to_string()), None))?;
        Ok((status, v))
    }
}

/// The URL as shown in the log: the query is appended and any user info is dropped (credentials go in the header only).
fn display_url(url: &url::Url, query: &[(&str, String)]) -> String {
    let mut u = url.clone();
    let _ = u.set_username("");
    let _ = u.set_password(None);
    if !query.is_empty() {
        u.query_pairs_mut().extend_pairs(query);
    }
    u.to_string()
}

/// A response body for the log: JSON when it parses, else the raw text.
fn json_or_text(b: &[u8]) -> Body {
    if b.is_empty() { Body::None } else { serde_json::from_slice(b).map_or_else(|_| Body::Text(b.to_vec()), Body::Json) }
}

pub(crate) fn retryable(e: &PulpError) -> bool {
    match e {
        PulpError::Network(_) | PulpError::Timeout => true,
        PulpError::Api { status, .. } => matches!(status, 429 | 502 | 503 | 504),
        _ => false,
    }
}

/// Exponential backoff with jitter: ~0.5s, 1s, 2s (+/- 25%).
pub fn backoff(attempt: u32) -> Duration {
    let base = 500u64 << attempt.saturating_sub(1).min(5);
    Duration::from_millis(jitter(base, 25))
}

/// `base` +/- `pct`% using the clock as an entropy source (no RNG dependency needed).
pub fn jitter(base: u64, pct: u64) -> u64 {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0) as u64;
    let span = base * pct / 100;
    if span == 0 { base } else { base - span + nanos % (2 * span + 1) }
}

fn map_reqwest(e: reqwest::Error) -> PulpError {
    if e.is_timeout() {
        PulpError::Timeout
    } else {
        // Walk the source chain: TLS/DNS details are where the useful message is.
        let mut msg = e.to_string();
        let mut src = std::error::Error::source(&e);
        while let Some(s) = src {
            msg = format!("{msg}: {s}");
            src = s.source();
        }
        PulpError::Network(msg)
    }
}

async fn read_limited(mut resp: reqwest::Response, max: u64) -> Result<Vec<u8>> {
    if resp.content_length().is_some_and(|l| l > max) {
        return Err(PulpError::TooLarge(max / 1024 / 1024));
    }
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(map_reqwest)? {
        if out.len() as u64 + chunk.len() as u64 > max {
            return Err(PulpError::TooLarge(max / 1024 / 1024));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Pulp errors are `{"detail": ".."}`, `{"field": ["msg"]}` or `{"non_field_errors": [..]}`.
pub fn error_text(body: &[u8]) -> String {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(m)) => {
            if let Some(d) = m.get("detail").and_then(Value::as_str) {
                return d.to_string();
            }
            // Some conflicts return the resource itself (e.g. cancelling a finished task).
            if let (Some(h), Some(st)) = (m.get("pulp_href").and_then(Value::as_str), m.get("state").and_then(Value::as_str)) {
                return format!("{h} is already {st}");
            }
            m.iter()
                .map(|(k, v)| {
                    let msg = match v {
                        Value::Array(a) => {
                            a.iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect::<Vec<_>>().join(" ")
                        }
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    if k == "non_field_errors" { msg } else { format!("{k}: {msg}") }
                })
                .collect::<Vec<_>>()
                .join("; ")
        }
        Ok(Value::Array(a)) => {
            a.iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect::<Vec<_>>().join("; ")
        }
        _ => {
            let s = String::from_utf8_lossy(body);
            let s = s.trim();
            if s.starts_with('<') { String::new() } else { s.chars().take(300).collect() }
        }
    }
}

pub fn base64_encode(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for c in input.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"admin:password"), "YWRtaW46cGFzc3dvcmQ=");
    }

    #[test]
    fn error_text_formats() {
        assert_eq!(error_text(br#"{"detail":"Not found."}"#), "Not found.");
        assert_eq!(error_text(br#"{"name":["This field must be unique."]}"#), "name: This field must be unique.");
        assert_eq!(error_text(br#"{"non_field_errors":["bad"]}"#), "bad");
        assert_eq!(error_text(b"<html>500</html>"), "");
        assert_eq!(error_text(b"plain"), "plain");
    }

    #[test]
    fn jitter_stays_in_range() {
        for _ in 0..100 {
            let j = jitter(1000, 20);
            assert!((800..=1200).contains(&j));
        }
    }

    fn client(domain: Option<&str>) -> PulpClient {
        let p = Profile {
            url: "https://pulp.example.com".into(),
            auth: AuthMethod::None,
            domain: domain.map(Into::into),
            ..Default::default()
        };
        PulpClient::new("t", &p, &Settings::default()).unwrap()
    }

    #[test]
    fn hrefs_are_confined_to_api_root() {
        let c = client(None);
        assert_eq!(c.api("repositories/"), "/pulp/api/v3/repositories/");
        assert!(c.url_for("/pulp/api/v3/tasks/1/").is_ok());
        assert!(c.url_for("https://pulp.example.com/pulp/api/v3/tasks/?offset=100").is_ok());
        assert!(c.url_for("https://evil.example.com/pulp/api/v3/tasks/").is_err());
        assert!(c.url_for("/admin/").is_err());
        assert!(c.url_for("/pulp/api/v3/../../admin/").is_err());
        assert!(c.url_for("//evil.example.com/pulp/api/v3/tasks/").is_err());
        assert!(c.url_for("\\\\evil.example.com/pulp/api/v3/tasks/").is_err());
        assert!(c.url_for("http://pulp.example.com/pulp/api/v3/tasks/").is_err()); // other scheme, other origin
        let d = client(Some("rhel"));
        assert_eq!(d.api("tasks/"), "/pulp/rhel/api/v3/tasks/");
    }
}
