//! Typed Pulp operations shared by the TUI and the workflow services. Every mutation
//! that Pulp runs as a task is awaited here with adaptive polling.

use super::client::{Mutation, Page, PulpClient, PulpError, Result, jitter, retryable};
use super::models::*;
use super::{deb, file, rpm};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// Callback for task progress (the TUI forwards these to the Task Center).
pub type OnTask<'a> = &'a (dyn Fn(&Task) + Send + Sync);

pub fn no_progress(_: &Task) {}

/// How long `wait_task` and `wait_task_group` keep polling through transient read errors (each GET is also retried by the client).
const POLL_OUTAGE: Duration = Duration::from_secs(120);

/// Poll a task until it reaches a final state. The interval grows while nothing changes
/// and resets on progress, so long tasks cost few requests. A task that cannot be read
/// gives `TaskUnknown`, never `TaskFailed`: it may still run.
pub async fn wait_task(c: &PulpClient, href: &str, on: OnTask<'_>) -> Result<Task> {
    let (min, max) = c.poll();
    let mut interval = min;
    let mut last: Option<Task> = None;
    let mut failing: Option<std::time::Instant> = None;
    loop {
        let t: Task = match c.get(href, &[]).await {
            Ok(t) => t,
            Err(e) if retryable(&e) && failing.get_or_insert_with(std::time::Instant::now).elapsed() < POLL_OUTAGE => {
                tokio::time::sleep(max).await;
                continue;
            }
            Err(e) => return Err(PulpError::TaskUnknown { href: href.into(), message: e.to_string() }),
        };
        failing = None;
        let changed = last.as_ref() != Some(&t);
        if changed {
            on(&t);
        }
        if t.is_final() {
            return match t.state.as_str() {
                "completed" | "skipped" => Ok(t),
                _ => {
                    Err(PulpError::TaskFailed { href: href.into(), message: t.error_message().unwrap_or_default(), state: t.state.clone() })
                }
            };
        }
        interval = if changed && last.is_some() { min } else { (interval * 3 / 2).min(max) };
        last = Some(t);
        tokio::time::sleep(Duration::from_millis(jitter(interval.as_millis() as u64, 20))).await;
    }
}

/// Await whatever a mutation returned. Returns the finished task when there was one.
pub async fn settle(c: &PulpClient, m: Mutation, on: OnTask<'_>) -> Result<Option<Task>> {
    match m {
        Mutation::Task(h) => wait_task(c, &h, on).await.map(Some),
        Mutation::TaskGroup(g) => {
            wait_task_group(c, &g, &|_| {}).await?;
            Ok(None)
        }
        _ => Ok(None),
    }
}

/// Poll a task group until it is final. When the server never sets a completion flag,
/// an idle group whose counts stay unchanged for three polls is treated as final.
pub async fn wait_task_group(c: &PulpClient, href: &str, on: &(dyn Fn(&TaskGroup) + Send + Sync)) -> Result<TaskGroup> {
    let (min, max) = c.poll();
    let mut interval = min;
    let mut stable = 0;
    let mut last = None;
    let mut failing: Option<std::time::Instant> = None;
    loop {
        let g: TaskGroup = match c.get(href, &[]).await {
            Ok(g) => g,
            Err(e) if retryable(&e) && failing.get_or_insert_with(std::time::Instant::now).elapsed() < POLL_OUTAGE => {
                tokio::time::sleep(max).await;
                continue;
            }
            Err(e) => return Err(e),
        };
        failing = None;
        on(&g);
        let key = (g.active(), g.done());
        stable = if g.active() == 0 && g.done() > 0 && last == Some(key) { stable + 1 } else { 0 };
        if g.is_final() || stable >= 3 {
            return Ok(g);
        }
        last = Some(key);
        interval = (interval * 3 / 2).min(max);
        tokio::time::sleep(Duration::from_millis(jitter(interval.as_millis() as u64, 20))).await;
    }
}

