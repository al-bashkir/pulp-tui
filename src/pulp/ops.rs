//! Typed Pulp operations shared by the TUI and the workflow services. Every mutation
//! that Pulp runs as a task is awaited here with adaptive polling.

use super::client::{Mutation, Page, PulpClient, PulpError, Result, jitter};
use super::models::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// Callback for task progress (the TUI forwards these to the Task Center).
pub type OnTask<'a> = &'a (dyn Fn(&Task) + Send + Sync);

pub fn no_progress(_: &Task) {}

/// Poll a task until it reaches a final state. The interval grows while nothing changes
/// and resets on progress, so long tasks cost few requests.
pub async fn wait_task(c: &PulpClient, href: &str, on: OnTask<'_>) -> Result<Task> {
    let (min, max) = c.poll();
    let mut interval = min;
    let mut last: Option<Task> = None;
    loop {
        let t: Task = c.get(href, &[]).await?;
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
    loop {
        let g: TaskGroup = c.get(href, &[]).await?;
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

pub async fn find_by_name<T: serde::de::DeserializeOwned>(c: &PulpClient, collection: &str, name: &str) -> Result<Option<T>> {
    let p: Page<T> = c.list(collection, &[("name", name.into()), ("limit", "1".into())]).await?;
    Ok(p.results.into_iter().next())
}

/// File has only `mirror: bool`: any `mirror*` policy (the RPM values, from a mixed Content View) means mirror.
pub async fn sync(c: &PulpClient, t: RepoType, repo: &str, remote: Option<&str>, policy: &str) -> Result<Mutation> {
    let mut body = json!({});
    if let Some(r) = remote {
        body["remote"] = r.into();
    }
    match t {
        RepoType::Rpm => body["sync_policy"] = policy.into(),
        RepoType::File => body["mirror"] = policy.starts_with("mirror").into(),
    }
    c.post(&format!("{repo}sync/"), &body).await
}

/// Reuse the publication of a version if one exists (publishing is deterministic).
pub async fn ensure_publication(c: &PulpClient, t: RepoType, version: &str, on: OnTask<'_>) -> Result<String> {
    let p: Page<Publication> = c.list(&c.api(t.publications()), &[("repository_version", version.into()), ("limit", "1".into())]).await?;
    if let Some(pubn) = p.results.into_iter().next() {
        return Ok(pubn.pulp_href);
    }
    create(c, &c.api(t.publications()), &json!({ "repository_version": version }), "/publications/", on).await
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
        let owned = labels.as_object().into_iter().flatten().all(|(k, v)| d.pulp_labels.get(k).map(String::as_str) == v.as_str());
        if !owned {
            return Err(PulpError::Api { status: 409, message: format!("distribution {name} exists but lacks ownership labels {labels}") });
        }
        let adopt = match db {
            Some((key, id, aliases)) => label_owner(&d.pulp_labels, key, id, aliases)
                .map_err(|o| PulpError::Api {
                    status: 409,
                    message: format!("distribution {name} is owned by another pulp-tui database ({o})"),
                })?
                .then_some((key, id)),
            None => None,
        };
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
                all.insert(key.into(), id.into());
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

/// Who owns an object by its database-id label: `Ok(false)` this database, `Ok(true)` nobody
/// yet or an alias of this database (set the label), `Err(id)` another database.
pub fn label_owner(labels: &BTreeMap<String, String>, key: &str, id: &str, aliases: &[String]) -> std::result::Result<bool, String> {
    match labels.get(key) {
        Some(v) if v == id => Ok(false),
        Some(v) if !aliases.contains(v) => Err(v.clone()),
        _ => Ok(true),
    }
}

pub async fn packages_in(c: &PulpClient, version: &str) -> Result<Vec<Package>> {
    let fields = "pulp_href,name,epoch,version,release,arch,location_href,sha256";
    c.list_all(&c.api(RepoType::Rpm.content()), &[("repository_version", version.into()), ("fields", fields.into())], 2_000_000).await
}

pub async fn files_in(c: &PulpClient, version: &str) -> Result<Vec<FileContent>> {
    c.list_all(
        &c.api(RepoType::File.content()),
        &[("repository_version", version.into()), ("fields", "pulp_href,relative_path,sha256".into())],
        2_000_000,
    )
    .await
}

/// Hrefs of all content units (any type) of a version. The generic endpoint returns no
/// type field; callers classify by href path (`/content/rpm/packages/`, ...).
pub async fn content_in(c: &PulpClient, version: &str) -> Result<Vec<String>> {
    let v: Vec<Value> =
        c.list_all(&c.api("content/"), &[("repository_version", version.into()), ("fields", "pulp_href".into())], 2_000_000).await?;
    Ok(v.into_iter().filter_map(|x| x["pulp_href"].as_str().map(str::to_string)).collect())
}

/// Human-readable identity of each unit in a version (NEVRA or path+digest) for diffs.
pub async fn content_keys(c: &PulpClient, t: RepoType, version: Option<&str>) -> Result<BTreeSet<String>> {
    let Some(v) = version else { return Ok(BTreeSet::new()) };
    Ok(match t {
        RepoType::Rpm => packages_in(c, v).await?.iter().map(Package::nevra).collect(),
        RepoType::File => files_in(c, v).await?.iter().map(file_key).collect(),
    })
}

pub fn file_key(f: &FileContent) -> String {
    format!("{}  ({})", f.relative_path, f.sha256.as_deref().map(|s| &s[..s.len().min(12)]).unwrap_or("?"))
}

/// Fetch what a distribution actually serves and compare it with a repository version:
/// File manifests must list exactly the version's files; RPM metadata must be served and
/// the smallest package must be downloadable with the expected digest.
pub async fn verify_served(c: &PulpClient, t: RepoType, base_url: &str, version: &str, layout: &str) -> Result<String> {
    let base = if base_url.ends_with('/') { base_url.to_string() } else { format!("{base_url}/") };
    let fail = |m: String| Err(PulpError::Unsupported(m));
    let (status, body) = c.fetch_content(&format!("{base}{}", t.probe_file()), 64 << 20).await?;
    if status != 200 {
        return fail(format!("{} returned HTTP {status}", t.probe_file()));
    }
    match t {
        RepoType::File => {
            let served: BTreeSet<(String, String)> = String::from_utf8_lossy(&body)
                .lines()
                .filter_map(|l| {
                    let mut it = l.splitn(3, ',');
                    Some((it.next()?.to_string(), it.next()?.to_string()))
                })
                .collect();
            let want: BTreeSet<(String, String)> =
                files_in(c, version).await?.into_iter().map(|f| (f.relative_path, f.sha256.unwrap_or_default())).collect();
            if served != want {
                return fail(format!("served manifest differs from snapshot ({} served, {} expected)", served.len(), want.len()));
            }
            Ok(format!("PULP_MANIFEST matches {} files", want.len()))
        }
        RepoType::Rpm => {
            let q = [
                ("repository_version", version.to_string()),
                ("fields", "location_href,sha256".into()),
                ("ordering", "size_package".into()),
                ("limit", "1".into()),
            ];
            let pkgs: Page<Value> = c.list(&c.api(t.content()), &q).await?;
            let Some(p) = pkgs.results.first() else { return Ok("repomd.xml served (no packages)".into()) };
            let loc = p["location_href"].as_str().unwrap_or_default();
            // Publications use the layout; mirror_complete publications keep upstream paths.
            let candidates: Vec<String> = rpm_package_path(layout, loc).into_iter().chain([loc.to_string()]).collect();
            for path in &candidates {
                let (status, body) = c.fetch_content(&format!("{base}{path}"), 64 << 20).await?;
                if status == 200 && p["sha256"].as_str() == Some(hex::encode(Sha256::digest(&body)).as_str()) {
                    return Ok(format!("repomd.xml and {path} served with matching digest ({} packages)", pkgs.count));
                }
            }
            fail(format!("package {loc} not served with the expected digest (tried {})", candidates.join(", ")))
        }
    }
}

/// Path of a package inside a published RPM repository for the publication `layout`.
// shortcut: nested_by_digest paths are not derived, callers skip the package probe; add when needed.
pub fn rpm_package_path(layout: &str, location_href: &str) -> Option<String> {
    let file = location_href.rsplit('/').next().filter(|f| !f.is_empty())?;
    match layout {
        "flat" => Some(format!("Packages/{file}")),
        "nested_alphabetically" | "" => Some(format!("Packages/{}/{file}", file.chars().next()?.to_ascii_lowercase())),
        _ => None,
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
    if t == RepoType::File {
        fields.push(("relative_path", relative_path.unwrap_or(&file_name).into()));
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
    fn diff_reports_added_and_removed() {
        let a: BTreeSet<String> = ["x-1-1.noarch", "y-1-1.noarch"].iter().map(|s| s.to_string()).collect();
        let b: BTreeSet<String> = ["y-1-1.noarch", "z-1-1.noarch"].iter().map(|s| s.to_string()).collect();
        let d = diff(&a, &b);
        assert_eq!(d.added, vec!["z-1-1.noarch"]);
        assert_eq!(d.removed, vec!["x-1-1.noarch"]);
        assert_eq!(d.lines("v1", "v2")[0], "v1 -> v2: +1 added, -1 removed");
    }
}