/// Href of the object a mutation created: synchronous object or task's created resource.
pub fn created_href(m_obj: Option<&Value>, task: Option<&Task>, needle: &str) -> Result<String> {
    if let Some(h) = m_obj.and_then(|o| o["pulp_href"].as_str()) {
        return Ok(h.into());
    }
    task.and_then(|t| t.created_matching(needle))
        .ok_or_else(|| PulpError::Decode(format!("server did not report a created {needle} resource")))
}

/// POST and await; returns the href of the created object.
pub async fn create(c: &PulpClient, collection: &str, body: &Value, needle: &str, on: OnTask<'_>) -> Result<String> {
    let m = c.post(collection, body).await?;
    let obj = if let Mutation::Object(o) = &m { Some(o.clone()) } else { None };
    let task = settle(c, m, on).await?;
    created_href(obj.as_ref(), task.as_ref(), needle)
}

/// All distributions of a type. Callers match bindings client-side: the list has no `publication` filter.
pub async fn distributions(c: &PulpClient, t: RepoType) -> Result<Vec<Distribution>> {
    c.list_all(&c.api(t.distributions()), &[], 100_000).await
}

/// `/…/repositories/rpm/rpm/<id>/versions/3/` -> repository href.
pub fn repo_of_version(v: &str) -> String {
    v.split("versions/").next().unwrap_or(v).to_string()
}

/// `/…/versions/3/` -> 3.
pub fn version_number(v: &str) -> u64 {
    v.trim_end_matches('/').rsplit('/').next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// The distributions that make Pulp refuse to delete `version`, each with the reason: pulpcore
/// `Repository.protected_versions`. `pubs`: the publications of its repository (the list has only
/// complete ones). File, RPM and Deb distributions serve from publications.
pub fn protecting<'a>(version: &str, pubs: &[Publication], ds: &'a [Distribution]) -> Vec<(&'a Distribution, &'static str)> {
    let repo = repo_of_version(version);
    let newest = pubs.iter().filter_map(|p| p.repository_version.as_deref()).max_by_key(|v| version_number(v));
    let own: Vec<&Publication> = pubs.iter().filter(|p| p.repository_version.as_deref() == Some(version)).collect();
    let reason = |d: &Distribution| {
        let follows = d.repository.as_deref() == Some(repo.as_str());
        if d.repository_version.as_deref() == Some(version) {
            Some("pinned to this version")
        } else if own.iter().any(|p| d.publication.as_ref() == Some(&p.pulp_href)) {
            Some("pinned to a publication of this version")
        } else if follows && d.checkpoint && own.iter().any(|p| p.checkpoint) {
            Some("checkpoint distribution; this version has a checkpoint publication")
        } else if follows && !d.checkpoint && newest == Some(version) {
            Some("follows the repository; this is its newest published version")
        } else {
            None
        }
    };
    ds.iter().filter_map(|d| reason(d).map(|why| (d, why))).collect()
}

/// The publications of `version` (Pulp deletes them with it) and the distributions that protect it.
/// shortcut: lists only `t.publications()`, so Deb verbatim publications are missed; list the generic
/// `publications/` once pulp-tui creates verbatim publications.
pub async fn version_protection(
    c: &PulpClient,
    t: RepoType,
    version: &str,
) -> Result<(Vec<Publication>, Vec<(Distribution, &'static str)>)> {
    let pubs: Vec<Publication> = c.list_all(&c.api(t.publications()), &[("repository", repo_of_version(version))], 100_000).await?;
    let ds = distributions(c, t).await?;
    let by = protecting(version, &pubs, &ds).into_iter().map(|(d, why)| (d.clone(), why)).collect();
    Ok((pubs.into_iter().filter(|p| p.repository_version.as_deref() == Some(version)).collect(), by))
}

pub async fn find_by_name<T: serde::de::DeserializeOwned>(c: &PulpClient, collection: &str, name: &str) -> Result<Option<T>> {
    let p: Page<T> = c.list(collection, &[("name", name.into()), ("limit", "1".into())]).await?;
    Ok(p.results.into_iter().next())
}

/// File and Deb have only `mirror: bool`: any `mirror*` policy (the RPM values, from a mixed Content View) means mirror.
pub async fn sync(c: &PulpClient, t: RepoType, repo: &str, remote: Option<&str>, policy: &str) -> Result<Mutation> {
    let mut body = json!({});
    if let Some(r) = remote {
        body["remote"] = r.into();
    }
    match t {
        RepoType::Rpm => body["sync_policy"] = policy.into(),
        RepoType::File | RepoType::Deb => body["mirror"] = policy.starts_with("mirror").into(),
    }
    c.post(&format!("{repo}sync/"), &body).await
}

/// Reuse a publication of the version made with the same signing service, else create one.
/// A reused publication keeps the repository settings of the time it was made; the operator's
/// Publish uses `create_publication` instead.
pub async fn ensure_publication(
    c: &PulpClient,
    t: RepoType,
    version: &str,
    signing_service: Option<&str>,
    on: OnTask<'_>,
) -> Result<String> {
    let all: Vec<Publication> = c.list_all(&c.api(t.publications()), &[("repository_version", version.into())], 1_000).await?;
    if let Some(pubn) = all.into_iter().find(|p| p.signing_service.as_deref() == signing_service) {
        return Ok(pubn.pulp_href);
    }
    create_publication(c, t, version, signing_service, on).await
}

/// A new publication of `version`, made with the repository settings of now. `signing_service` is sent
/// only when set: Deb is the only type that takes it.
pub async fn create_publication(
    c: &PulpClient,
    t: RepoType,
    version: &str,
    signing_service: Option<&str>,
    on: OnTask<'_>,
) -> Result<String> {
    let mut body = json!({ "repository_version": version });
    if let Some(s) = signing_service {
        body["signing_service"] = s.into();
    }
    create(c, &c.api(t.publications()), &body, "/publications/", on).await
}

/// Create or re-point a distribution. Refuses to take over a base path owned by a
/// different distribution, or an existing distribution that lacks the given `labels`
/// (labels mark ownership; pass `{}` for no ownership check). `db` = (label key, database id,
/// aliases): a distribution of another database is refused; an unlabeled one is adopted.
#[allow(clippy::too_many_arguments)]
pub async fn ensure_distribution(
    c: &PulpClient,
    t: RepoType,
    name: &str,
    base_path: &str,
    publication: Option<&str>,
    labels: &Value,
    db: Option<(&str, &str, &[String])>,
    on: OnTask<'_>,
) -> Result<String> {
    let coll = c.api(t.distributions());
    if let Some(d) = find_by_name::<Distribution>(c, &coll, name).await? {
        if d.base_path != base_path {
            return Err(PulpError::Api {
                status: 409,
                message: format!("distribution {name} serves {} instead of {base_path}", d.base_path),
            });
        }
        let adopt = owner_check(&d, labels, db)?;
        let repoint = d.publication.as_deref() != publication || d.repository.is_some() || d.repository_version.is_some();
        if repoint || adopt.is_some() {
            let mut body = json!({});
            if repoint {
                body = json!({ "publication": publication, "repository": null });
                if d.repository_version.is_some() {
                    body["repository_version"] = Value::Null;
                }
            }
            if let Some((key, id)) = adopt {
                // Pulp replaces the whole label map; `labels` are already present (checked above).
                let mut all = d.pulp_labels.clone();
                all.insert(key, id);
                body["pulp_labels"] = json!(all);
            }
            let m = c.patch(&d.pulp_href, &body).await?;
            settle(c, m, on).await?;
        }
        return Ok(d.pulp_href);
    }
    let taken: Page<Value> = c.list(&c.api("distributions/"), &[("base_path", base_path.into()), ("limit", "1".into())]).await?;
    if let Some(o) = taken.results.first() {
        return Err(PulpError::Api {
            status: 409,
            message: format!("base path {base_path} is used by distribution {}", o["name"].as_str().unwrap_or("?")),
        });
    }
    let mut labels = labels.clone();
    if let Some((key, id, _)) = db {
        labels[key] = id.into();
    }
    create(
        c,
        &coll,
        &json!({ "name": name, "base_path": base_path, "publication": publication, "pulp_labels": labels }),
        "/distributions/",
        on,
    )
    .await
}

/// Ownership of an existing distribution: it must carry `labels` and must not belong to another
/// database (`db` as in `ensure_distribution`). Returns the database-id label to adopt, if any.
fn owner_check(d: &Distribution, labels: &Value, db: Option<(&str, &str, &[String])>) -> Result<Option<(String, String)>> {
    let name = &d.name;
    let owned = labels.as_object().into_iter().flatten().all(|(k, v)| d.pulp_labels.get(k).map(String::as_str) == v.as_str());
    if !owned {
        return Err(PulpError::Api { status: 409, message: format!("distribution {name} exists but lacks ownership labels {labels}") });
    }
    let Some((key, id, aliases)) = db else { return Ok(None) };
    let adopt = label_owner(&d.pulp_labels, key, id, aliases)
        .map_err(|o| PulpError::Api { status: 409, message: format!("distribution {name} is owned by another pulp-tui database ({o})") })?;
    Ok(adopt.then(|| (key.to_string(), id.to_string())))
}

/// Move a distribution at `href` to `base_path` in place, after the checks of `owner_check`, and
/// wait for the task. `Ok(false)`: it already serves at `base_path` and needs no adoption (nothing sent).
pub async fn move_distribution(
    c: &PulpClient,
    href: &str,
    base_path: &str,
    labels: &Value,
    db: Option<(&str, &str, &[String])>,
    on: OnTask<'_>,
) -> Result<bool> {
    let d: Distribution = c.get(href, &[]).await?;
    let adopt = owner_check(&d, labels, db)?;
    if d.base_path == base_path && adopt.is_none() {
        return Ok(false);
    }
    let mut body = json!({ "base_path": base_path });
    if let Some((key, id)) = adopt {
        // Pulp replaces the whole label map; `labels` are already present (checked above).
        let mut all = d.pulp_labels.clone();
        all.insert(key, id);
        body["pulp_labels"] = json!(all);
    }
    let m = c.patch(href, &body).await?;
    settle(c, m, on).await?;
    Ok(true)
}

/// Who owns an object by its database-id label: `Ok(false)` this database, `Ok(true)` nobody
/// yet or an alias of this database (set the label), `Err(id)` another database.
pub fn label_owner(labels: &BTreeMap<String, String>, key: &str, id: &str, aliases: &[String]) -> std::result::Result<bool, String> {
    match labels.get(key) {
        Some(v) if v == id => Ok(false),
        Some(v) if !aliases.contains(v) => Err(v.clone()),
        _ => Ok(true),
    }
}

/// Large pages: with offset paging, 127,000 units took about 4 minutes at 500 per page.
pub const CONTENT_PAGE: &str = "5000";

/// Hrefs of all content units (any type) of a version. The generic endpoint returns no
/// type field; callers classify by href path (`/content/rpm/packages/`, ...).
pub async fn content_in(c: &PulpClient, version: &str) -> Result<Vec<String>> {
    let q = [("repository_version", version.to_string()), ("fields", "pulp_href".into()), ("limit", CONTENT_PAGE.into())];
    let v: Vec<Value> = c.list_all(&c.api("content/"), &q, 2_000_000).await?;
    Ok(v.into_iter().filter_map(|x| x["pulp_href"].as_str().map(str::to_string)).collect())
}

/// A new version of `repo` without `units` (any content type).
pub async fn remove_content(c: &PulpClient, repo: &str, units: Vec<String>) -> Result<Mutation> {
    c.post(&format!("{repo}modify/"), &json!({ "remove_content_units": units })).await
}

/// A new version of `repo` with `units` added (any content type), exactly those: no related units.
pub async fn add_content(c: &PulpClient, repo: &str, units: Vec<String>) -> Result<Mutation> {
    c.post(&format!("{repo}modify/"), &json!({ "add_content_units": units })).await
}

/// Bytes of hrefs in one copy or modify body. Pulp refuses a body above Django's
/// `DATA_UPLOAD_MAX_MEMORY_SIZE` (default 2,621,440) with HTTP 400; this leaves room for the rest.
const BATCH_BYTES: usize = 2_000_000;

/// `hrefs` in batches that each fit in one request body, sized by the longest href (with its JSON
/// quotes and comma). An empty list is one empty batch: it still makes the request (an empty version).
pub fn batches(hrefs: &[String]) -> Vec<&[String]> {
    let longest = hrefs.iter().map(|h| h.len() + 3).max().unwrap_or(1);
    let mut out: Vec<_> = hrefs.chunks((BATCH_BYTES / longest).max(1)).collect();
    if out.is_empty() {
        out.push(hrefs);
    }
    out
}

/// Human-readable identity of each package or file in a version (NEVRA or path+digest) for diffs.
/// Same keys as a Content View selection, which compares against them; `all_content_keys` has every type.
pub async fn content_keys(c: &PulpClient, t: RepoType, version: Option<&str>) -> Result<BTreeSet<String>> {
    let Some(v) = version else { return Ok(BTreeSet::new()) };
    Ok(match t {
        RepoType::Rpm => rpm::packages_in(c, v).await?.iter().map(Package::nevra).collect(),
        RepoType::File => file::files_in(c, v).await?.iter().map(file::key).collect(),
        RepoType::Deb => deb::packages_in(c, v).await?.iter().map(DebPackage::key).collect(),
    })
}

/// Keys of every unit of a version, of each content type of `t`, prefixed with the type
/// (`advisory FIXTURE-2026:0001`): for the Versions compare. Fetches only the fields of the keys.
pub async fn all_content_keys(c: &PulpClient, t: RepoType, version: &str) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for ct in t.content_types() {
        let q = [("repository_version", version.to_string()), ("fields", ct.fields.into()), ("limit", CONTENT_PAGE.into())];
        let units: Vec<Value> = c.list_all(&c.api(ct.path), &q, 2_000_000).await?;
        out.extend(units.iter().map(ct.key));
    }
    Ok(out)
}

/// Fetch what a distribution actually serves and compare it with a repository version:
/// File manifests must list exactly the version's files; RPM and Deb metadata must be served and
/// the smallest package must be downloadable with the expected digest.
pub async fn verify_served(c: &PulpClient, t: RepoType, base_url: &str, version: &str, layout: &str) -> Result<String> {
    let base = if base_url.ends_with('/') { base_url.to_string() } else { format!("{base_url}/") };
    match t {
        RepoType::Rpm => rpm::verify(c, &base, version, layout).await,
        RepoType::File => file::verify(c, &base, version).await,
        RepoType::Deb => deb::verify(c, &base, version).await,
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct Diff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

pub fn diff(old: &BTreeSet<String>, new: &BTreeSet<String>) -> Diff {
    Diff { added: new.difference(old).cloned().collect(), removed: old.difference(new).cloned().collect() }
}

impl Diff {
    pub fn lines(&self, old: &str, new: &str) -> Vec<String> {
        let mut out = vec![format!("{old} -> {new}: +{} added, -{} removed", self.added.len(), self.removed.len()), String::new()];
        out.extend(self.added.iter().map(|k| format!("+ {k}")));
        out.extend(self.removed.iter().map(|k| format!("- {k}")));
        out
    }
}

/// Deletes an unfinished chunked upload when its future is dropped (the job was aborted with Esc).
/// Disarmed once the upload is committed: then a Pulp task uses it and must not lose it.
struct DeleteOnDrop(Option<(PulpClient, String)>);

impl Drop for DeleteOnDrop {
    fn drop(&mut self) {
        // shortcut: a quit stops the runtime before this delete runs; Pulp keeps that upload until its own cleanup.
        if let (Some((c, href)), Ok(rt)) = (self.0.take(), tokio::runtime::Handle::try_current()) {
            rt.spawn(async move {
                let _ = c.delete(&href).await;
            });
        }
    }
}

#[derive(Debug, Clone)]
pub struct UploadResult {
    pub content_href: String,
    pub sha256: String,
    pub task: Option<Task>,
}

/// Upload a local file as RPM package or File content, optionally adding it to a repository.
/// Small files go in one request; large ones use the chunked upload API. The server's
/// digest is compared with the local one, and an unfinished chunked upload is deleted on failure.
pub async fn upload(
    c: &PulpClient,
    t: RepoType,
    path: &Path,
    relative_path: Option<&str>,
    repository: Option<&str>,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
    on: OnTask<'_>,
) -> Result<UploadResult> {
    let io = |e: std::io::Error| PulpError::Config(format!("{}: {e}", path.display()));
    let size = tokio::fs::metadata(path).await.map_err(io)?.len();
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("upload").to_string();
    let mut fields: Vec<(&str, String)> = vec![];
    if let Some(r) = repository {
        fields.push(("repository", r.into()));
    }
    match t {
        RepoType::Rpm => {}
        RepoType::File => fields.push(("relative_path", relative_path.unwrap_or(&file_name).into())),
        RepoType::Deb => return Err(PulpError::Unsupported("upload of Deb packages is not supported".into())),
    }
    let chunk = c.upload_chunk();
    let mut f = tokio::fs::File::open(path).await.map_err(io)?;
    let mut hasher = Sha256::new();

    let (m, upload_href) = if size as usize <= chunk {
        let mut buf = Vec::with_capacity(size as usize);
        f.read_to_end(&mut buf).await.map_err(io)?;
        hasher.update(&buf);
        progress(size, size);
        (c.post_form(&c.api(t.content()), &fields, Some((file_name, buf))).await?, None)
    } else {
        let up = c.post(&c.api("uploads/"), &json!({ "size": size })).await?;
        let Mutation::Object(o) = up else { return Err(PulpError::Decode("upload create returned no object".into())) };
        let href = o["pulp_href"].as_str().ok_or_else(|| PulpError::Decode("upload without href".into()))?.to_string();
        let mut guard = DeleteOnDrop(Some((c.clone(), href.clone())));
        let res = async {
            let mut offset = 0u64;
            let mut buf = vec![0u8; chunk];
            loop {
                let mut n = 0;
                while n < chunk {
                    let r = f.read(&mut buf[n..]).await.map_err(io)?;
                    if r == 0 {
                        break;
                    }
                    n += r;
                }
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                c.put_chunk(&href, (offset, offset + n as u64 - 1, size), buf[..n].to_vec()).await?;
                offset += n as u64;
                progress(offset, size);
            }
            fields.push(("upload", href.clone()));
            c.post_form(&c.api(t.content()), &fields, None).await
        }
        .await;
        guard.0 = None;
        match res {
            Ok(m) => (m, Some(href)),
            Err(e) => {
                let _ = c.delete(&href).await;
                return Err(e);
            }
        }
    };
    let sha256 = hex::encode(hasher.finalize());
    let obj = if let Mutation::Object(o) = &m { Some(o.clone()) } else { None };
    let task = match settle(c, m, on).await {
        Ok(t) => t,
        Err(e) => {
            if let Some(h) = upload_href {
                // Pulp deletes the upload when it consumes it; this only removes leftovers.
                let _ = c.delete(&h).await;
            }
            return Err(e);
        }
    };
    let content_href = created_href(obj.as_ref(), task.as_ref(), "/content/")?;
    let content: Value = c.get(&content_href, &[("fields", "sha256".into())]).await?;
    if content["sha256"].as_str() != Some(sha256.as_str()) {
        return Err(PulpError::Decode(format!("digest mismatch for {content_href}: local {sha256}, server {}", content["sha256"])));
    }
    Ok(UploadResult { content_href, sha256, task })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_owner_rules() {
        let l = |v: Option<&str>| v.map(|x| BTreeMap::from([("pulp_tui_db".to_string(), x.to_string())])).unwrap_or_default();
        let aliases = vec!["old".to_string()];
        assert_eq!(label_owner(&l(Some("me")), "pulp_tui_db", "me", &aliases), Ok(false));
        assert_eq!(label_owner(&l(None), "pulp_tui_db", "me", &aliases), Ok(true));
        assert_eq!(label_owner(&l(Some("old")), "pulp_tui_db", "me", &aliases), Ok(true));
        assert_eq!(label_owner(&l(Some("other")), "pulp_tui_db", "me", &aliases), Err("other".into()));
    }

    #[test]
    fn protected_version_rules() {
        let v = |n: u32| format!("/r/1/versions/{n}/");
        let pubs: Vec<Publication> = serde_json::from_value(json!([
            { "pulp_href": "/p/1/", "repository_version": v(1) },
            { "pulp_href": "/p/2/", "repository_version": v(2), "checkpoint": true },
            { "pulp_href": "/p/10/", "repository_version": v(10) },
            { "pulp_href": "/p/9/", "repository_version": v(9) },
        ]))
        .unwrap();
        let d = |name: &str, bind: Value| -> Distribution {
            let mut d = json!({ "pulp_href": format!("/d/{name}/"), "name": name, "base_path": name });
            d.as_object_mut().unwrap().extend(bind.as_object().unwrap().clone());
            serde_json::from_value(d).unwrap()
        };
        let why = |version: &str, ds: &[Distribution]| -> Vec<(String, &str)> {
            protecting(version, &pubs, ds).into_iter().map(|(d, w)| (d.name.clone(), w)).collect()
        };
        let pinned = [d("ver", json!({ "repository_version": v(3) })), d("pub", json!({ "publication": "/p/1/" }))];
        assert_eq!(why(&v(3), &pinned), [("ver".into(), "pinned to this version")]);
        assert_eq!(why(&v(1), &pinned), [("pub".into(), "pinned to a publication of this version")]);
        assert!(why(&v(4), &pinned).is_empty(), "nothing serves v4");

        // A checkpoint distribution protects every version with a checkpoint publication.
        let cp = [d("cp", json!({ "repository": "/r/1/", "checkpoint": true }))];
        assert_eq!(why(&v(2), &cp), [("cp".into(), "checkpoint distribution; this version has a checkpoint publication")]);
        assert!(why(&v(1), &cp).is_empty(), "v1 has no checkpoint publication");

        // A distribution that follows the repository protects its newest published version (by number: v10 > v9).
        let follow = [d("follow", json!({ "repository": "/r/1/" })), d("other", json!({ "repository": "/r/2/" }))];
        assert_eq!(why(&v(10), &follow), [("follow".into(), "follows the repository; this is its newest published version")]);
        assert!(why(&v(9), &follow).is_empty(), "an older published version");
        assert!(why(&v(11), &follow).is_empty(), "the latest version without a publication");
        assert!(why(&v(10), &cp).is_empty(), "a checkpoint distribution does not protect the newest published version");
        assert!(protecting(&v(1), &[], &follow).is_empty(), "no publication at all");
    }

    #[test]
    fn batches_fit_one_request_body() {
        let hrefs: Vec<String> = (0..60_000).map(|i| format!("/pulp/api/v3/content/deb/packages/{i:036}/")).collect();
        let b = batches(&hrefs);
        assert_eq!(b.len(), 3, "60,000 hrefs of 73 bytes");
        assert!(b.iter().all(|x| serde_json::to_string(x).unwrap().len() <= BATCH_BYTES));
        assert_eq!(b.concat(), hrefs, "every href once, in order");
        assert_eq!(batches(&hrefs[..10]).len(), 1);
        assert_eq!(batches(&[]), [&[] as &[String]], "an empty selection is still one request");
    }

    #[test]
    fn diff_reports_added_and_removed() {
        let a: BTreeSet<String> = ["x-1-1.noarch", "y-1-1.noarch"].iter().map(|s| s.to_string()).collect();
        let b: BTreeSet<String> = ["y-1-1.noarch", "z-1-1.noarch"].iter().map(|s| s.to_string()).collect();
        let d = diff(&a, &b);
        assert_eq!(d.added, vec!["z-1-1.noarch"]);
        assert_eq!(d.removed, vec!["x-1-1.noarch"]);
        assert_eq!(d.lines("v1", "v2")[0], "v1 -> v2: +1 added, -1 removed");
    }
}
