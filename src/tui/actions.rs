//! Actions behind the menus: forms, previews, confirmations and background jobs.
//! Content View and administration actions live in `actions_cv` / `actions_admin`.

use super::app::{App, Apply, Ctx, Level, Screen, item_key};
use super::form::{Field, FieldKind, Form, Source, format_labels, parse_labels};
use super::kinds::{Act, Kind, s, short};
use crate::config::{AuthMethod, Database, LOCAL_DATABASE, Profile, database_keyring_user};
use crate::filters::{Selector, filter_help, parse_rules};
use crate::pulp::client::Page;
use crate::pulp::models::{Distribution, Publication, Task};
use crate::pulp::ops::{self, repo_of_version, version_number};
use crate::pulp::{Capabilities, Feature, Mutation, PulpClient, PulpError, RepoType, deb, file, rpm};
use crate::store::Store;
use anyhow::{Context, anyhow, bail};
use serde_json::{Value, json};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;

pub fn ap(f: impl FnOnce(&mut App) + Send + 'static) -> anyhow::Result<Apply> {
    Ok(Box::new(f))
}

/// Run a mutation in the background. A returned task goes to the Task Center; a
/// synchronous result reloads the current list.
pub fn submit<F, Fut>(app: &mut App, label: impl Into<String>, f: F)
where
    F: FnOnce(PulpClient) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<Mutation>> + Send + 'static,
{
    let label = label.into();
    let l2 = label.clone();
    app.job(label, move |ctx| async move {
        let m = f(ctx.client.clone()).await?;
        ap(move |app| submitted(app, m, &l2))
    });
}

/// Report what a mutation returned, as `submit` does.
fn submitted(app: &mut App, m: Mutation, label: &str) {
    match m {
        Mutation::Task(h) => {
            app.track_task(&h, label);
            app.notify(Level::Info, format!("submitted: {label} (see Tasks)"));
        }
        Mutation::TaskGroup(g) => app.ok(format!("{label}: task group {g}")),
        _ => {
            app.ok(format!("{label}: done"));
            app.reload();
        }
    }
}

/// `submitted`, then wait for the task in a second job and give the finished task to `done`.
fn follow(app: &mut App, m: Mutation, label: String, done: impl FnOnce(&mut App, Task) + Send + 'static) {
    let Mutation::Task(h) = &m else { return submitted(app, m, &label) };
    let h = h.clone();
    submitted(app, m, &label);
    app.job(label, move |ctx| async move {
        let on = ctx.on_task();
        match ops::wait_task(&ctx.client, &h, &on).await {
            Ok(t) => ap(move |app| done(app, t)),
            // The Task Center already reports it with Pulp's message.
            Err(PulpError::TaskFailed { .. }) => ap(|_| {}),
            Err(e) => Err(e.into()),
        }
    });
}

/// A Remove task ended: a Content screen that still shows the browsed version moves to the new one.
fn content_removed(app: &mut App, old: &str, n: usize, t: &Task) {
    let Some(new) = t.created_matching("/versions/") else {
        return app.ok(format!("removed {n} unit(s): no new version (the latest version did not have them)"));
    };
    let v = version_number(&new);
    match &app.screen {
        Screen::List(Kind::Content { version, ctype }) if version == old => {
            app.replace(Kind::Content { version: new, ctype: *ctype });
            app.ok(format!("removed {n} unit(s): now showing version {v}"));
        }
        _ => app.ok(format!("removed {n} unit(s): new version {v}")),
    }
}

pub async fn href_by_name(c: &PulpClient, collection: &str, name: &str) -> anyhow::Result<String> {
    ops::find_by_name::<Value>(c, &c.api(collection), name)
        .await?
        .and_then(|v| v["pulp_href"].as_str().map(str::to_string))
        .ok_or_else(|| anyhow!("no object named {name:?} in {collection}"))
}

fn version_of(repo: &str, number: i64) -> String {
    format!("{repo}versions/{number}/")
}

/// The Content View whose label a Pulp object carries: it changes through the Content View actions only.
fn cv_owner(item: &Value) -> Option<String> {
    item["pulp_labels"].get(crate::cv::LABEL_CV).and_then(Value::as_str).map(str::to_string)
}

fn snapshot_refusal(repo: &Value) -> Option<String> {
    cv_owner(repo).map(|cv| {
        format!(
            "refused: {} is a snapshot repository of Content View {cv}; change its source repositories and publish a new version",
            s(repo, "name")
        )
    })
}

fn cv_distribution_refusal(d: &Value) -> anyhow::Result<()> {
    match cv_owner(d) {
        Some(cv) => bail!("refused: distribution {} belongs to Content View {cv}; promote or roll back the view instead", s(d, "name")),
        None => Ok(()),
    }
}

/// "  name (URL)" of each distribution that `serves` matches.
fn served_by(ds: &[Distribution], serves: impl Fn(&Distribution) -> bool) -> Vec<String> {
    ds.iter().filter(|d| serves(d)).map(|d| format!("  {} ({})", d.name, d.base_url.as_deref().unwrap_or(&d.base_path))).collect()
}

/// What a distribution serves: (repository version, its publication or `Null`, how). `None`: nothing.
/// A repository binding serves the newest publication of the repository (pulpcore orders by repository
/// version, then creation), which is not the latest version when that one has no publication.
async fn served_now(c: &PulpClient, t: RepoType, d: &Value) -> anyhow::Result<Option<(String, Value, String)>> {
    if let Some(p) = d["publication"].as_str() {
        let pv: Value = c.get(p, &[]).await?;
        return Ok(Some((s(&pv, "repository_version"), pv, format!("pinned to publication {}", short(p)))));
    }
    if let Some(v) = d["repository_version"].as_str() {
        return Ok(Some((v.to_string(), Value::Null, "pinned to repository version".into())));
    }
    let Some(r) = d["repository"].as_str() else { return Ok(None) };
    let pubs: Vec<Value> = c.list_all(&c.api(t.publications()), &[("repository", r.into())], 100_000).await?;
    let newest = pubs.into_iter().max_by_key(|p| (version_number(&s(p, "repository_version")), s(p, "pulp_created")));
    Ok(newest.map(|p| {
        let how = format!("follows repository {}: newest publication {}", short(r), short(&s(&p, "pulp_href")));
        (s(&p, "repository_version"), p, how)
    }))
}

pub(super) fn caps(app: &App) -> Option<std::sync::Arc<Capabilities>> {
    app.conn.as_ref().map(|c| c.caps.clone())
}

/// The connected server lacks `f`: report "not supported by this server: ..." and return true.
fn unsupported(app: &mut App, f: Feature) -> bool {
    let Some(Err(e)) = caps(app).map(|c| c.require(f)) else { return false };
    app.error(e.to_string());
    true
}

/// The sync policies of a repository type: the server's `SyncPolicyEnum` for RPM, if it has one.
pub fn sync_policies(caps: Option<&Capabilities>, t: RepoType) -> Vec<String> {
    match t {
        RepoType::Rpm => caps
            .and_then(|c| c.enum_values("SyncPolicyEnum").map(|e| e.to_vec()))
            .unwrap_or_else(|| vec!["additive".into(), "mirror_complete".into(), "mirror_content_only".into()]),
        RepoType::File | RepoType::Deb => vec!["additive".into(), "mirror".into()],
    }
}

/// Runs `go` at once, or after the operator confirms the mirror warning when `policy` is a mirror policy.
pub fn confirm_mirror(app: &mut App, policy: &str, go: impl FnOnce(&mut App) + 'static) {
    if !policy.contains("mirror") {
        return go(app);
    }
    let mut lines = vec![
        "Mirror sync makes the NEW repository version match the upstream exactly:".into(),
        "content that is not upstream is removed from the new version.".into(),
        "Older versions are not changed and can still be published (rollback).".into(),
    ];
    if policy == "mirror_complete" {
        lines.push("mirror_complete also publishes the upstream metadata unchanged (a publication is created).".into());
    }
    app.confirm("Confirm mirror sync", lines, go);
}

pub fn pretty(v: &Value) -> Vec<String> {
    serde_json::to_string_pretty(v).unwrap_or_default().lines().map(str::to_string).collect()
}

/// OSC 52 clipboard copy: works over SSH and inside containers, no dependency needed.
pub fn copy_to_clipboard(s: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(osc52(s, std::env::var_os("TMUX").is_some()).as_bytes());
    let _ = out.flush();
}

/// tmux drops a program's OSC 52 unless `set-clipboard on` (the default is `external`); the copy in a
/// passthrough sequence gets to the outer terminal when `allow-passthrough on`.
fn osc52(s: &str, tmux: bool) -> String {
    let seq = format!("\x1b]52;c;{}\x07", crate::pulp::client::base64_encode(s.as_bytes()));
    if tmux { format!("{seq}\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b")) } else { seq }
}

pub fn run(app: &mut App, act: Act, item: Value) {
    let kind = app.list.kind.clone();
    if act.mutates(&kind, app.shared()) && app.read_only() {
        app.error("read-only mode: this action changes the server");
        return;
    }
    if !matches!((&kind, act), (Kind::Logs { .. }, Act::Inspect)) {
        let name = ["name", "username"].iter().map(|k| s(&item, k)).find(|n| !n.is_empty());
        let name = name.unwrap_or_else(|| short(&s(&item, "pulp_href")));
        // An action without an item (Create, ...) has no target, even when a row is selected.
        let on = if act.needs_item() && !name.is_empty() { format!(" on {name}") } else { String::new() };
        crate::session_log::record("action", app.profile.as_deref().unwrap_or_default(), format!("{}{on}", act.label()), vec![]);
    }
    match (&kind, act) {
        (Kind::Logs { .. }, Act::Inspect) => app.text(s(&item, "summary"), log_lines(&item)),
        (_, Act::Inspect) => inspect(app, &kind, item),
        (Kind::Profiles, _) => profile_action(app, act, item),
        (Kind::Repos(t, _), _) => repo_action(app, *t, act, item),
        (Kind::Remotes(t), _) => remote_action(app, *t, act, item),
        (Kind::Versions(_), Act::PruneOrphans) => prune_orphans(app),
        (Kind::Versions(_), _) => version_action(app, act, item),
        (Kind::Content { version, .. }, _) => content_action(app, version.clone(), act),
        (Kind::Publications(t), _) => publication_action(app, *t, act, item),
        (Kind::Distributions(t), _) => distribution_action(app, *t, act, item),
        (Kind::Tasks | Kind::ServerTasks, _) => task_action(app, act, item),
        (Kind::ContentViews | Kind::CvVersions { .. } | Kind::Environments, _) => super::actions_cv::run(app, act, item),
        _ => super::actions_admin::run(app, act, item),
    }
}

/// Detail of a Logs row: the stored lines, the summary for rows without any, a note for a dropped row.
pub fn log_lines(row: &Value) -> Vec<String> {
    match row["id"].as_u64().and_then(crate::session_log::detail) {
        Some(d) if d.is_empty() => vec![s(row, "summary")],
        Some(d) => d,
        None => vec!["This entry is older than the last 1000 and was dropped.".into()],
    }
}

fn inspect(app: &mut App, kind: &Kind, mut item: Value) {
    let href = s(&item, "pulp_href");
    let title = match kind {
        Kind::CvVersions { name, .. } => format!("{name} v{}", s(&item, "number")),
        _ => s(&item, "name"),
    };
    let title = if title.is_empty() { short(&href) } else { title };
    // Lists may exclude heavy fields; fetch the complete object for inspection.
    if !href.is_empty() && kind.needs_connection() && !matches!(kind, Kind::Tasks) {
        app.job(format!("inspect {title}"), move |ctx| async move {
            let mut v: Value = ctx.client.get(&href, &[]).await?;
            // Remote headers and tokens are readable on GET.
            crate::session_log::redact(&mut v);
            ap(move |app| app.text(format!("Inspect {title}"), pretty(&v)))
        });
    } else {
        let lines = if let Kind::Tasks = kind {
            task_lines(&item)
        } else {
            crate::session_log::redact(&mut item);
            pretty(&item)
        };
        app.text(format!("Inspect {title}"), lines);
    }
}

pub(super) fn task_lines(item: &Value) -> Vec<String> {
    let mut out = vec![
        format!("label: {}", s(item, "label")),
        format!("href:  {}", s(item, "pulp_href")),
        format!("state: {}", s(item, "state")),
        String::new(),
    ];
    if let Some(t) = item.get("task") {
        for r in t["progress_reports"].as_array().into_iter().flatten() {
            let n = match (r["done"].as_u64(), r["total"].as_u64()) {
                (Some(d), Some(t)) if t > 0 => format!("{d}/{t}"),
                (Some(d), _) => d.to_string(),
                _ => String::new(),
            };
            out.push(format!("  [{}] {} {n}", s(r, "state"), s(r, "message")));
        }
        out.push(String::new());
        out.extend(pretty(t));
    }
    out
}

// ---- profiles ----------------------------------------------------------------------

/// `inherited` is the top-level `[database]`; `local_db` is the local file, shown for "no external database".
fn profile_form(name: Option<&str>, p: &Profile, inherited: Option<&Database>, local_db: &Path) -> Form {
    let auth = ["basic", "bearer", "none"].map(String::from).to_vec();
    let mut fields = vec![];
    if name.is_none() {
        fields.push(Field::text("name", "Profile name").required());
    }
    let opt = |o: &Option<std::path::PathBuf>| o.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
    // The stored text, unchanged: an untouched form must store no `database` block.
    let db = p.database.as_ref().or(inherited).filter(|d| !d.is_local());
    let db_url = db.map_or_else(|| local_db.display().to_string(), |d| d.url.clone());
    fields.extend([
        Field::text("url", "Server URL").required().value(&p.url).help("scheme://host[:port], e.g. https://pulp.example.com"),
        Field::text("api_root", "API root").value(&p.api_root).help("usually /pulp/"),
        Field::text("domain", "Domain").value(p.domain.clone().unwrap_or_default()).help("Pulp domain (only when domains are enabled)"),
        Field::new("auth", "Auth", FieldKind::Choice(auth)).value(match p.auth {
            AuthMethod::Basic => "basic",
            AuthMethod::Bearer => "bearer",
            AuthMethod::None => "none",
        }),
        Field::text("username", "Username").value(p.username.clone().unwrap_or_default()),
        Field::text("secret", "Secret reference")
            .value(p.secret.clone().unwrap_or_default())
            .help("env:VAR, file:/path or keyring (never the secret itself)"),
        Field::text("ca_cert", "CA bundle (PEM path)").value(opt(&p.ca_cert)),
        Field::text("client_cert", "mTLS client cert (PEM path)").value(opt(&p.client_cert)),
        Field::text("client_key", "mTLS client key (PEM path)").value(opt(&p.client_key)),
        Field::text("proxy", "HTTP(S) proxy URL").value(p.proxy.clone().unwrap_or_default()),
        Field::text("database", "Database")
            .value(db_url)
            .help("local, a file path, postgres://user@host:5432/db or sqlite:///path; never a password"),
        Field::text("db_secret", "Database secret")
            .value(db.and_then(|d| d.secret.clone()).unwrap_or_default())
            .help("env:VAR, file:/path or keyring (keyring user [database]:<profile>)"),
        Field::text("db_ca_cert", "Database CA (PEM path)").value(opt(&db.and_then(|d| d.ca_cert.clone()))),
        Field::new("timeout_secs", "Timeout (s)", FieldKind::Number).value(p.timeout_secs.to_string()),
        Field::new("read_only", "Read-only", FieldKind::Bool).value(p.read_only.to_string()),
        Field::new("production", "Production (red banner)", FieldKind::Bool).value(p.production.to_string()),
    ]);
    Form::new(if name.is_some() { "Edit server profile" } else { "New server profile" }, fields)
        .note("TLS certificates are always verified. Secrets are referenced, never stored in the config file.")
}

/// The `database` field as a block. `None`: the same database the profile would get without a block.
fn database_from_form(f: &Form, inherited: Option<&Database>, local_db: &Path) -> Option<Database> {
    let (v, local) = (f.get("database"), local_db.display().to_string());
    let marker = Database { url: LOCAL_DATABASE.into(), secret: None, ca_cert: None };
    let db = if v.is_empty() || v == LOCAL_DATABASE || v == local || v.strip_prefix("sqlite://") == Some(local.as_str()) {
        marker.clone()
    } else {
        let url = if v.starts_with('/') { format!("sqlite://{v}") } else { v.to_string() };
        Database { url, secret: f.opt("db_secret").map(Into::into), ca_cert: f.opt("db_ca_cert").map(Into::into) }
    };
    (db != *inherited.unwrap_or(&marker)).then_some(db)
}

fn profile_from_form(f: &Form, inherited: Option<&Database>, local_db: &Path) -> Result<Profile, String> {
    let path = |k: &str| f.opt(k).map(std::path::PathBuf::from);
    let p = Profile {
        url: f.get("url").trim_end_matches('/').to_string(),
        api_root: f.opt("api_root").unwrap_or("/pulp/").to_string(),
        domain: f.opt("domain").map(Into::into),
        auth: match f.get("auth") {
            "bearer" => AuthMethod::Bearer,
            "none" => AuthMethod::None,
            _ => AuthMethod::Basic,
        },
        username: f.opt("username").map(Into::into),
        secret: f.opt("secret").map(Into::into),
        ca_cert: path("ca_cert"),
        client_cert: path("client_cert"),
        client_key: path("client_key"),
        proxy: f.opt("proxy").map(Into::into),
        timeout_secs: f.number("timeout_secs").unwrap_or(30).max(1) as u64,
        read_only: f.bool("read_only"),
        production: f.bool("production"),
        database: database_from_form(f, inherited, local_db),
    };
    p.validate().map_err(|e| format!("{e:#}"))?;
    Ok(p)
}

fn save_config(app: &App, cfg: &crate::config::Config) -> Result<(), String> {
    cfg.save(&app.cfg_path).map_err(|e| format!("cannot save {}: {e:#}", app.cfg_path.display()))
}

/// After a save that changed the database of `name`: open it now, so a wrong URL or secret shows up here
/// and a database without the schema gets the offer to migrate it.
fn check_database(app: &mut App, name: &str) {
    let read_only = app.cfg.profiles.get(name).is_some_and(|p| p.read_only);
    let n = name.to_string();
    app.with_store(name, read_only, app.cfg.database_for(name), move |app, r| {
        if let Err(e) = r {
            app.notify(Level::Warn, format!("profile {n}: {e}"));
        }
    });
}

/// Save the config file with the profile, then use it and reload. A failed save leaves the config in memory as it was.
fn put_profile(app: &mut App, name: &str, p: Profile) -> Result<(), String> {
    let mut next = app.cfg.clone();
    next.profiles.insert(name.to_string(), p);
    save_config(app, &next)?;
    app.cfg = next;
    app.reload();
    Ok(())
}

fn location(db: &Option<(Database, String)>, local_db: &Path) -> String {
    db.as_ref().map_or_else(|| format!("the local file {}", local_db.display()), |(d, _)| d.display_url())
}

/// Save a profile whose database changed, and open the new one.
fn switch_database(app: &mut App, name: &str, p: Profile) -> Result<(), String> {
    put_profile(app, name, p)?;
    app.ok(format!("profile {name} saved (reconnect to apply)"));
    check_database(app, name);
    Ok(())
}

/// Edit changed the database of `name`. Count what the old one holds for this server: nothing, and the profile
/// is saved; else offer to copy it first. Nothing is written to the old database, and the profile stays as it
/// was until the copy succeeds.
fn change_database(app: &mut App, name: String, p: Profile, old: Option<(Database, String)>, new: Option<(Database, String)>) {
    let cur = app.cfg.profiles.get(&name).cloned().unwrap_or_else(|| p.clone());
    let (open, label) = (app.open_store(old.clone()), format!("database impact of {name}"));
    app.spawn(label, false, async move {
        let counted = async {
            let st = open.await?;
            let scope = st.scope(&name, &cur);
            let views = st.content_views(&scope).await?;
            let mut versions = 0;
            for v in &views {
                versions += st.versions(v.id).await?.iter().filter(|x| x.state != "deleted").count();
            }
            let envs = st.environments(&scope).await?.iter().filter(|e| e.prior_id.is_some()).count();
            anyhow::Ok((st, scope, [views.len(), versions, envs]))
        }
        .await;
        ap(move |app| match counted {
            Ok((_, _, [0, _, 0])) => {
                if let Err(e) = switch_database(app, &name, p) {
                    app.error(e);
                }
            }
            Ok((st, scope, counts)) => offer_copy(app, name, p, (old, new), Some((st, scope)), Ok(counts)),
            Err(e) => offer_copy(app, name, p, (old, new), None, Err(format!("{e:#}"))),
        })
    });
}

type Databases = (Option<(Database, String)>, Option<(Database, String)>);

/// The choice form of `change_database`. `old_store` is `None` when the old database cannot be read.
fn offer_copy(
    app: &mut App,
    name: String,
    p: Profile,
    (old, new): Databases,
    old_store: Option<(Arc<Store>, String)>,
    counts: Result<[usize; 3], String>,
) {
    let read_only = p.read_only && Act::CopyDb.mutates(&Kind::Profiles, true);
    let copy = old_store.is_some() && !read_only;
    let (from, to) = (location(&old, &app.local_db), location(&new, &app.local_db));
    let mut choices = vec!["switch without copy".to_string()];
    if copy {
        choices.insert(0, "copy and switch".into());
    }
    let mut form = Form::new(format!("Database of {name} changes"), vec![Field::new("choice", "On save", FieldKind::Choice(choices))])
        .note(format!("Old: {from}"))
        .note(format!("New: {to}"));
    form = match counts {
        Ok([v, n, e]) => form.note(format!("{v} Content View(s), {n} version(s), {e} environment(s) of this server in the old database.")),
        Err(e) => form.note(format!("The old database is unavailable ({e}); nothing can be counted or copied.")),
    };
    if copy {
        form = form.note(
            "copy and switch: copy them, then save. The copies keep ownership of the existing Pulp objects. The old database is only read.",
        );
    }
    form = form.note("switch without copy: save only. The old data stays in the old database; the new one refuses its Pulp objects, so publish and promote of these views fail there.");
    if read_only {
        form = form.note("Read-only profile: copy is not offered.");
    }
    app.form(form.note("Esc: cancel; the profile is not changed."), move |app, f| {
        let (Some((old_st, old_scope)), "copy and switch") = (&old_store, f.get("choice")) else {
            return switch_database(app, &name, p.clone());
        };
        let (n, p, to, old_st, old_scope) = (name.clone(), p.clone(), to.clone(), old_st.clone(), old_scope.clone());
        app.with_store(&name, p.read_only, new.clone(), move |app, r| {
            let st = match r {
                Ok(st) => st,
                // Cut the CLI hint of `with_store`: it names the saved database, and this profile is not saved.
                Err(e) => return app.error(format!("profile {n} not changed: {}", e.split(" (open Servers").next().unwrap_or(&e))),
            };
            let (n, p, to, old_st, old_scope) = (n.clone(), p.clone(), to.clone(), old_st.clone(), old_scope.clone());
            app.spawn(format!("copy {n} data"), false, async move {
                let r = st.copy_from(&old_st, &old_scope, &st.scope(&n, &p)).await?;
                ap(move |app| {
                    let copied = format!(
                        "copied {} content view(s), {} version(s), {} environment(s) into {to}",
                        r.views, r.versions, r.environments
                    );
                    match put_profile(app, &n, p) {
                        Ok(()) => app.ok(format!("{copied}; profile {n} saved (reconnect to apply)")),
                        Err(e) => app.error(format!("{copied}, but profile {n} was not saved: {e}")),
                    }
                })
            });
        });
        Ok(())
    });
}

fn profile_action(app: &mut App, act: Act, item: Value) {
    let name = s(&item, "name");
    match act {
        Act::Connect => app.connect(&name),
        Act::TestProfile => {
            let Some(p) = app.cfg.profiles.get(&name).cloned() else { return };
            let settings = app.cfg.settings.clone();
            let fut = async move {
                let c = PulpClient::new(&name, &p, &settings)?;
                let caps = Capabilities::detect(&c).await?;
                let mut lines = vec![format!("Connection to {} OK", p.url)];
                lines.push(format!(
                    "user: {}",
                    caps.user.as_ref().map(|u| format!("{} (staff: {:?})", u.username, u.is_staff)).unwrap_or("anonymous / unknown".into())
                ));
                lines.push(format!("content served at: {}", caps.content_base().unwrap_or_default()));
                lines.push(String::new());
                lines.extend(caps.summary_lines());
                ap(move |app| app.text(format!("Test {name}"), lines))
            };
            app.spawn("test connection".into(), false, fut);
        }
        Act::Create => {
            let form = profile_form(None, &Profile::default(), app.cfg.database.as_ref(), &app.local_db);
            app.form(form, |app, f| {
                let name = f.get("name").to_string();
                crate::store::validate_name(&name).map_err(|e| e.to_string())?;
                if app.cfg.profiles.contains_key(&name) {
                    return Err(format!("profile {name} exists"));
                }
                let before = app.cfg.database_for(&name);
                let mut next = app.cfg.clone();
                next.profiles.insert(name.clone(), profile_from_form(f, app.cfg.database.as_ref(), &app.local_db)?);
                next.default_profile.get_or_insert_with(|| name.clone());
                save_config(app, &next)?;
                app.cfg = next;
                app.ok(format!("profile {name} saved"));
                if app.cfg.database_for(&name) != before {
                    check_database(app, &name);
                }
                app.reload();
                Ok(())
            });
        }
        Act::Edit => {
            let Some(p) = app.cfg.profiles.get(&name).cloned() else { return };
            let form = profile_form(Some(&name), &p, app.cfg.database.as_ref(), &app.local_db);
            app.form(form, move |app, f| {
                let edited = profile_from_form(f, app.cfg.database.as_ref(), &app.local_db)?;
                let old = app.cfg.database_for(&name);
                let mut next = app.cfg.clone();
                next.profiles.insert(name.clone(), edited.clone());
                let new = next.database_for(&name);
                // The place decides the copy offer; another secret or CA for the same URL is a plain save.
                let place = |d: &Option<(Database, String)>| d.as_ref().map(|(d, _)| d.url.clone());
                if place(&old) != place(&new) {
                    change_database(app, name.clone(), edited, old, new);
                } else {
                    put_profile(app, &name, edited)?;
                    app.ok(format!("profile {name} saved (reconnect to apply)"));
                    if old != new {
                        check_database(app, &name);
                    }
                }
                Ok(())
            });
        }
        Act::Delete => {
            let kept = match app.cfg.database_for(&name) {
                Some((d, _)) => format!("stay in the database {} (they belong to the server, not to the profile name)", d.display_url()),
                None => "stay in the local database file".into(),
            };
            let lines = vec![
                format!("Remove profile {name} from the config file."),
                format!("Pulp is not changed. Content View definitions stored for this profile {kept}."),
            ];
            let n = name.clone();
            app.confirm_danger(format!("Delete profile {name}"), lines, &name, move |app| {
                let mut next = app.cfg.clone();
                next.profiles.remove(&n);
                if next.default_profile.as_deref() == Some(n.as_str()) {
                    next.default_profile = None;
                }
                match save_config(app, &next) {
                    Ok(()) => app.cfg = next,
                    Err(e) => app.error(e),
                }
                app.reload();
            });
        }
        Act::StoreSecret => {
            let mut fields = vec![Field::new("secret", "Password / token", FieldKind::Secret).required()];
            if app.cfg.profiles.get(&name).and_then(|p| p.database.as_ref()).is_some_and(|d| !d.is_local()) {
                let target = ["Pulp server", "database"].map(String::from).to_vec();
                fields.insert(0, Field::new("target", "Secret of", FieldKind::Choice(target)));
            }
            let form = Form::new(format!("Store secret for {name} in the system keyring"), fields)
                .note("The matching secret reference (profile secret or database secret) must be keyring.");
            app.form(form, move |app, f| {
                let user = if f.get("target") == "database" { database_keyring_user(&name) } else { name.clone() };
                let (n, secret) = (user, f.get("secret").to_string());
                let fut = async move {
                    tokio::task::spawn_blocking(move || crate::config::keyring_set(&n, &secret)).await??;
                    ap(|app| app.ok("secret stored in keyring"))
                };
                app.spawn("keyring".into(), false, fut);
                Ok(())
            });
        }
        Act::ExportCv => {
            let profile = if name.is_empty() { app.profile.clone().unwrap_or_default() } else { name };
            app.input(
                format!("Export Content View definitions of {profile} to file"),
                &format!("pulp-tui-{profile}-cv.json"),
                move |app, path| {
                    let target = app.profile_store(&profile);
                    let fut = async move {
                        let (store, scope) = target.await?;
                        let ex = store.export(&scope).await?;
                        tokio::fs::write(&path, serde_json::to_vec_pretty(&ex)?).await.with_context(|| format!("write {path}"))?;
                        ap(move |app| {
                            app.ok(format!("exported {} content view(s) to {path} (no secrets, no Pulp state)", ex.content_views.len()))
                        })
                    };
                    app.spawn("export".into(), false, fut);
                },
            );
        }
        Act::ImportCv => {
            let profile = app.profile.clone().unwrap_or_default();
            app.input(format!("Import Content View definitions into {profile} from file"), "", move |app, path| {
                let target = app.profile_store(&profile);
                let fut = async move {
                    let ex: crate::store::Export =
                        serde_json::from_slice(&tokio::fs::read(&path).await.with_context(|| format!("read {path}"))?)?;
                    let (store, scope) = target.await?;
                    let (cvs, envs, warnings) = store.import(&scope, &ex).await?;
                    ap(move |app| {
                        let t = format!("imported {cvs} content view(s), {envs} environment(s); existing names were kept");
                        if warnings.is_empty() { app.ok(t) } else { app.notify(Level::Warn, format!("{t}; {}", warnings.join("; "))) }
                    })
                };
                app.spawn("import".into(), false, fut);
            });
        }
        _ => {}
    }
}

// ---- repositories --------------------------------------------------------------------

/// Repository form fields that only some plugins have.
fn repo_extra_fields(t: RepoType, caps: Option<&Capabilities>, v: &Value) -> Vec<Field> {
    let has = |f: &str| caps.is_none_or(|c| c.has_field(t.repository_schema(), f));
    let mut fields = Vec::new();
    match t {
        RepoType::Rpm => {
            if has("retain_package_versions") {
                fields.push(
                    Field::new("retain_package_versions", "Retain package versions", FieldKind::Number)
                        .value(s(v, "retain_package_versions"))
                        .help("0 = keep all"),
                );
            }
            if has("checksum_type") {
                let mut c: Vec<String> =
                    caps.and_then(|c| c.enum_values("PackageChecksumTypeEnum")).map(|e| e.to_vec()).unwrap_or_default();
                c.retain(|x| !matches!(x.as_str(), "unknown" | "md5" | "sha1"));
                c.insert(0, String::new());
                fields.push(Field::new("checksum_type", "Checksum type", FieldKind::Choice(c)).value(s(v, "checksum_type")));
            }
            if has("metadata_signing_service") {
                fields.push(
                    Field::pick("metadata_signing_service", "Metadata signing service", Source::SigningServices)
                        .value(s(v, "metadata_signing_service")),
                );
            }
        }
        RepoType::File => {
            if has("manifest") {
                fields.push(Field::text("manifest", "Manifest file name").value(if v.is_null() {
                    "PULP_MANIFEST".into()
                } else {
                    s(v, "manifest")
                }));
            }
        }
        RepoType::Deb => {}
    }
    fields
}

/// `remote` is the name of the repository's default remote ("" = none, or not readable).
fn repo_form(t: RepoType, caps: Option<&Capabilities>, item: Option<&Value>, remote: &str) -> Form {
    let v = item.cloned().unwrap_or(Value::Null);
    let mut fields = vec![
        Field::text("name", "Name").required().value(s(&v, "name")),
        Field::text("description", "Description").value(s(&v, "description")),
        Field::pick("remote", "Remote", Source::Remotes(t)).help("default remote for sync; empty = none").value(remote),
        Field::new("retain_repo_versions", "Retain versions", FieldKind::Number)
            .value(s(&v, "retain_repo_versions"))
            .help("empty = keep all versions"),
        Field::new("autopublish", "Autopublish", FieldKind::Bool).value(if v["autopublish"] == true { "true" } else { "false" }),
        Field::text("labels", "Labels (k=v,...)").value(format_labels(&v["pulp_labels"])),
    ];
    fields.extend(repo_extra_fields(t, caps, &v));
    let mut form =
        Form::new(if item.is_some() { format!("Edit {} repository", t.label()) } else { format!("New {} repository", t.label()) }, fields);
    if let Some(r) = v["remote"].as_str()
        && remote.is_empty()
    {
        form = form.note(format!("current remote {r} could not be read; an empty Remote keeps it"));
    }
    form
}

/// JSON body from a repository form; on edit only changed fields are sent.
/// `remote` holds the remote's name (the caller sends its href), or null when cleared on edit.
fn repo_body(f: &Form, edit: bool) -> Result<Value, String> {
    let mut b = json!({});
    for fld in &f.fields {
        if edit && !fld.changed() {
            continue;
        }
        let v = fld.value.trim();
        b[fld.key.as_str()] = match (fld.key.as_str(), &fld.kind) {
            ("labels", _) => {
                b["pulp_labels"] = parse_labels(v)?;
                continue;
            }
            (_, FieldKind::Bool) => Value::Bool(v == "true"),
            (_, FieldKind::Number) if v.is_empty() && edit && fld.key == "retain_repo_versions" => Value::Null,
            (_, FieldKind::Number) if v.is_empty() => continue,
            (_, FieldKind::Number) => v.parse::<i64>().map_err(|_| format!("{} must be a number", fld.label))?.into(),
            (_, _) if v.is_empty() && edit => Value::Null,
            (_, _) if v.is_empty() => continue,
            _ => v.into(),
        };
    }
    Ok(b)
}

/// Repository Create, or Edit of `item`.
fn repo_edit(app: &mut App, t: RepoType, edit: bool, item: Value) {
    let (href, name) = (s(&item, "pulp_href"), s(&item, "name"));
    let current = item["remote"].as_str().filter(|_| edit).map(str::to_string);
    let caps = caps(app);
    let open = move |app: &mut App, remote: String| {
        let form = repo_form(t, caps.as_deref(), edit.then_some(&item), &remote);
        app.form(form, move |app, f| {
            let mut body = repo_body(f, edit)?;
            let remote = body["remote"].as_str().map(str::to_string);
            let href = href.clone();
            let label = format!("{} repository {}", if edit { "update" } else { "create" }, f.get("name"));
            submit(app, label, move |c| async move {
                if let Some(r) = remote {
                    body["remote"] = href_by_name(&c, t.remotes(), &r).await?.into();
                }
                Ok(if edit { c.patch(&href, &body).await? } else { c.post(&c.api(t.repositories()), &body).await? })
            });
            Ok(())
        });
    };
    match current {
        // The picker holds names: read the current remote's name so the field shows it, as Sync does.
        Some(r) => app.job(format!("remote of {name}"), move |ctx| async move {
            let v = ctx.client.get::<Value>(&r, &[("fields", "name".into())]).await;
            let current = v.map(|v| s(&v, "name")).unwrap_or_default();
            ap(move |app| open(app, current))
        }),
        None => open(app, String::new()),
    }
}

fn repo_action(app: &mut App, t: RepoType, act: Act, item: Value) {
    let href = s(&item, "pulp_href");
    let name = s(&item, "name");
    if let Some(e) = snapshot_refusal(&item)
        && matches!(act, Act::Sync | Act::Upload | Act::CopyContent | Act::Edit)
    {
        app.error(e);
        return;
    }
    match act {
        Act::Versions => {
            app.last_repo = Some((t, href.clone(), name.clone()));
            app.push(Kind::Versions(Some((href, name))));
        }
        Act::Content => match item["latest_version_href"].as_str() {
            Some(v) => app.push(Kind::Content { version: v.into(), ctype: 0 }),
            None => app.error("repository has no versions"),
        },
        Act::Create | Act::Edit => repo_edit(app, t, act == Act::Edit, item),
        Act::Delete => {
            app.job(format!("impact of deleting {name}"), move |ctx| async move {
                let c = &ctx.client;
                // Fails closed: without the Content View store the snapshot check cannot run.
                let st = ctx.cv()?.store;
                let protected = st.protected_versions(&ctx.scope).await?.iter().filter(|v| v.starts_with(&href)).count();
                if protected > 0 {
                    bail!("refused: {protected} version(s) of {name} are Content View snapshots; delete those Content View versions first");
                }
                let mut sources = vec![];
                for cv in st.content_views(&ctx.scope).await? {
                    if st.cv_repos(cv.id).await?.iter().any(|r| r.repo_href == href) {
                        sources.push(cv.name);
                    }
                }
                let pubs: Vec<Publication> = c.list_all(&c.api(t.publications()), &[("repository", href.clone())], 100_000).await?;
                let versions: Page<Value> = c.list(&format!("{href}versions/"), &[("limit", "1".into())]).await?;
                let users = served_by(&ops::distributions(c, t).await?, |d| {
                    d.repository.as_deref() == Some(href.as_str())
                        || d.repository_version.as_ref().is_some_and(|v| v.starts_with(href.as_str()))
                        || pubs.iter().any(|p| d.publication.as_ref() == Some(&p.pulp_href))
                });
                let mut lines =
                    vec![format!("Deletes repository {name} with all {} versions and {} publications.", versions.count, pubs.len())];
                if users.is_empty() {
                    lines.push("No distribution serves it.".into());
                } else {
                    lines.push(format!("WARNING: {} distribution(s) serve it and stop serving:", users.len()));
                    lines.extend(users);
                }
                if !sources.is_empty() {
                    lines.push(format!("WARNING: Content View(s) {} use it as a source; their next publish fails.", sources.join(", ")));
                }
                ap(move |app| {
                    app.confirm_danger(format!("Delete repository {name}"), lines, &name.clone(), move |app| {
                        submit(app, format!("delete repository {name}"), move |c| async move { Ok(c.delete(&href).await?) })
                    });
                })
            });
        }
        Act::Sync => {
            let policies = sync_policies(caps(app).as_deref(), t);
            let default = item["remote"].as_str().map(str::to_string);
            let note = if default.is_some() { "repository has a default remote" } else { "repository has NO default remote: pick one" };
            let label = format!("remote of {name}");
            let open = move |app: &mut App, current: String| {
                let form = Form::new(
                    format!("Sync {name}"),
                    vec![
                        Field::pick("remote", "Remote", Source::Remotes(t)).help("empty = the repository's default remote").value(current),
                        Field::new("policy", "Sync policy", FieldKind::Choice(policies)),
                    ],
                )
                .note(note);
                app.form(form, move |app, f| {
                    let (remote, policy) = (f.opt("remote").map(str::to_string), f.get("policy").to_string());
                    let (href, name) = (href.clone(), name.clone());
                    let go = move |app: &mut App| {
                        submit(app, format!("sync {name} ({policy})"), move |c| async move {
                            let r = match remote {
                                Some(r) => Some(href_by_name(&c, t.remotes(), &r).await?),
                                None => None,
                            };
                            Ok(ops::sync(&c, t, &href, r.as_deref(), &policy).await?)
                        })
                    };
                    confirm_mirror(app, f.get("policy"), go);
                    Ok(())
                });
            };
            match default {
                // The picker holds names: read the default remote's name so the field shows it.
                Some(r) => app.job(label, move |ctx| async move {
                    let v = ctx.client.get::<Value>(&r, &[("fields", "name".into())]).await;
                    // An unreadable remote leaves the field empty, which still syncs from the default remote.
                    let current = v.map(|v| s(&v, "name")).unwrap_or_default();
                    ap(move |app| open(app, current))
                }),
                None => open(app, String::new()),
            }
        }
        Act::SyncHistory => app.job(format!("sync history {name}"), move |ctx| async move {
            let c = &ctx.client;
            let q = [
                ("reserved_resources", href.clone()),
                ("name__contains", "sync".to_string()),
                ("ordering", "-pulp_created".into()),
                ("limit", "50".into()),
            ];
            let p: Page<Task> = c.list(&c.api("tasks/"), &q).await?;
            let lines: Vec<String> = p
                .results
                .iter()
                .map(|t| {
                    let created = t.created_matching("/versions/").map(|v| format!(" -> {}", short(&v))).unwrap_or_default();
                    let err = t.error_message().map(|e| format!("  error: {e}")).unwrap_or_default();
                    format!(
                        "{}  {:<10} {}{created}{err}",
                        t.pulp_created.as_deref().unwrap_or("").get(..19).unwrap_or(""),
                        t.state,
                        t.pulp_href
                    )
                })
                .collect();
            ap(move |app| app.text(format!("Sync history of {name} ({} tasks)", p.count), lines))
        }),
        Act::Upload => {
            let mut fields = vec![Field::text("path", "Local file path").required()];
            match t {
                RepoType::Rpm => {}
                RepoType::File => fields.push(Field::text("relative_path", "Relative path in repository").help("default: file name")),
                RepoType::Deb => {
                    app.error("upload of Deb packages is not supported");
                    return;
                }
            }
            app.form(
                Form::new(format!("Upload to {name}"), fields)
                    .note("Large files use the chunked upload API; the server digest is verified."),
                move |app, f| {
                    let path = std::path::PathBuf::from(f.get("path"));
                    if !path.is_file() {
                        return Err(format!("{} is not a readable file", path.display()));
                    }
                    let rel = f.opt("relative_path").map(str::to_string);
                    let href = href.clone();
                    app.job(format!("upload {}", path.display()), move |ctx| async move {
                        if ops::chunked(&ctx.client, tokio::fs::metadata(&path).await?.len()) {
                            ctx.caps.require(Feature::ChunkedUpload)?;
                        }
                        let p2 = ctx.clone();
                        let progress = move |d: u64, total: u64| {
                            p2.progress(format!("{:.1}/{:.1} MiB", d as f64 / 1048576.0, total as f64 / 1048576.0))
                        };
                        let r = ops::upload(&ctx.client, t, &path, rel.as_deref(), Some(&href), &progress, &ctx.on_task()).await?;
                        ap(move |app| {
                            app.ok(format!("uploaded {} (sha256 {}…) as {}", path.display(), &r.sha256[..12], short(&r.content_href)));
                            app.reload();
                        })
                    });
                    Ok(())
                },
            );
        }
        Act::Publish => match item["latest_version_href"].as_str() {
            None => app.error("repository has no versions"),
            Some(v) => {
                let v = v.to_string();
                publish_form(app, t, format!("Publish {name}"), move |app, sv| publish(app, t, &name, v.clone(), sv));
            }
        },
        Act::Distribute => {
            distribute_form(app, t, name.clone(), item["latest_version_href"].as_str().map(str::to_string), Some(href), None)
        }
        Act::CopyContent => {
            // File copies through modify/.
            let api = match t {
                RepoType::Rpm => Some(Feature::RpmCopy),
                RepoType::Deb => Some(Feature::DebCopy),
                RepoType::File => None,
            };
            if api.is_some_and(|f| unsupported(app, f)) {
                return;
            }
            let mut fields = vec![
                Field::pick("source", "Source repository", Source::Repos(Some(t))).required(),
                Field::text("filters", "Filter rules").help(filter_help(t)),
            ];
            match t {
                RepoType::Rpm => fields.push(Field::new("deps", "Copy dependencies", FieldKind::Bool).value("true")),
                RepoType::File | RepoType::Deb => {}
            }
            app.form(
                Form::new(format!("Copy content into {name}"), fields)
                    .note("Creates a new version of the target; source and older versions are unchanged."),
                move |app, f| {
                    let rules = parse_rules(f.get("filters"), t)?;
                    let (source, deps) = (f.get("source").to_string(), f.bool("deps"));
                    let (href, name) = (href.clone(), name.clone());
                    app.job(format!("preview copy {source} -> {name}"), move |ctx| async move {
                        let c = &ctx.client;
                        let src: Value = ops::find_by_name(c, &c.api(t.repositories()), &source)
                            .await?
                            .ok_or_else(|| anyhow!("no repository {source}"))?;
                        let sv = src["latest_version_href"].as_str().ok_or_else(|| anyhow!("{source} has no versions"))?.to_string();
                        let sel = Selector::new(&rules);
                        let units: Option<Vec<String>> = match (t, rules.is_empty()) {
                            (RepoType::Rpm, true) => None,
                            (RepoType::Rpm, false) => {
                                Some(rpm::packages_in(c, &sv).await?.into_iter().filter(|p| sel.package(p)).map(|p| p.pulp_href).collect())
                            }
                            (RepoType::File, _) => {
                                Some(file::files_in(c, &sv).await?.into_iter().filter(|x| sel.file(x)).map(|x| x.pulp_href).collect())
                            }
                            (RepoType::Deb, true) => None,
                            (RepoType::Deb, false) => Some(
                                deb::packages_in(c, &sv).await?.into_iter().filter(|p| sel.deb_package(p)).map(|p| p.pulp_href).collect(),
                            ),
                        };
                        let what = units.as_ref().map(|u| format!("{} unit(s)", u.len())).unwrap_or("all content".into());
                        let lines = vec![
                            format!("Copy {what} from {} into {name}.", short(&sv)),
                            if deps { "RPM dependencies are copied too.".into() } else { String::new() },
                        ];
                        ap(move |app| {
                            app.confirm("Confirm copy", lines, move |app| {
                                submit(app, format!("copy {source} -> {name}"), move |c| async move {
                                    let batches: Vec<Option<Vec<String>>> = match units {
                                        None => vec![None],
                                        Some(u) => ops::batches(&u).into_iter().map(|b| Some(b.to_vec())).collect(),
                                    };
                                    // Each batch adds to the latest version: wait for one before the next; the last goes to Tasks.
                                    let mut sent = None;
                                    for units in batches {
                                        if let Some(m) = sent.take() {
                                            ops::settle(&c, m, &ops::no_progress).await?;
                                        }
                                        sent = Some(match t {
                                            RepoType::Rpm => rpm::copy(&c, &sv, &href, None, units, deps).await?,
                                            RepoType::File => ops::add_content(&c, &href, units.unwrap_or_default()).await?,
                                            RepoType::Deb => deb::copy(&c, &sv, &href, None, units).await?,
                                        });
                                    }
                                    sent.ok_or_else(|| anyhow!("nothing to copy"))
                                })
                            })
                        })
                    });
                    Ok(())
                },
            );
        }
        _ => {}
    }
}

/// The optional signing service of a Deb publication; `None` for other types and when the server's Deb
/// publications have no such field (the repository's keys apply).
fn signing_field(app: &App, t: RepoType) -> Option<Field> {
    match t {
        RepoType::Rpm | RepoType::File => None,
        RepoType::Deb => caps(app).is_none_or(|c| c.has_field("deb.AptPublication", "signing_service")).then(|| {
            // shortcut: signing-services/ has no type filter, so non-Deb services are listed and Pulp refuses them; filter when pulpcore exposes the type.
            Field::pick("signing_service", "Signing service", Source::SigningServices)
                .help("empty = repository default (the repository's own keys apply; a publication cannot turn them off)")
        }),
    }
}

/// Run `go` with the signing service of a publish: asked first when `signing_field` offers one, else `None`.
fn publish_form(app: &mut App, t: RepoType, title: String, go: impl Fn(&mut App, Option<String>) + 'static) {
    match signing_field(app, t) {
        None => go(app, None),
        Some(field) => app.form(Form::new(title, vec![field]), move |app, f| {
            go(app, f.opt("signing_service").map(str::to_string));
            Ok(())
        }),
    }
}

fn publish(app: &mut App, t: RepoType, name: &str, version: String, signing_service: Option<String>) {
    app.job(format!("publish {name} {}", short(&version)), move |ctx| async move {
        let p = ops::create_publication(&ctx.client, t, &version, signing_service.as_deref(), &ctx.on_task()).await?;
        ap(move |app| app.ok(format!("publication {} for {}", short(&p), short(&version))))
    });
}

/// Create or re-point a distribution to `publication`, or to the publication of `version` (pinned), or bind
/// it to the repository (follows the latest publication).
fn distribute_form(app: &mut App, t: RepoType, name: String, version: Option<String>, repo: Option<String>, publication: Option<String>) {
    let modes = if repo.is_some() {
        vec!["pin this version".to_string(), "follow repository latest".to_string()]
    } else {
        vec!["pin this version".to_string()]
    };
    let mut fields = vec![
        Field::text("dist", "Distribution name").required().value(name.clone()),
        Field::text("base_path", "Base path").required().value(name.clone()),
        Field::new("mode", "Binding", FieldKind::Choice(modes)),
    ];
    if publication.is_none() {
        fields.extend(signing_field(app, t));
    }
    let form = Form::new(format!("Distribute {name}"), fields)
        .note("Pinned: serves exactly one immutable publication. Following latest: serves the newest publication (RPM: use autopublish).")
        .note("An existing distribution with this name is re-pointed; base paths of other distributions are never taken over.");
    app.form(form, move |app, f| {
        let (dist, bp, follow) =
            (f.get("dist").to_string(), f.get("base_path").trim_matches('/').to_string(), f.get("mode").starts_with("follow"));
        let (version, repo, publication) = (version.clone(), repo.clone(), publication.clone());
        let signing = f.opt("signing_service").map(str::to_string);
        app.job(format!("distribute {dist}"), move |ctx| async move {
            let c = &ctx.client;
            let on = ctx.on_task();
            let existing = ops::find_by_name::<Value>(c, &c.api(t.distributions()), &dist).await?;
            if let Some(d) = &existing {
                cv_distribution_refusal(d)?;
            }
            let href = if follow {
                let repo = repo.ok_or_else(|| anyhow!("no repository"))?;
                match existing {
                    Some(d) => {
                        if s(&d, "base_path") != bp {
                            bail!("distribution {dist} serves {} instead of {bp}", s(&d, "base_path"));
                        }
                        let h = s(&d, "pulp_href");
                        let mut b = json!({ "repository": repo, "publication": null });
                        // Pulp keeps a field that a PATCH leaves out, and refuses two bindings (HTTP 400).
                        if d["repository_version"].is_string() {
                            b["repository_version"] = Value::Null;
                        }
                        ops::settle(c, c.patch(&h, &b).await?, &on).await?;
                        h
                    }
                    None => {
                        ops::create(
                            c,
                            &c.api(t.distributions()),
                            &json!({ "name": dist, "base_path": bp, "repository": repo }),
                            "/distributions/",
                            &on,
                        )
                        .await?
                    }
                }
            } else {
                let p = match publication {
                    Some(p) => p,
                    None => {
                        let v = version.ok_or_else(|| anyhow!("repository has no versions"))?;
                        ops::ensure_publication(c, t, &v, signing.as_deref(), &on).await?
                    }
                };
                ops::ensure_distribution(c, t, &dist, &bp, Some(&p), &json!({}), None, &on).await?
            };
            let d: Value = c.get(&href, &[]).await?;
            let url = s(&d, "base_url");
            ap(move |app| {
                app.ok(format!("distribution {dist} serves {url}"));
                app.reload();
            })
        });
        Ok(())
    });
}

// ---- remotes -------------------------------------------------------------------------

/// Help of a remote field whose value the edit form cannot show (write-only, or a file path).
const WRITE_ONLY: &str = "empty = unchanged, - = clear";

fn remote_url_help(t: RepoType) -> &'static str {
    match t {
        RepoType::Rpm => "yum repository base URL",
        RepoType::File => "URL of the PULP_MANIFEST",
        RepoType::Deb => "Debian archive base URL (the directory that holds dists/)",
    }
}

/// Remote form fields that only some plugins have; `v` is the remote being edited (`Null` on create).
fn remote_extra_fields(t: RepoType, caps: Option<&Capabilities>, v: &Value) -> Vec<Field> {
    let has = |f: &str| caps.is_none_or(|c| c.has_field(t.remote_schema(), f));
    let write_only = if v.is_null() { "" } else { WRITE_ONLY };
    match t {
        RepoType::Rpm => {
            if has("sles_auth_token") {
                vec![Field::new("sles_auth_token", "SLES auth token", FieldKind::Secret).help(write_only)]
            } else {
                vec![]
            }
        }
        RepoType::File => vec![],
        RepoType::Deb => {
            let text = |k: &str, label: &str| Field::text(k, label).value(s(v, k));
            let flag = |k: &str, label: &str| Field::new(k, label, FieldKind::Bool).value(if v[k] == true { "true" } else { "false" });
            let all = [
                text("distributions", "Distributions").required().help("space-separated, e.g. bookworm bookworm-updates"),
                text("components", "Components").help("space-separated; empty = all"),
                text("architectures", "Architectures").help("space-separated; empty = all"),
                flag("sync_sources", "Sync source packages"),
                flag("sync_udebs", "Sync udebs"),
                flag("sync_installer", "Sync installer files"),
                flag("ignore_missing_package_indices", "Ignore missing package indices"),
                Field::text("gpgkey", "GPG public key (file path)").help(write_only),
            ];
            all.into_iter().filter(|f| has(&f.key)).collect()
        }
    }
}

/// JSON body from a remote form, and the (field, file path) pairs whose file content goes into it.
fn remote_body(f: &Form, edit: bool) -> Result<(Value, Vec<(String, String)>), String> {
    let mut b = json!({});
    let mut files = vec![];
    for fld in &f.fields {
        let val = f.get(&fld.key).to_string();
        let secretish = matches!(fld.kind, FieldKind::Secret) || matches!(fld.key.as_str(), "username" | "proxy_username");
        // Write-only file paths: the body gets the file's content.
        let file = matches!(fld.key.as_str(), "ca_cert" | "client_cert" | "client_key" | "gpgkey");
        if (!edit || secretish || file) && val.is_empty() {
            continue; // write-only: empty keeps the server value
        }
        if edit && !fld.changed() && !secretish && !file {
            continue;
        }
        match (fld.key.as_str(), &fld.kind) {
            ("labels", _) => b["pulp_labels"] = parse_labels(&val)?,
            // The server accepts null for all of them (allow_null).
            _ if (secretish || file) && val == "-" => b[fld.key.as_str()] = Value::Null,
            _ if file => files.push((fld.key.clone(), val)),
            (_, FieldKind::Bool) => b[fld.key.as_str()] = (val == "true").into(),
            (_, FieldKind::Number) if val.is_empty() => b[fld.key.as_str()] = Value::Null,
            (_, FieldKind::Number) => {
                b[fld.key.as_str()] = val.parse::<i64>().map_err(|_| format!("{} must be a number", fld.label))?.into()
            }
            _ if val.is_empty() => b[fld.key.as_str()] = Value::Null,
            _ => b[fld.key.as_str()] = val.into(),
        }
    }
    url::Url::parse(f.get("url")).map_err(|e| format!("invalid URL: {e}"))?;
    Ok((b, files))
}

fn remote_action(app: &mut App, t: RepoType, act: Act, item: Value) {
    let href = s(&item, "pulp_href");
    let name = s(&item, "name");
    match act {
        Act::Create | Act::Edit => {
            let edit = act == Act::Edit;
            let v = if edit { item.clone() } else { Value::Null };
            let caps = caps(app);
            let write_only = if edit { WRITE_ONLY } else { "" };
            let policies = caps
                .as_ref()
                .and_then(|c| c.enum_values("Policy692Enum").map(|e| e.to_vec()))
                .unwrap_or_else(|| vec!["immediate".into(), "on_demand".into(), "streamed".into()]);
            let mut fields = vec![
                Field::text("name", "Name").required().value(s(&v, "name")),
                Field::text("url", "URL").required().value(s(&v, "url")).help(remote_url_help(t)),
                Field::new("policy", "Download policy", FieldKind::Choice(policies)).value(if edit {
                    s(&v, "policy")
                } else {
                    "immediate".into()
                }),
                Field::new("tls_validation", "Validate TLS", FieldKind::Bool).value(if v["tls_validation"] == false {
                    "false"
                } else {
                    "true"
                }),
                Field::text("ca_cert", "CA cert (PEM file path)").help(write_only),
                Field::text("client_cert", "Client cert (PEM file path)").help(write_only),
                Field::text("client_key", "Client key (PEM file path)").help(write_only),
                Field::text("username", "Username").help(write_only),
                Field::new("password", "Password", FieldKind::Secret).help(write_only),
                Field::text("proxy_url", "Proxy URL").value(s(&v, "proxy_url")),
                Field::text("proxy_username", "Proxy username").help(write_only),
                Field::new("proxy_password", "Proxy password", FieldKind::Secret).help(write_only),
                Field::new("download_concurrency", "Download concurrency", FieldKind::Number).value(s(&v, "download_concurrency")),
                Field::new("rate_limit", "Rate limit (req/s)", FieldKind::Number).value(s(&v, "rate_limit")),
                // A float on the server: `s()` would show `3600.0`, which is not a whole number.
                Field::new("total_timeout", "Total timeout (s)", FieldKind::Number)
                    .value(v["total_timeout"].as_f64().map(|x| x.to_string()).unwrap_or_default())
                    .help("seconds one download may take (large packages need more); empty = server default"),
                Field::text("labels", "Labels (k=v,...)").value(format_labels(&v["pulp_labels"])),
            ];
            fields.extend(remote_extra_fields(t, caps.as_deref(), &v));
            let form = Form::new(if edit { format!("Edit remote {name}") } else { format!("New {} remote", t.label()) }, fields);
            app.form(form, move |app, f| {
                let (mut b, files) = remote_body(f, edit)?;
                let href = href.clone();
                submit(app, format!("{} remote {}", if edit { "update" } else { "create" }, f.get("name")), move |c| async move {
                    for (k, path) in files {
                        b[k.as_str()] = tokio::fs::read_to_string(&path).await.with_context(|| format!("read {path}"))?.into();
                    }
                    Ok(if edit { c.patch(&href, &b).await? } else { c.post(&c.api(t.remotes()), &b).await? })
                });
                Ok(())
            });
        }
        Act::Delete => app.job(format!("impact of deleting remote {name}"), move |ctx| async move {
            let c = &ctx.client;
            let users: Page<Value> = c.list(&c.api(t.repositories()), &[("remote", href.clone()), ("limit", "100".into())]).await?;
            let lines = vec![
                format!("Delete remote {name}. Repository content and versions are not affected."),
                format!(
                    "{} repository(ies) use it as default remote: {}",
                    users.count,
                    users.results.iter().map(|r| s(r, "name")).collect::<Vec<_>>().join(", ")
                ),
            ];
            ap(move |app| {
                app.confirm_danger(format!("Delete remote {name}"), lines, &name.clone(), move |app| {
                    submit(app, format!("delete remote {name}"), move |c| async move { Ok(c.delete(&href).await?) })
                })
            })
        }),
        _ => {}
    }
}

// ---- versions and content ------------------------------------------------------------------

fn version_action(app: &mut App, act: Act, item: Value) {
    let href = s(&item, "pulp_href");
    let Some(t) = RepoType::from_href(&href) else {
        app.error("only RPM, File and Deb repository versions are managed here");
        return;
    };
    let number = s(&item, "number");
    let repo = repo_of_version(&href);
    match act {
        Act::Content => app.push(Kind::Content { version: href, ctype: 0 }),
        Act::Publish => {
            let name = app.last_repo.as_ref().filter(|r| r.1 == repo).map(|r| r.2.clone()).unwrap_or_else(|| short(&repo));
            publish_form(app, t, format!("Publish version {number}"), move |app, sv| publish(app, t, &name, href.clone(), sv));
        }
        Act::Distribute => {
            let name = app.last_repo.as_ref().filter(|r| r.1 == repo).map(|r| r.2.clone()).unwrap_or_else(|| short(&repo));
            distribute_form(app, t, name, Some(href), None, None);
        }
        Act::Rollback => {
            let form = Form::new(
                format!("Point a distribution to version {number}"),
                vec![Field::pick("dist", "Distribution", Source::Distributions(t)).required()],
            )
            .note("The distribution will serve this version's publication (created if missing). No version is deleted.");
            app.form(form, move |app, f| {
                rollback_preview(app, t, f.get("dist").to_string(), href.clone());
                Ok(())
            });
        }
        Act::Compare => {
            let marked: Vec<String> = app.list.marked.iter().filter(|m| **m != href).cloned().collect();
            let [other] = marked.as_slice() else {
                app.error("mark exactly one other version with Space, then compare");
                return;
            };
            if RepoType::from_href(other) != Some(t) {
                app.error("compare needs two versions of one repository type");
                return;
            }
            let other = other.clone();
            app.job("compare versions", move |ctx| async move {
                // Always show older -> newer.
                let (a, b) = if version_number(&other) <= version_number(&href) { (other, href) } else { (href, other) };
                let ka = ops::all_content_keys(&ctx.client, t, &a).await?;
                let kb = ops::all_content_keys(&ctx.client, t, &b).await?;
                let lines = ops::diff(&ka, &kb).lines(&short(&a), &short(&b));
                ap(move |app| app.text("Version comparison", lines))
            });
        }
        Act::Delete => app.job(format!("impact of deleting version {number}"), move |ctx| async move {
            let c = &ctx.client;
            // Fails closed: without the Content View store the snapshot check cannot run.
            if ctx.cv()?.store.protected_versions(&ctx.scope).await?.contains(&href) {
                bail!("refused: this version is a snapshot of a Content View version (delete the CV version instead)");
            }
            let (pubs, by) = ops::version_protection(c, t, &href).await?;
            let id = short(&href);
            if !by.is_empty() {
                // Pulp refuses the delete (HTTP 400): no typed confirm.
                let mut lines = vec![format!("Pulp refuses to delete version {number} ({id}): these distributions use it.")];
                lines.extend(by.iter().map(|(d, why)| format!("  {} ({}): {why}", d.name, d.base_url.as_deref().unwrap_or(&d.base_path))));
                lines.push("First re-point them to another version (Rollback or Distribute), or edit or delete them.".into());
                return ap(move |app| app.text(format!("Cannot delete version {number}"), lines));
            }
            let lines = vec![
                format!("Delete repository version {number} ({id}) and its {} publication(s).", pubs.len()),
                "No distribution uses it. Deleting versions is never needed for rollback: re-point a distribution instead.".into(),
            ];
            ap(move |app| {
                app.confirm_danger(format!("Delete version {number}"), lines, &id, move |app| {
                    submit(app, format!("delete version {number}"), move |c| async move { Ok(c.delete(&href).await?) })
                })
            })
        }),
        _ => {}
    }
}

/// Orphan cleanup of the whole server. The count and the request use the protection time the operator gives:
/// the server's own `ORPHAN_PROTECTION_TIME` cannot be read, so a count without it would not match what goes.
fn prune_orphans(app: &mut App) {
    if caps(app).is_some_and(|c| !c.has_endpoint("orphans/cleanup/")) {
        return app.error("orphan cleanup is not supported by this server");
    }
    let minutes = Field::new("minutes", "Orphaned for at least (minutes)", FieldKind::Number)
        .value("1440")
        .help("1440 = Pulp's default; 0 = all, also the content of an upload that is still running");
    app.form(Form::new("Prune orphan content", vec![minutes]), |app, f| {
        let minutes: u32 = f.get("minutes").trim().parse().map_err(|_| "minutes must be a whole number from 0".to_string())?;
        let profile = app.profile.clone().unwrap_or_default();
        app.job("count orphan content", move |ctx| async move {
            let c = &ctx.client;
            let orphans: Page<Value> = c.list(&c.api("content/"), &[("orphaned_for", minutes.to_string()), ("limit", "1".into())]).await?;
            let lines = vec![
                format!("Delete the orphans on {profile}: content that no repository version has, and files that no content uses."),
                format!("{} content unit(s) orphaned for {minutes} minutes or more.", orphans.count),
                "Pulp cannot undo this. A sync downloads deleted content again.".into(),
            ];
            ap(move |app| {
                let typed = profile.clone();
                app.confirm_danger("Prune orphan content", lines, &typed, move |app| {
                    let label = format!("prune orphans on {profile}");
                    app.job(label.clone(), move |ctx| async move {
                        let c = &ctx.client;
                        let m = c.post(&c.api("orphans/cleanup/"), &json!({ "orphan_protection_time": minutes })).await?;
                        ap(move |app| follow(app, m, label.clone(), move |app, t| orphans_pruned(app, &label, &t)))
                    })
                })
            })
        });
        Ok(())
    });
}

/// The cleanup task ended: what its progress reports say it deleted.
fn orphans_pruned(app: &mut App, label: &str, t: &Task) {
    let done = |code: &str| t.progress_reports.iter().find(|r| r.code == code).and_then(|r| r.done).unwrap_or(0);
    app.ok(format!("{label}: deleted {} content unit(s) and {} artifact(s)", done("clean-up.content"), done("clean-up.artifacts")));
}

/// Show what a distribution serves now and what it will serve, then re-point on confirmation.
fn rollback_preview(app: &mut App, t: RepoType, dist: String, version: String) {
    app.job(format!("rollback preview {dist}"), move |ctx| async move {
        let c = &ctx.client;
        let d: Value =
            ops::find_by_name(c, &c.api(t.distributions()), &dist).await?.ok_or_else(|| anyhow!("no {} distribution {dist}", t.label()))?;
        cv_distribution_refusal(&d)?;
        // The new publication keeps the signing service of the served one (a Deb publication can have its own).
        let (current, signing) = match served_now(c, t, &d).await? {
            Some((v, p, how)) => (format!("{} ({how})", short(&v)), p["signing_service"].as_str().map(str::to_string)),
            None => ("nothing".into(), None),
        };
        let lines = vec![
            format!("Distribution {dist} ({})", s(&d, "base_url")),
            format!("  now serves:  {current}"),
            format!("  will serve:  {} (pinned)", short(&version)),
            String::new(),
            "Newer versions and publications are kept.".into(),
            "Repository metadata switches immediately. Pulp may keep serving files of the previous".into(),
            "publication for DISTRIBUTED_PUBLICATION_RETENTION_PERIOD (default 3 days).".into(),
        ];
        let typed = dist.clone();
        ap(move |app| {
            app.confirm_prod(format!("Roll back {dist}"), lines, &typed, move |app| {
                app.job(format!("rollback {dist}"), move |ctx| async move {
                    let c = &ctx.client;
                    let on = ctx.on_task();
                    let p = ops::ensure_publication(c, t, &version, signing.as_deref(), &on).await?;
                    let base = s(&d, "base_path");
                    ops::ensure_distribution(c, t, &dist, &base, Some(&p), &json!({}), None, &on).await?;
                    let pv: Value = c.get(&p, &[]).await?;
                    let check = ops::verify_served(c, t, &s(&d, "base_url"), &version, &s(&pv, "layout")).await;
                    ap(move |app| {
                        match check {
                            Ok(m) => app.ok(format!("{dist} now serves {}: {m}", short(&version))),
                            Err(e) => app.notify(Level::Warn, format!("{dist} re-pointed, but verification failed: {e}")),
                        }
                        app.reload();
                    })
                });
            })
        })
    });
}

fn content_action(app: &mut App, version: String, act: Act) {
    let Some(t) = RepoType::from_href(&version) else { return };
    let mut units: Vec<String> = app.list.marked.iter().cloned().collect();
    if units.is_empty()
        && let Some(v) = app.list.current()
    {
        units.push(item_key(v));
    }
    let repo = repo_of_version(&version);
    match act {
        Act::RemoveContent => {
            let lines = vec![
                format!("Create a new version of {} without {} unit(s).", short(&repo), units.len()),
                "The current and older versions are immutable and keep the content.".into(),
            ];
            app.confirm("Remove content", lines, move |app| {
                let (n, label) = (units.len(), format!("remove {} unit(s)", units.len()));
                app.job(label.clone(), move |ctx| async move {
                    let c = &ctx.client;
                    let r: Value = c.get(&repo, &[]).await?;
                    if let Some(e) = snapshot_refusal(&r) {
                        bail!(e);
                    }
                    let units = match t {
                        RepoType::Deb => {
                            let latest = r["latest_version_href"].as_str().ok_or_else(|| anyhow!("repository has no versions"))?;
                            deb::with_release_records(c, latest, units).await?
                        }
                        RepoType::Rpm | RepoType::File => units,
                    };
                    let m = ops::remove_content(c, &repo, units).await?;
                    // Only a success shows the new version (without marks): on a failure the operator keeps them.
                    ap(move |app| follow(app, m, label, move |app, t| content_removed(app, &version, n, &t)))
                });
            });
        }
        Act::AddToRepo => {
            if t == RepoType::Deb && unsupported(app, Feature::DebCopy) {
                return;
            }
            let form = Form::new(
                format!("Add {} unit(s) to another {} repository", units.len(), t.label()),
                vec![Field::pick("target", "Target repository", Source::Repos(Some(t))).required()],
            );
            app.form(form, move |app, f| {
                let (target, units, kind, version) = (f.get("target").to_string(), units.clone(), app.list.kind.clone(), version.clone());
                let (n, label) = (units.len(), format!("add {} unit(s) to {target}", units.len()));
                app.job(label.clone(), move |ctx| async move {
                    let c = &ctx.client;
                    let r: Value =
                        ops::find_by_name(c, &c.api(t.repositories()), &target).await?.ok_or_else(|| anyhow!("no repository {target}"))?;
                    if let Some(e) = snapshot_refusal(&r) {
                        bail!(e);
                    }
                    let m = match t {
                        RepoType::Rpm | RepoType::File => ops::add_content(c, &s(&r, "pulp_href"), units).await?,
                        // modify/ adds no release component records, so apt would not see the packages.
                        RepoType::Deb => deb::copy(c, &version, &s(&r, "pulp_href"), None, Some(units)).await?,
                    };
                    ap(move |app| {
                        follow(app, m, label, move |app, t| {
                            // Only after success: on a failure the operator keeps the marks.
                            if app.list.kind == kind {
                                app.list.marked.clear();
                            }
                            let v = t
                                .created_matching("/versions/")
                                .map_or("no new version (it has them already)".into(), |v| format!("version {}", version_number(&v)));
                            app.ok(format!("added {n} unit(s) to {target}: {v}"));
                        })
                    })
                });
                Ok(())
            });
        }
        _ => {}
    }
}

// ---- publications and distributions --------------------------------------------------------

fn publication_action(app: &mut App, t: RepoType, act: Act, item: Value) {
    let href = s(&item, "pulp_href");
    match act {
        Act::Create => {
            let mut fields = vec![
                Field::pick("repo", "Repository", Source::Repos(Some(t))).required(),
                Field::new("number", "Version number", FieldKind::Number).help("empty = latest"),
            ];
            fields.extend(signing_field(app, t));
            let form = Form::new(format!("New {} publication", t.label()), fields);
            app.form(form, move |app, f| {
                let (repo, number) = (f.get("repo").to_string(), f.number("number"));
                let signing = f.opt("signing_service").map(str::to_string);
                app.job(format!("publish {repo}"), move |ctx| async move {
                    let c = &ctx.client;
                    let r: Value =
                        ops::find_by_name(c, &c.api(t.repositories()), &repo).await?.ok_or_else(|| anyhow!("no repository {repo}"))?;
                    let v = match number {
                        Some(n) => version_of(&s(&r, "pulp_href"), n),
                        None => r["latest_version_href"].as_str().ok_or_else(|| anyhow!("no versions"))?.to_string(),
                    };
                    let p = ops::create_publication(c, t, &v, signing.as_deref(), &ctx.on_task()).await?;
                    ap(move |app| {
                        app.ok(format!("publication {} of {}", short(&p), short(&v)));
                        app.reload();
                    })
                });
                Ok(())
            });
        }
        Act::Distribute => {
            let v = s(&item, "repository_version");
            distribute_form(app, t, short(&repo_of_version(&v)), Some(v), None, Some(href));
        }
        Act::Delete => {
            let (id, version) = (short(&href), s(&item, "repository_version"));
            app.job(format!("impact of deleting publication {id}"), move |ctx| async move {
                // Fails closed: without the Content View store the snapshot check cannot run.
                let st = ctx.cv()?.store;
                if let Some((_, cv, env)) = st.served_publications(&ctx.scope).await?.into_iter().find(|p| p.0 == href) {
                    bail!("refused: Content View {cv} serves publication {id} in {env}; promote or roll back the view instead");
                }
                if st.protected_versions(&ctx.scope).await?.contains(&version) {
                    bail!("refused: publication {id} is of a Content View snapshot (delete the Content View version instead)");
                }
                // Pulp unbinds the distributions that serve it without an error (`on_delete=SET_NULL`).
                let users = served_by(&ops::distributions(&ctx.client, t).await?, |d| d.publication.as_ref() == Some(&href));
                let mut lines = vec![format!("Delete publication {id} of {}. The repository version is not affected.", short(&version))];
                if !users.is_empty() {
                    lines.push(format!("WARNING: {} distribution(s) serve this publication and stop serving:", users.len()));
                    lines.extend(users);
                }
                ap(move |app| {
                    app.confirm_danger("Delete publication", lines, &id, move |app| {
                        submit(app, "delete publication", move |c| async move { Ok(c.delete(&href).await?) })
                    })
                })
            });
        }
        _ => {}
    }
}

fn distribution_action(app: &mut App, t: RepoType, act: Act, item: Value) {
    let href = s(&item, "pulp_href");
    let name = s(&item, "name");
    match act {
        Act::CopyUrl => {
            let url = s(&item, "base_url");
            copy_to_clipboard(&url);
            app.ok(format!("copied {url}"));
        }
        Act::Verify => app.job(format!("verify {name}"), move |ctx| async move {
            let c = &ctx.client;
            let d: Value = c.get(&href, &[]).await?;
            let Some((version, publication, how)) = served_now(c, t, &d).await? else {
                bail!("distribution serves nothing (no binding, or its repository has no publication)");
            };
            let res = ops::verify_served(c, t, &s(&d, "base_url"), &version, &s(&publication, "layout")).await;
            let lines = vec![
                format!("{name}: {how}"),
                format!("expected content: {}", short(&version)),
                format!("URL: {}", s(&d, "base_url")),
                String::new(),
                match res {
                    Ok(m) => format!("OK: {m}"),
                    Err(e) => format!("MISMATCH: {e}"),
                },
            ];
            ap(move |app| app.text(format!("Verify {name}"), lines))
        }),
        Act::Rollback => {
            let form = Form::new(
                format!("Re-point {name}"),
                vec![
                    Field::pick("repo", "Repository", Source::Repos(Some(t))).required(),
                    Field::new("number", "Version number", FieldKind::Number).required(),
                ],
            );
            app.form(form, move |app, f| {
                let (repo, n, dist) = (f.get("repo").to_string(), f.number("number").unwrap_or(0), name.clone());
                app.job("resolve version", move |ctx| async move {
                    let h = href_by_name(&ctx.client, t.repositories(), &repo).await?;
                    ap(move |app| rollback_preview(app, t, dist, version_of(&h, n)))
                });
                Ok(())
            });
        }
        Act::Create | Act::Edit => {
            let edit = act == Act::Edit;
            let v = if edit { item.clone() } else { Value::Null };
            let modes = ["publication of a repository version", "follow repository latest", "none"].map(String::from).to_vec();
            let (owner, bound_to_version) = (cv_owner(&v), v["repository_version"].is_string());
            // Promote finds a view's distribution by its name and ownership labels.
            let ownership =
                |labels: &Value| [crate::cv::LABEL_CV, crate::cv::LABEL_ENV, crate::cv::LABEL_DB].map(|k| labels.get(k).cloned());
            let owned_labels = ownership(&v["pulp_labels"]);
            let mut fields = vec![
                Field::text("name", "Name").required().value(s(&v, "name")),
                Field::text("base_path", "Base path").required().value(s(&v, "base_path")),
                Field::new("mode", "Serve", FieldKind::Choice(modes)).value(if v["repository"].is_string() {
                    "follow repository latest"
                } else {
                    "publication of a repository version"
                }),
                Field::pick("repo", "Repository", Source::Repos(Some(t)))
                    .help("required unless Serve = none; empty on edit = keep binding"),
                Field::new("number", "Version number", FieldKind::Number).help("empty = latest version"),
            ];
            fields.extend(signing_field(app, t));
            fields.extend([
                Field::pick("content_guard", "Content guard", Source::ContentGuards).value(s(&v, "content_guard")),
                Field::text("labels", "Labels (k=v,...)").value(format_labels(&v["pulp_labels"])),
            ]);
            let mut form =
                Form::new(if edit { format!("Edit distribution {name}") } else { format!("New {} distribution", t.label()) }, fields)
                    .note("Changing the base path or binding changes what consumers receive at this URL.");
            if let Some(cv) = &owner {
                form = form.note(format!(
                    "Content View {cv} owns this distribution: change what it serves with promote or roll back. Later promotions keep a changed base path."
                ));
            }
            app.form(form, move |app, f| {
                let mut b = json!({});
                for k in ["name", "base_path", "content_guard"] {
                    if !edit || f.changed(k) {
                        // An href keeps its slashes; names and base paths do not.
                        b[k] = f
                            .opt(k)
                            .map(|x| Value::from(if k == "content_guard" { x } else { x.trim_matches('/') }))
                            .unwrap_or(Value::Null);
                    }
                }
                if !edit || f.changed("labels") {
                    b["pulp_labels"] = parse_labels(f.get("labels"))?;
                }
                let (mode, repo, n) = (f.get("mode").to_string(), f.opt("repo").map(str::to_string), f.number("number"));
                let signing = f.opt("signing_service").map(str::to_string);
                if repo.is_none() && mode != "none" && !edit {
                    return Err("repository is required".into());
                }
                let rebind = mode == "none" || repo.is_some();
                if let (Some(cv), true) = (&owner, rebind) {
                    return Err(format!("refused: Content View {cv} owns this distribution; promote or roll back the view instead"));
                }
                if let Some(cv) = &owner
                    && (f.changed("name") || (f.changed("labels") && ownership(&b["pulp_labels"]) != owned_labels))
                {
                    return Err(format!("refused: Content View {cv} finds this distribution by its name and pulp_tui labels; keep them"));
                }
                let href = href.clone();
                let label = format!("{} distribution {}", if edit { "update" } else { "create" }, f.get("name"));
                app.job(label.clone(), move |ctx| async move {
                    let c = &ctx.client;
                    let on = ctx.on_task();
                    // Pulp keeps a field that a PATCH leaves out, and refuses two bindings (HTTP 400).
                    if rebind && bound_to_version {
                        b["repository_version"] = Value::Null;
                    }
                    if mode == "none" {
                        b["publication"] = Value::Null;
                        b["repository"] = Value::Null;
                    } else if let Some(r) = repo {
                        let rv: Value =
                            ops::find_by_name(c, &c.api(t.repositories()), &r).await?.ok_or_else(|| anyhow!("no repository {r}"))?;
                        let rh = s(&rv, "pulp_href");
                        if mode.starts_with("follow") {
                            b["repository"] = rh.into();
                            b["publication"] = Value::Null;
                        } else {
                            let v = match n {
                                Some(n) => version_of(&rh, n),
                                None => rv["latest_version_href"].as_str().ok_or_else(|| anyhow!("no versions"))?.to_string(),
                            };
                            b["publication"] = ops::ensure_publication(c, t, &v, signing.as_deref(), &on).await?.into();
                            b["repository"] = Value::Null;
                        }
                    }
                    let m = if edit { c.patch(&href, &b).await? } else { c.post(&c.api(t.distributions()), &b).await? };
                    ops::settle(c, m, &on).await?;
                    ap(move |app| {
                        app.ok(format!("{label}: done"));
                        app.reload();
                    })
                });
                Ok(())
            });
        }
        Act::Delete => {
            let mut lines = vec![format!("Delete distribution {name}."), format!("{} will stop serving content.", s(&item, "base_url"))];
            if let Some(cv) = cv_owner(&item) {
                let env = item["pulp_labels"].get(crate::cv::LABEL_ENV).and_then(Value::as_str).unwrap_or("?");
                lines.push(format!("WARNING: Content View {cv} serves environment {env} here; the view still records this binding."));
            }
            app.confirm_danger(format!("Delete distribution {name}"), lines, &name.clone(), move |app| {
                submit(app, format!("delete distribution {name}"), move |c| async move { Ok(c.delete(&href).await?) })
            });
        }
        _ => {}
    }
}

// ---- tasks ----------------------------------------------------------------------------------

fn task_action(app: &mut App, act: Act, item: Value) {
    let href = s(&item, "pulp_href");
    match act {
        Act::DismissTasks => {
            let n = app.tasks.dismiss_finished();
            app.ok(format!("dismissed {n} finished task(s) from the list (Pulp keeps its records)"));
            app.reload();
        }
        Act::TrackTask => {
            app.track_task(&href, &s(&item, "name"));
            app.ok("tracking task");
        }
        Act::WatchTask => {
            // Task Center rows have a label, server tasks a name.
            let label = Some(s(&item, "label")).filter(|l| !l.is_empty()).unwrap_or_else(|| s(&item, "name"));
            app.watch_task(&href, &label);
        }
        Act::CancelTask => {
            let lines = vec![
                format!("Ask Pulp to cancel task {href}."),
                "This is a server-side cancellation (not just closing the client).".into(),
                "Pulp decides the final state; work already done may remain (e.g. downloaded artifacts).".into(),
            ];
            app.confirm("Cancel server task", lines, move |app| {
                app.job("cancel task", move |ctx| async move {
                    let res = ctx.client.patch(&href, &json!({ "state": "canceled" })).await;
                    let t: Task = ctx.client.get(&href, &[]).await?;
                    ap(move |app| {
                        match res {
                            Ok(_) => app.notify(Level::Info, format!("cancel requested; Pulp reports state {}", t.state)),
                            Err(e) => app.error(format!("Pulp refused cancellation ({e}); task state is {}", t.state)),
                        }
                        app.handle_msg(super::app::Msg::Task(t));
                        app.reload();
                    })
                });
            });
        }
        _ => {}
    }
}

// ---- dashboard --------------------------------------------------------------------------------

pub fn load_dashboard(app: &mut App) {
    if app.conn.is_none() {
        let mut lines = vec![if app.connecting { "Connecting...".to_string() } else { "Not connected.".into() }, String::new()];
        if app.cfg.profiles.is_empty() {
            lines.push("No server profiles yet: open Servers (Tab, then select) and press c to create one,".into());
            lines.push(format!("or edit {}.", app.cfg_path.display()));
        } else {
            lines.push("Open Servers and press o on a profile to connect, or use the palette (:).".into());
        }
        app.dashboard = lines;
        return;
    }
    // A refresh replaces a load still in flight.
    for id in app.jobs.iter().filter(|j| j.label == "load dashboard").map(|j| j.id).collect::<Vec<_>>() {
        app.abort_job(id);
    }
    // The text shows at once, from the capabilities of the connect; each value comes in when its request returns.
    let mut wanted = vec![];
    dashboard_lines(app, &mut |s| {
        wanted.push(s);
        String::new()
    });
    show_dashboard(app);
    app.load_job("load dashboard", |ctx: Ctx| async move {
        let st: crate::pulp::models::Status = ctx.client.get(&ctx.client.api("status/"), &[]).await?;
        ap(move |app| {
            if let Some(conn) = app.conn.as_mut() {
                std::sync::Arc::make_mut(&mut conn.caps).set_status(st);
            }
            show_dashboard(app);
        })
    });
    for s in wanted {
        app.load_job("load dashboard", move |ctx: Ctx| async move {
            let v = load_stat(&ctx, s).await;
            ap(move |app| {
                app.dashboard_stats.insert(s, v);
                show_dashboard(app);
            })
        });
    }
}

/// A Dashboard value with a request of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stat {
    /// Number of objects in a list: path under `api/v3/`, task state.
    Count(&'static str, Option<&'static str>),
    ContentViews,
}

async fn load_stat(ctx: &Ctx, s: Stat) -> String {
    match s {
        Stat::Count(path, state) => {
            let c = &ctx.client;
            let mut q = vec![("limit", "1".to_string()), ("fields", "pulp_href".into())];
            q.extend(state.map(|s| ("state", s.to_string())));
            c.list::<Value>(&c.api(path), &q)
                .await
                .map(|p| p.count.to_string())
                .unwrap_or_else(|e| format!("n/a ({})", e.status().map(|s| s.to_string()).unwrap_or_default()))
        }
        // A shared database that is down must not hide the Pulp part of the dashboard.
        Stat::ContentViews => {
            let counts = async {
                let st = ctx.cv()?.store;
                let place = if st.external { "shared database" } else { "local to this pulp-tui database" };
                anyhow::Ok((st.content_views(&ctx.scope).await?.len(), st.environments(&ctx.scope).await?.len(), place))
            };
            match counts.await {
                Ok((cvs, envs, place)) => format!("Content Views {cvs}   lifecycle environments {envs}   ({place})"),
                Err(e) => format!("Content Views: {e:#}"),
            }
        }
    }
}

fn show_dashboard(app: &mut App) {
    app.dashboard = dashboard_lines(app, &mut |s| app.dashboard_stats.get(&s).cloned().unwrap_or_else(|| "...".into()));
}

/// The Dashboard of the connection; `stat` gives each value that has a request of its own.
fn dashboard_lines(app: &App, stat: &mut dyn FnMut(Stat) -> String) -> Vec<String> {
    let Some(conn) = &app.conn else { return vec![] };
    let (c, caps) = (&conn.client, &conn.caps);
    let mut lines = vec![];
    if let Some(st) = &caps.status {
        lines.push(format!("Server        {}   content: {}", c.profile(), caps.content_base().unwrap_or_default()));
        lines.push(format!(
            "User          {}",
            caps.user
                .as_ref()
                .map(|u| format!("{}{}", u.username, if u.is_staff == Some(true) { " (staff/admin)" } else { "" }))
                .unwrap_or("anonymous".into())
        ));
        lines.push(format!(
            "Health        database: {}   redis: {}   workers: {}   content apps: {}",
            if st.database_connection.as_ref().is_some_and(|d| d["connected"] == true) { "ok" } else { "DOWN" },
            st.redis_connection.as_ref().map(|r| if r["connected"] == true { "ok" } else { "not connected" }).unwrap_or("n/a"),
            st.online_workers.len(),
            st.online_content_apps.len(),
        ));
        if let Some(sto) = &st.storage {
            let gib = |k: &str| sto[k].as_f64().map(|x| format!("{:.1} GiB", x / 1073741824.0)).unwrap_or_default();
            lines.push(format!("Storage       used {} / total {}   free {}", gib("used"), gib("total"), gib("free")));
        }
        if st.domain_enabled == Some(true) {
            lines.push("Domains       enabled".into());
        }
    }
    lines.push(String::new());
    lines.push("Plugins and capabilities".into());
    for (k, v) in &caps.versions {
        lines.push(format!("  {k:<14} {v}"));
    }
    use crate::pulp::Feature::*;
    for (f, label) in [
        (Rpm, "RPM"),
        (File, "File"),
        (Deb, "Deb"),
        (RpmCopy, "RPM copy API"),
        (DebCopy, "Deb copy API"),
        (ChunkedUpload, "chunked uploads"),
        (NativeReplication, "native replication (tech preview)"),
        (Users, "users/groups/roles API"),
    ] {
        lines.push(format!("  [{}] {label}", if caps.supports(f) { "x" } else { " " }));
    }
    if let Some(e) = &caps.openapi_error {
        lines.push(format!("  OpenAPI unavailable: {e}"));
    }
    lines.push(String::new());
    lines.push("Statistics".into());
    let mut n = |path, state| stat(Stat::Count(path, state));
    lines.push(format!(
        "  RPM repositories   {:>8}   File repositories {:>8}",
        n("repositories/rpm/rpm/", None),
        n("repositories/file/file/", None)
    ));
    lines.push(format!(
        "  RPM distributions  {:>8}   File distributions{:>8}",
        n("distributions/rpm/rpm/", None),
        n("distributions/file/file/", None)
    ));
    lines.push(format!(
        "  RPM packages       {:>8}   File content      {:>8}",
        n("content/rpm/packages/", None),
        n("content/file/files/", None)
    ));
    if caps.supports(Deb) {
        lines.push(format!(
            "  Deb repositories {:>6}   distributions {:>6}   packages {:>8}",
            n(RepoType::Deb.repositories(), None),
            n(RepoType::Deb.distributions(), None),
            n(RepoType::Deb.content(), None)
        ));
    }
    lines.push(format!(
        "  Tasks running {}  waiting {}  failed (all time) {}",
        n("tasks/", Some("running")),
        n("tasks/", Some("waiting")),
        n("tasks/", Some("failed"))
    ));
    lines.push(format!("  {}", stat(Stat::ContentViews)));
    // Operators compare it: only the same key shares Content Views.
    if app.cv_store.as_ref().is_ok_and(|st| st.external) {
        lines.push(format!("  shared database scope: {}", conn.scope));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inside_tmux_the_copy_also_passes_through_to_the_outer_terminal() {
        let plain = "\x1b]52;c;aHR0cDovL3gv\x07";
        assert_eq!(osc52("http://x/", false), plain);
        assert_eq!(osc52("http://x/", true), format!("{plain}\x1bPtmux;\x1b\x1b]52;c;aHR0cDovL3gv\x07\x1b\\"));
    }

    #[test]
    fn repo_body_sends_only_changes_on_edit() {
        let item = json!({ "name": "os", "description": "d", "retain_repo_versions": 5, "autopublish": false, "pulp_labels": {"a": "b"} });
        let mut f = repo_form(RepoType::File, None, Some(&item), "");
        assert_eq!(repo_body(&f, true).unwrap(), json!({}));
        f.fields.iter_mut().find(|x| x.key == "retain_repo_versions").unwrap().value.clear();
        f.fields.iter_mut().find(|x| x.key == "autopublish").unwrap().value = "true".into();
        assert_eq!(repo_body(&f, true).unwrap(), json!({ "retain_repo_versions": null, "autopublish": true }));
        let new = repo_form(RepoType::Rpm, None, None, "");
        let mut new = new;
        new.fields[0].value = "x".into();
        let b = repo_body(&new, false).unwrap();
        assert_eq!(b["name"], "x");
        assert!(b.get("description").is_none() && b["retain_repo_versions"].is_null());
    }

    #[test]
    fn repo_body_sets_or_clears_the_remote() {
        let set = |f: &mut Form, k: &str, v: &str| f.fields.iter_mut().find(|x| x.key == k).unwrap().value = v.into();
        let item = json!({ "name": "os", "remote": "/r/1/" });
        let mut f = repo_form(RepoType::Rpm, None, Some(&item), "epel");
        assert_eq!((f.get("remote"), f.notes.len()), ("epel", 0));
        assert_eq!(repo_body(&f, true).unwrap(), json!({}), "unchanged: not sent");
        set(&mut f, "remote", "updates");
        assert_eq!(repo_body(&f, true).unwrap(), json!({ "remote": "updates" }), "the name; the caller sends its href");
        set(&mut f, "remote", "");
        assert_eq!(repo_body(&f, true).unwrap(), json!({ "remote": null }), "cleared");
        // A remote that could not be read: the field is empty, and empty keeps it.
        let f = repo_form(RepoType::Rpm, None, Some(&item), "");
        assert_eq!(repo_body(&f, true).unwrap(), json!({}));
        assert!(f.notes.iter().any(|n| n.contains("/r/1/") && n.contains("empty Remote keeps it")), "{:?}", f.notes);
        let mut new = repo_form(RepoType::Rpm, None, None, "");
        set(&mut new, "name", "x");
        assert!(repo_body(&new, false).unwrap().get("remote").is_none(), "create: empty is not sent");
        set(&mut new, "remote", "epel");
        assert_eq!(repo_body(&new, false).unwrap()["remote"], "epel");
    }

    #[test]
    fn remote_body_on_create_sends_only_the_filled_fields() {
        let f = Form::new(
            "t",
            vec![
                Field::text("name", "Name").value("r"),
                Field::text("url", "URL").value("https://h/repo/"),
                Field::new("tls_validation", "Validate TLS", FieldKind::Bool).value("false"),
                Field::text("proxy_url", "Proxy URL"),
                Field::new("download_concurrency", "Download concurrency", FieldKind::Number),
                Field::text("ca_cert", "CA cert (PEM file path)"),
            ],
        );
        let (b, files) = remote_body(&f, false).unwrap();
        assert_eq!(b, json!({ "name": "r", "url": "https://h/repo/", "tls_validation": false }));
        assert!(files.is_empty());
    }

    #[test]
    fn profile_form_database_round_trip() {
        let local = std::path::Path::new("/data/pulp-tui.db");
        let p = Profile { url: "https://h".into(), auth: AuthMethod::None, ..Default::default() };
        let top = Database { url: "postgres://u@db/x".into(), secret: Some("env:PW".into()), ca_cert: None };
        let set = |f: &mut Form, k: &str, v: &str| f.fields.iter_mut().find(|x| x.key == k).unwrap().value = v.into();
        for inherited in [None, Some(&top)] {
            let f = profile_form(Some("lab"), &p, inherited, local);
            assert_eq!(f.get("database"), inherited.map_or("/data/pulp-tui.db", |d| d.url.as_str()));
            assert_eq!(profile_from_form(&f, inherited, local).unwrap().database, None, "untouched: no block");
        }
        let mut f = profile_form(Some("lab"), &p, Some(&top), local);
        assert_eq!((f.get("db_secret"), f.get("db_ca_cert")), ("env:PW", ""), "the secret starts with the inherited one");
        set(&mut f, "db_secret", "");
        set(&mut f, "database", "/srv/cv.db");
        assert_eq!(profile_from_form(&f, Some(&top), local).unwrap().database.unwrap().url, "sqlite:///srv/cv.db");
        for same in ["", "local", "/data/pulp-tui.db", "sqlite:///data/pulp-tui.db"] {
            set(&mut f, "database", same);
            assert!(profile_from_form(&f, Some(&top), local).unwrap().database.unwrap().is_local(), "{same:?} is the local file");
        }
        set(&mut f, "database", "postgres://u:pw@db/x");
        let e = profile_from_form(&f, Some(&top), local).unwrap_err();
        assert!(e.contains("put the database password in secret") && !e.contains("u:pw"), "{e}");
    }

    #[test]
    fn profile_form_shows_the_own_database_not_the_inherited_one() {
        let local = std::path::Path::new("/data/pulp-tui.db");
        let own = Database { url: "postgres://o@own/y".into(), secret: Some("keyring".into()), ca_cert: Some("/ca.pem".into()) };
        let top = Database { url: "postgres://u@db/x".into(), secret: Some("env:PW".into()), ca_cert: None };
        let mut p = Profile { url: "https://h".into(), auth: AuthMethod::None, database: Some(own.clone()), ..Default::default() };
        let f = profile_form(Some("lab"), &p, Some(&top), local);
        assert_eq!((f.get("database"), f.get("db_secret"), f.get("db_ca_cert")), (own.url.as_str(), "keyring", "/ca.pem"));
        assert_eq!(profile_from_form(&f, Some(&top), local).unwrap().database, Some(own));
        // The local marker hides the inherited database and its secret.
        p.database = Some(Database { url: LOCAL_DATABASE.into(), secret: None, ca_cert: None });
        let f = profile_form(Some("lab"), &p, Some(&top), local);
        assert_eq!((f.get("database"), f.get("db_secret")), ("/data/pulp-tui.db", ""));
        assert!(profile_from_form(&f, Some(&top), local).unwrap().database.unwrap().is_local());
    }

    #[tokio::test]
    async fn editing_a_profile_keeps_its_database() {
        use crate::store::Store;
        use crate::tui::app::Modal;
        let dir = tempfile::tempdir().unwrap();
        let db = Database { url: "postgres://u@db.example.com/x".into(), secret: Some("keyring".into()), ca_cert: Some("/ca.pem".into()) };
        let mut cfg = crate::config::Config::default();
        cfg.profiles.insert(
            "lab".into(),
            Profile { url: "http://h".into(), auth: AuthMethod::None, database: Some(db.clone()), ..Default::default() },
        );
        let st = std::sync::Arc::new(Store::open_in_memory().await.unwrap());
        let mut app = App::new(cfg, dir.path().join("config.toml"), st.clone(), Ok(st));
        profile_action(&mut app, Act::Edit, json!({ "name": "lab" }));
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no edit form") };
        form.fields.iter_mut().find(|f| f.key == "timeout_secs").unwrap().value = "45".into();
        submit(&mut app, &form).unwrap();
        let p = &app.cfg.profiles["lab"];
        assert_eq!((p.timeout_secs, p.database.as_ref()), (45, Some(&db)), "an untouched database field keeps the block");
        let saved = crate::config::Config::load(&dir.path().join("config.toml")).unwrap();
        assert_eq!(saved.profiles["lab"].database, Some(db), "and the file keeps the [profiles.lab.database] block");
    }

    #[tokio::test]
    async fn a_failed_save_leaves_the_profile_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Config::default();
        let old = Profile { url: "http://h".into(), auth: AuthMethod::None, ..Default::default() };
        cfg.profiles.insert("lab".into(), old.clone());
        let st = std::sync::Arc::new(Store::open_in_memory().await.unwrap());
        let mut app = App::new(cfg, dir.path().join("config.toml"), st.clone(), Ok(st));
        std::fs::write(dir.path().join("file"), b"").unwrap();
        app.cfg_path = dir.path().join("file").join("config.toml"); // its parent is a file: the save fails
        let new = Profile { database: Some(Database { url: "postgres://u@db/x".into(), secret: None, ca_cert: None }), ..old.clone() };
        let e = put_profile(&mut app, "lab", new).unwrap_err();
        assert!(e.starts_with("cannot save "), "{e}");
        assert_eq!(app.cfg.profiles["lab"], old, "memory and file still agree");
    }

    #[test]
    fn sync_policies_without_schema() {
        assert_eq!(sync_policies(None, RepoType::Rpm), ["additive", "mirror_complete", "mirror_content_only"]);
        assert_eq!(sync_policies(None, RepoType::File), ["additive", "mirror"]);
        assert_eq!(sync_policies(None, RepoType::Deb), ["additive", "mirror"]);
    }

    #[test]
    fn deb_remote_fields_follow_the_server_schema_and_prefill_on_edit() {
        let keys = |f: &[Field]| f.iter().map(|f| f.key.clone()).collect::<Vec<_>>();
        let all = ["distributions", "components", "architectures", "sync_sources", "sync_udebs", "sync_installer"];
        let all = [&all[..], &["ignore_missing_package_indices", "gpgkey"]].concat();
        let fields = remote_extra_fields(RepoType::Deb, None, &Value::Null);
        assert_eq!(keys(&fields), all);
        assert!(fields[0].required && !fields[1].required);
        assert_eq!(remote_url_help(RepoType::Deb), "Debian archive base URL (the directory that holds dists/)");

        // A server without these fields (older pulp_deb) shows only what it has.
        let mut caps = Capabilities::default();
        caps.schemas.insert("deb.AptRemote".into(), ["distributions", "components"].map(String::from).into());
        assert_eq!(keys(&remote_extra_fields(RepoType::Deb, Some(&caps), &Value::Null)), ["distributions", "components"]);

        let v = json!({ "distributions": "bookworm", "components": "main", "sync_sources": true, "gpgkey": "-----BEGIN PGP" });
        let f = remote_extra_fields(RepoType::Deb, None, &v);
        let value = |k: &str| f.iter().find(|x| x.key == k).unwrap().value.clone();
        assert_eq!(
            (value("distributions"), value("components"), value("architectures")),
            ("bookworm".into(), "main".into(), String::new())
        );
        assert_eq!((value("sync_sources"), value("sync_udebs")), ("true".into(), "false".into()));
        assert_eq!(value("gpgkey"), "", "the key is a file path, never prefilled");
        assert!(f.iter().all(|x| !x.changed()), "an untouched edit form sends nothing");
    }

    /// Edit a Deb remote on a mock server: set the fields in `set`, submit, and return the PATCH body.
    async fn edit_deb_remote(set: &[(&str, &str)]) -> Value {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        let href = format!("{API}remotes/deb/apt/1/");
        wiremock::Mock::given(method("PATCH"))
            .and(path(href.clone()))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({ "pulp_href": href })))
            .mount(&server)
            .await;
        let (mut app, mut rx) = connected(&server).await;
        let item = json!({ "pulp_href": href, "name": "d", "url": "http://u/", "policy": "immediate", "distributions": "bookworm",
            "tls_validation": true, "pulp_labels": {} });
        remote_action(&mut app, RepoType::Deb, Act::Edit, item);
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no edit form") };
        for (k, v) in set {
            form.fields.iter_mut().find(|f| f.key == *k).unwrap().value = v.to_string();
        }
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        assert!(
            app.notices.iter().any(|n| n.text == "update remote d: done"),
            "{:?}",
            app.notices.iter().map(|n| &n.text).collect::<Vec<_>>()
        );
        let patches = server.received_requests().await.unwrap();
        patches.iter().find(|r| r.method.as_str() == "PATCH").unwrap().body_json().unwrap()
    }

    #[tokio::test]
    async fn prune_orphans_counts_with_the_protection_time_it_sends() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path, query_param};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path(format!("{API}content/")))
            .and(query_param("orphaned_for", "60"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({ "count": 12, "next": null, "results": [] })))
            .mount(&server)
            .await;
        wiremock::Mock::given(method("POST"))
            .and(path(format!("{API}orphans/cleanup/")))
            .respond_with(wiremock::ResponseTemplate::new(202).set_body_json(json!({ "task": format!("{API}tasks/1/") })))
            .expect(1)
            .mount(&server)
            .await;
        let reports = json!([
            { "code": "clean-up.content", "message": "Clean up orphan Content", "total": 12, "done": 12 },
            { "code": "clean-up.artifacts", "message": "Clean up orphan Artifacts", "total": 30, "done": 30 },
        ]);
        let task =
            json!({ "pulp_href": format!("{API}tasks/1/"), "state": "completed", "name": "orphan_cleanup", "progress_reports": reports });
        get(&server, &format!("{API}tasks/1/"), task).await;
        let (mut app, mut rx) = connected(&server).await;
        app.list.kind = Kind::Versions(None);
        run(&mut app, Act::PruneOrphans, Value::Null);
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
        assert_eq!(form.fields[0].value, "1440", "Pulp's default");
        form.fields[0].value = "-1".into();
        assert!(submit(&mut app, &form).is_err(), "minutes are a whole number from 0");
        form.fields[0].value = "60".into();
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, typed, on_yes, .. }) = app.modal.take() else { panic!("no confirm") };
        assert_eq!(typed.as_deref(), Some("lab"), "the operator types the profile name");
        assert!(lines.iter().any(|l| l.contains("12 content unit(s) orphaned for 60 minutes or more")), "{lines:?}");
        on_yes.unwrap()(&mut app);
        settle(&mut app, &mut rx).await;
        let requests = server.received_requests().await.unwrap();
        let post: Value = requests.iter().find(|r| r.method.as_str() == "POST").unwrap().body_json().unwrap();
        assert_eq!(post, json!({ "orphan_protection_time": 60 }));
        let texts: Vec<&String> = app.notices.iter().map(|n| &n.text).collect();
        assert!(texts.iter().any(|t| *t == "prune orphans on lab: deleted 12 content unit(s) and 30 artifact(s)"), "{texts:?}");
    }

    #[tokio::test]
    async fn prune_orphans_needs_the_cleanup_endpoint() {
        let server = wiremock::MockServer::start().await;
        let (mut app, _rx) = connected(&server).await;
        let mut caps = plugins();
        caps.paths.insert("repositories/".into());
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        app.list.kind = Kind::Versions(None);
        run(&mut app, Act::PruneOrphans, Value::Null);
        assert!(app.modal.is_none());
        assert_eq!(app.notices.back().map(|n| n.text.as_str()), Some("orphan cleanup is not supported by this server"));
    }

    #[tokio::test]
    async fn deb_remote_gpgkey_is_read_from_a_file_and_kept_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("key.asc");
        std::fs::write(&key, "-----BEGIN PGP PUBLIC KEY BLOCK-----\n").unwrap();
        let body = edit_deb_remote(&[("gpgkey", key.to_str().unwrap())]).await;
        assert_eq!(body, json!({ "gpgkey": "-----BEGIN PGP PUBLIC KEY BLOCK-----\n" }));
        // Empty = unchanged: the key is not sent, other changes still are.
        let body = edit_deb_remote(&[("distributions", "bookworm bookworm-updates")]).await;
        assert_eq!(body, json!({ "distributions": "bookworm bookworm-updates" }));
    }

    #[tokio::test]
    async fn deb_copy_sends_the_packages_the_filter_selects_as_a_structured_copy() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        let (src, dst) = (format!("{API}repositories/deb/apt/src/"), format!("{API}repositories/deb/apt/dst/"));
        get(
            &server,
            &format!("{API}repositories/deb/apt/"),
            page(json!([{ "pulp_href": src, "name": "src", "latest_version_href": format!("{src}versions/3/") }])),
        )
        .await;
        let pkg = |n: &str| json!({ "pulp_href": format!("{API}content/deb/packages/{n}/"), "package": n, "version": "1.0-1", "architecture": "amd64" });
        get(&server, &format!("{API}content/deb/packages/"), page(json!([pkg("fixture-beta"), pkg("fixture-beta-dbg")]))).await;
        wiremock::Mock::given(method("POST"))
            .and(path(format!("{API}deb/copy/")))
            .respond_with(wiremock::ResponseTemplate::new(202).set_body_json(json!({ "task": format!("{API}tasks/1/") })))
            .mount(&server)
            .await;
        let (mut app, mut rx) = connected(&server).await;
        app.conn.as_mut().unwrap().caps = Arc::new(plugins());
        for (rules, want) in [("exclude name=*-dbg", Some(json!([format!("{API}content/deb/packages/fixture-beta/")]))), ("", None)] {
            repo_action(&mut app, RepoType::Deb, Act::CopyContent, json!({ "pulp_href": dst, "name": "dst" }));
            let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
            assert!(form.fields.iter().all(|f| f.key != "deps"), "Deb copy has no dependency solving");
            assert_eq!(form.fields[1].help, filter_help(RepoType::Deb));
            form.fields[0].value = "src".into();
            form.fields[1].value = rules.into();
            submit(&mut app, &form).unwrap();
            settle(&mut app, &mut rx).await;
            let Some(Modal::Confirm { lines, on_yes, .. }) = app.modal.take() else { panic!("no confirm") };
            assert_eq!(
                lines[0],
                format!(
                    "Copy {} from src@v3 into dst.",
                    want.as_ref().map_or("all content".into(), |u| format!("{} unit(s)", u.as_array().unwrap().len()))
                )
            );
            on_yes.unwrap()(&mut app);
            settle(&mut app, &mut rx).await;
            let posts = server.received_requests().await.unwrap();
            let sent: Value = posts.iter().rfind(|r| r.method.as_str() == "POST").unwrap().body_json().unwrap();
            let mut cfg = json!({ "source_repo_version": format!("{src}versions/3/"), "dest_repo": dst });
            if let Some(u) = want {
                cfg["content"] = u;
            }
            assert_eq!(sent, json!({ "config": [cfg], "structured": true, "dependency_solving": false }));
        }
    }

    #[tokio::test]
    async fn a_large_copy_is_sent_in_batches_one_after_the_other() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        let (src, dst) = (format!("{API}repositories/deb/apt/src/"), format!("{API}repositories/deb/apt/dst/"));
        let repo = json!([{ "pulp_href": src, "name": "src", "latest_version_href": format!("{src}versions/3/") }]);
        get(&server, &format!("{API}repositories/deb/apt/"), page(repo)).await;
        let hrefs: Vec<String> = (0..30_000).map(|i| format!("{API}content/deb/packages/{i:036}/")).collect();
        let pkgs: Vec<Value> =
            hrefs.iter().map(|h| json!({ "pulp_href": h, "package": "p", "version": "1.0-1", "architecture": "amd64" })).collect();
        get(&server, &format!("{API}content/deb/packages/"), page(json!(pkgs))).await;
        get(&server, &format!("{API}tasks/1/"), json!({ "pulp_href": format!("{API}tasks/1/"), "state": "completed", "name": "copy" }))
            .await;
        wiremock::Mock::given(method("POST"))
            .and(path(format!("{API}deb/copy/")))
            .respond_with(wiremock::ResponseTemplate::new(202).set_body_json(json!({ "task": format!("{API}tasks/1/") })))
            .mount(&server)
            .await;
        let (mut app, mut rx) = connected(&server).await;
        app.conn.as_mut().unwrap().caps = Arc::new(plugins());
        repo_action(&mut app, RepoType::Deb, Act::CopyContent, json!({ "pulp_href": dst, "name": "dst" }));
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
        form.fields[0].value = "src".into();
        form.fields[1].value = "include arch=amd64".into();
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { on_yes, .. }) = app.modal.take() else { panic!("no confirm") };
        on_yes.unwrap()(&mut app);
        settle(&mut app, &mut rx).await;
        let sent = server.received_requests().await.unwrap();
        let kinds: Vec<_> = sent
            .iter()
            .filter(|r| r.method.as_str() == "POST" || r.url.path().ends_with("/tasks/1/"))
            .map(|r| r.method.to_string())
            .collect();
        assert_eq!(kinds, ["POST", "GET", "POST"], "the first batch is done before the second is sent");
        let units: Vec<String> = sent
            .iter()
            .filter(|r| r.method.as_str() == "POST")
            .flat_map(|r| serde_json::from_value::<Vec<String>>(r.body_json::<Value>().unwrap()["config"][0]["content"].clone()).unwrap())
            .collect();
        assert_eq!(units, hrefs);
    }

    #[tokio::test]
    async fn copy_and_upload_need_the_server_api_before_the_operator_types() {
        use crate::tui::app::{Conn, Modal};
        let server = wiremock::MockServer::start().await;
        let (mut app, mut rx) = connected(&server).await;
        // The plugins are there, but not rpm/copy/, deb/copy/ and uploads/.
        let mut caps = plugins();
        caps.paths.insert("repositories/rpm/rpm/".into());
        let mut settings = app.cfg.settings.clone();
        settings.upload_chunk_mb = 1;
        let client = PulpClient::new("lab", &app.cfg.profiles["lab"], &settings).unwrap();
        app.conn = Some(Conn { client, caps: Arc::new(caps), scope: "lab".into() });
        let repo = |coll: &str| json!({ "pulp_href": format!("{API}repositories/{coll}/r/"), "name": "r" });
        let last_error = |app: &App| app.notices.iter().rfind(|n| n.level == Level::Error).map(|n| n.text.clone()).unwrap_or_default();

        repo_action(&mut app, RepoType::Rpm, Act::CopyContent, repo("rpm/rpm"));
        assert!(app.modal.is_none(), "no form");
        assert_eq!(last_error(&app), "not supported by this server: RPM copy API (rpm/copy/)");
        repo_action(&mut app, RepoType::Deb, Act::CopyContent, repo("deb/apt"));
        assert!(app.modal.is_none(), "no form");
        assert_eq!(last_error(&app), "not supported by this server: Deb copy API (deb/copy/)");
        app.notices.clear();
        content_action(&mut app, format!("{API}repositories/deb/apt/r/versions/1/"), Act::AddToRepo);
        assert!(app.modal.is_none(), "no form");
        assert_eq!(last_error(&app), "not supported by this server: Deb copy API (deb/copy/)");
        // File copies through modify/.
        repo_action(&mut app, RepoType::File, Act::CopyContent, repo("file/file"));
        assert!(matches!(app.modal.take(), Some(Modal::Form { .. })));

        // Only a file larger than one chunk needs uploads/: refused before any request.
        let dir = tempfile::tempdir().unwrap();
        let upload = |app: &mut App, size: usize| {
            let f = dir.path().join(format!("{size}.rpm"));
            std::fs::write(&f, vec![0u8; size]).unwrap();
            repo_action(app, RepoType::Rpm, Act::Upload, repo("rpm/rpm"));
            let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no upload form") };
            form.fields[0].value = f.display().to_string();
            submit(app, &form).unwrap();
        };
        upload(&mut app, 1024 * 1024 + 1);
        settle(&mut app, &mut rx).await;
        assert!(last_error(&app).ends_with(": not supported by this server: chunked uploads API (uploads/)"), "{}", last_error(&app));
        assert!(server.received_requests().await.unwrap().is_empty(), "no request reached the server");
        upload(&mut app, 1024 * 1024);
        settle(&mut app, &mut rx).await;
        assert_eq!(server.received_requests().await.unwrap()[0].url.path(), format!("{API}content/rpm/packages/"), "one chunk: no check");
    }

    #[tokio::test]
    async fn deb_content_is_added_with_a_structured_copy() {
        use crate::tui::app::Modal;
        let server = wiremock::MockServer::start().await;
        let accepted = wiremock::ResponseTemplate::new(202).set_body_json(json!({ "task": format!("{API}tasks/1/") }));
        wiremock::Mock::given(wiremock::matchers::method("POST")).respond_with(accepted).mount(&server).await;
        let done = json!({ "pulp_href": format!("{API}tasks/1/"), "state": "completed", "created_resources": ["/r/dst/versions/3/"] });
        get(&server, &format!("{API}tasks/1/"), done).await;
        let (mut app, mut rx) = connected(&server).await;
        app.conn.as_mut().unwrap().caps = Arc::new(plugins());
        for (t, coll) in [(RepoType::Deb, "deb/apt"), (RepoType::Rpm, "rpm/rpm")] {
            let dst = format!("{API}repositories/{coll}/dst/");
            get(&server, &format!("{API}repositories/{coll}/"), page(json!([{ "pulp_href": dst, "name": "dst" }]))).await;
            let unit = format!("{API}{}1/", t.content());
            app.list.marked = [unit.clone()].into();
            content_action(&mut app, format!("{API}repositories/{coll}/src/versions/2/"), Act::AddToRepo);
            let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
            form.fields[0].value = "dst".into();
            submit(&mut app, &form).unwrap();
            settle(&mut app, &mut rx).await;
            assert_eq!(app.notices.back().unwrap().text, "added 1 unit(s) to dst: version 3", "told when the task ends");
            assert!(app.list.marked.is_empty());
            let sent = server.received_requests().await.unwrap();
            let post = sent.iter().rfind(|r| r.method.as_str() == "POST").unwrap();
            let body: Value = post.body_json().unwrap();
            match t {
                RepoType::Deb => {
                    assert_eq!(post.url.path(), format!("{API}deb/copy/"));
                    let cfg = json!({ "source_repo_version": format!("{API}repositories/deb/apt/src/versions/2/"), "dest_repo": dst, "content": [unit] });
                    assert_eq!(body, json!({ "config": [cfg], "structured": true, "dependency_solving": false }));
                }
                RepoType::Rpm | RepoType::File => {
                    assert_eq!((post.url.path(), body), (format!("{dst}modify/").as_str(), json!({ "add_content_units": [unit] })));
                }
            }
        }
    }

    #[tokio::test]
    async fn deb_remove_content_takes_the_release_records_along() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path, query_param};
        let server = wiremock::MockServer::start().await;
        let repo = format!("{API}repositories/deb/apt/r/");
        let latest = format!("{repo}versions/2/");
        let (p1, p2) = (format!("{API}content/deb/packages/1/"), format!("{API}content/deb/packages/2/"));
        get(&server, &repo, json!({ "pulp_href": repo, "name": "r", "latest_version_href": latest })).await;
        let prcs = page(json!([{ "pulp_href": "/prc/1/", "package": p1 }, { "pulp_href": "/prc/2/", "package": p2 }]));
        wiremock::Mock::given(method("GET"))
            .and(path(format!("{API}content/deb/package_release_components/")))
            .and(query_param("repository_version", latest.as_str()))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(prcs))
            .mount(&server)
            .await;
        let accepted = wiremock::ResponseTemplate::new(202).set_body_json(json!({ "task": format!("{API}tasks/1/") }));
        wiremock::Mock::given(method("POST")).and(path(format!("{repo}modify/"))).respond_with(accepted).mount(&server).await;
        let (mut app, mut rx) = connected(&server).await;
        app.list.marked = [p1.clone()].into();
        // The browsed version is older: the removal starts from the latest one.
        content_action(&mut app, format!("{repo}versions/1/"), Act::RemoveContent);
        let Some(Modal::Confirm { on_yes, .. }) = app.modal.take() else { panic!("no confirm") };
        on_yes.unwrap()(&mut app);
        settle(&mut app, &mut rx).await;
        assert_eq!(posts_to(&server, &format!("{repo}modify/")).await, [json!({ "remove_content_units": [p1, "/prc/1/"] })]);
    }

    /// Remove the marked unit from the browsed version 1 on a mock server whose task ends as `task`.
    /// `away`: the operator opens the Task Center before the task ends.
    async fn remove_one(task: Value, away: bool) -> App {
        use crate::tui::app::{ListState, Modal, Screen};
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        let repo = format!("{API}repositories/file/file/r/");
        get(&server, &repo, json!({ "pulp_href": repo, "name": "r", "latest_version_href": format!("{repo}versions/1/") })).await;
        let accepted = wiremock::ResponseTemplate::new(202).set_body_json(json!({ "task": format!("{API}tasks/1/") }));
        wiremock::Mock::given(method("POST")).and(path(format!("{repo}modify/"))).respond_with(accepted).mount(&server).await;
        let mut t = json!({ "pulp_href": format!("{API}tasks/1/"), "name": "pulpcore.app.tasks.repository.add_and_remove" });
        t.as_object_mut().unwrap().extend(task.as_object().unwrap().clone());
        get(&server, &format!("{API}tasks/1/"), t).await;
        let (mut app, mut rx) = connected(&server).await;
        let k = Kind::Content { version: format!("{repo}versions/1/"), ctype: 0 };
        (app.screen, app.list) = (Screen::List(k.clone()), ListState::new(k));
        app.list.marked = [format!("{API}content/file/files/1/")].into();
        content_action(&mut app, format!("{repo}versions/1/"), Act::RemoveContent);
        let Some(Modal::Confirm { on_yes, .. }) = app.modal.take() else { panic!("no confirm") };
        on_yes.unwrap()(&mut app);
        if away {
            app.open(Screen::List(Kind::Tasks));
        }
        settle(&mut app, &mut rx).await;
        app
    }

    #[tokio::test]
    async fn removed_content_shows_the_new_version_where_the_old_one_is_still_shown() {
        use crate::tui::app::Screen;
        let repo = format!("{API}repositories/file/file/r/");
        let done = json!({ "state": "completed", "created_resources": [format!("{repo}versions/2/")] });
        let texts = |app: &App| app.notices.iter().map(|n| n.text.clone()).collect::<Vec<_>>();

        let app = remove_one(done.clone(), false).await;
        assert_eq!(app.screen, Screen::List(Kind::Content { version: format!("{repo}versions/2/"), ctype: 0 }));
        assert!(app.list.marked.is_empty());
        assert!(texts(&app).contains(&"removed 1 unit(s): now showing version 2".into()), "{:?}", texts(&app));
        assert_eq!(app.tasks.items[0].label, "remove 1 unit(s)", "the Task Center keeps the readable label");

        let app = remove_one(done, true).await;
        assert_eq!(app.screen, Screen::List(Kind::Tasks), "the operator left: the screen stays");
        assert!(texts(&app).contains(&"removed 1 unit(s): new version 2".into()), "{:?}", texts(&app));

        let app = remove_one(json!({ "state": "failed", "error": { "description": "boom" } }), false).await;
        assert_eq!(app.screen, Screen::List(Kind::Content { version: format!("{repo}versions/1/"), ctype: 0 }));
        assert_eq!(app.list.marked.len(), 1, "a failure keeps the marks");
        let errors: Vec<_> = app.notices.iter().filter(|n| n.level == Level::Error).map(|n| n.text.clone()).collect();
        assert_eq!(errors, ["task failed: remove 1 unit(s): boom"], "Pulp's message, once");
    }

    #[tokio::test]
    async fn only_a_deb_publish_asks_for_a_signing_service() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        for coll in ["publications/deb/apt/", "publications/rpm/rpm/"] {
            get(&server, &format!("{API}{coll}"), page(json!([]))).await;
            wiremock::Mock::given(method("POST"))
                .and(path(format!("{API}{coll}")))
                .respond_with(wiremock::ResponseTemplate::new(201).set_body_json(json!({ "pulp_href": format!("{API}{coll}1/") })))
                .mount(&server)
                .await;
        }
        let (mut app, mut rx) = connected(&server).await;
        let repo = |t: &str| json!({ "pulp_href": format!("{API}repositories/{t}/r/"), "name": "r", "latest_version_href": "/v/" });

        repo_action(&mut app, RepoType::Rpm, Act::Publish, repo("rpm/rpm"));
        assert!(app.modal.is_none(), "RPM publishes at once");
        settle(&mut app, &mut rx).await;

        repo_action(&mut app, RepoType::Deb, Act::Publish, repo("deb/apt"));
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
        assert_eq!(form.fields.len(), 1);
        assert_eq!(form.fields[0].key, "signing_service");
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        form.fields[0].value = "/ss/1/".into();
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;

        let posts: Vec<Value> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "POST")
            .map(|r| r.body_json().unwrap())
            .collect();
        assert_eq!(
            posts,
            [
                json!({ "repository_version": "/v/" }),
                json!({ "repository_version": "/v/" }),
                json!({ "repository_version": "/v/", "signing_service": "/ss/1/" }),
            ]
        );
    }

    /// The bodies of the POSTs to `p`.
    async fn posts_to(server: &wiremock::MockServer, p: &str) -> Vec<Value> {
        let sent = server.received_requests().await.unwrap();
        sent.iter().filter(|r| r.method.as_str() == "POST" && r.url.path() == p).map(|r| r.body_json().unwrap()).collect()
    }

    /// A Deb server without publications and distributions: each POST creates `.../9/`.
    async fn deb_distribute_server() -> wiremock::MockServer {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        for coll in ["publications/deb/apt/", "distributions/deb/apt/", "distributions/"] {
            get(&server, &format!("{API}{coll}"), page(json!([]))).await;
        }
        for coll in ["publications/deb/apt/", "distributions/deb/apt/"] {
            wiremock::Mock::given(method("POST"))
                .and(path(format!("{API}{coll}")))
                .respond_with(wiremock::ResponseTemplate::new(201).set_body_json(json!({ "pulp_href": format!("{API}{coll}9/") })))
                .mount(&server)
                .await;
            get(&server, &format!("{API}{coll}9/"), json!({ "pulp_href": format!("{API}{coll}9/"), "base_url": "http://h/d/" })).await;
        }
        server
    }

    fn keys(f: &Form) -> Vec<String> {
        f.fields.iter().map(|x| x.key.clone()).collect()
    }

    #[tokio::test]
    async fn deb_distribute_pins_the_picked_service_or_the_selected_publication() {
        use crate::tui::app::Modal;
        let server = deb_distribute_server().await;
        let (mut app, mut rx) = connected(&server).await;
        let pubs = format!("{API}publications/deb/apt/");
        let repo = |coll: &str| json!({ "pulp_href": format!("{API}repositories/{coll}/r/"), "name": "r", "latest_version_href": "/v/" });
        repo_action(&mut app, RepoType::Rpm, Act::Distribute, repo("rpm/rpm"));
        let Some(Modal::Form { form, .. }) = app.modal.take() else { panic!("no form") };
        assert_eq!(keys(&form), ["dist", "base_path", "mode"], "RPM: no signing service");

        repo_action(&mut app, RepoType::Deb, Act::Distribute, repo("deb/apt"));
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
        assert_eq!(keys(&form), ["dist", "base_path", "mode", "signing_service"]);
        form.fields[3].value = "/ss/1/".into();
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        assert_eq!(posts_to(&server, &pubs).await, [json!({ "repository_version": "/v/", "signing_service": "/ss/1/" })]);

        // Publications D: the selected publication itself, no lookup by version and no new publication.
        let lookups = |sent: &[wiremock::Request]| sent.iter().filter(|r| r.method.as_str() == "GET" && r.url.path() == pubs).count();
        let before = lookups(&server.received_requests().await.unwrap());
        let selected = format!("{pubs}5/");
        let item = json!({ "pulp_href": selected, "repository_version": format!("{API}repositories/deb/apt/r/versions/1/") });
        publication_action(&mut app, RepoType::Deb, Act::Distribute, item);
        let Some(Modal::Form { form, mut submit }) = app.modal.take() else { panic!("no form") };
        assert_eq!(keys(&form), ["dist", "base_path", "mode"]);
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        assert_eq!(lookups(&server.received_requests().await.unwrap()), before);
        assert_eq!(posts_to(&server, &pubs).await.len(), 1);
        let pinned: Vec<Value> =
            posts_to(&server, &format!("{API}distributions/deb/apt/")).await.iter().map(|d| d["publication"].clone()).collect();
        assert_eq!(pinned, [json!(format!("{pubs}9/")), json!(selected)]);
    }

    #[tokio::test]
    async fn deb_publication_and_distribution_forms_send_the_picked_service() {
        use crate::tui::app::Modal;
        let server = deb_distribute_server().await;
        let r = json!({ "pulp_href": format!("{API}repositories/deb/apt/r/"), "name": "r", "latest_version_href": "/v/" });
        get(&server, &format!("{API}repositories/deb/apt/"), page(json!([r]))).await;
        let (mut app, mut rx) = connected(&server).await;
        let set = |form: &mut Form, values: &[(&str, &str)]| {
            for (k, v) in values {
                form.fields.iter_mut().find(|f| f.key == *k).unwrap().value = v.to_string();
            }
        };
        publication_action(&mut app, RepoType::Deb, Act::Create, Value::Null);
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
        assert_eq!(keys(&form), ["repo", "number", "signing_service"]);
        set(&mut form, &[("repo", "r"), ("signing_service", "/ss/1/")]);
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;

        distribution_action(&mut app, RepoType::Deb, Act::Create, Value::Null);
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
        assert!(keys(&form).contains(&"signing_service".to_string()));
        set(&mut form, &[("name", "d"), ("base_path", "d"), ("repo", "r"), ("signing_service", "/ss/2/")]);
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        let sent = posts_to(&server, &format!("{API}publications/deb/apt/")).await;
        let service = |s: &str| json!({ "repository_version": "/v/", "signing_service": s });
        assert_eq!(sent, [service("/ss/1/"), service("/ss/2/")]);
    }

    #[tokio::test]
    async fn repository_delete_names_the_content_views_that_use_it() {
        use crate::tui::app::Modal;
        let server = wiremock::MockServer::start().await;
        let repo = format!("{API}repositories/rpm/rpm/1/");
        for p in [format!("{API}publications/rpm/rpm/"), format!("{repo}versions/"), format!("{API}distributions/rpm/rpm/")] {
            get(&server, &p, page(json!([]))).await;
        }
        let (mut app, mut rx) = connected(&server).await;
        let st = app.cv_store.clone().unwrap();
        let cv = st.create_cv("lab", "web", "").await.unwrap();
        st.set_cv_repo(
            cv,
            &crate::store::CvRepo { repo_href: repo.clone(), repo_name: "os".into(), repo_type: RepoType::Rpm, filters: vec![] },
        )
        .await
        .unwrap();
        repo_action(&mut app, RepoType::Rpm, Act::Delete, json!({ "pulp_href": repo, "name": "os" }));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, .. }) = &app.modal else {
            panic!("no confirm: {:?}", app.notices.iter().map(|n| &n.text).collect::<Vec<_>>())
        };
        assert!(lines.contains(&"WARNING: Content View(s) web use it as a source; their next publish fails.".to_string()), "{lines:?}");
    }

    #[tokio::test]
    async fn deb_version_publish_asks_for_a_signing_service() {
        use crate::tui::app::Modal;
        let server = deb_distribute_server().await;
        let (mut app, mut rx) = connected(&server).await;
        let v = format!("{API}repositories/deb/apt/r/versions/1/");
        version_action(&mut app, Act::Publish, json!({ "pulp_href": v, "number": 1 }));
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
        assert_eq!(keys(&form), ["signing_service"]);
        form.fields[0].value = "/ss/1/".into();
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        assert_eq!(
            posts_to(&server, &format!("{API}publications/deb/apt/")).await,
            [json!({ "repository_version": v, "signing_service": "/ss/1/" })]
        );
        assert!(
            app.notices.iter().any(|n| n.text == format!("publication 9 for {}", short(&v))),
            "{:?}",
            app.notices.iter().map(|n| &n.text).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn no_signing_service_picker_without_the_publication_field() {
        use crate::tui::app::Modal;
        let server = deb_distribute_server().await;
        let (mut app, mut rx) = connected(&server).await;
        let mut caps = Capabilities::default();
        caps.schemas.insert("deb.AptPublication".into(), ["repository_version"].map(String::from).into());
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        let r = format!("{API}repositories/deb/apt/r/");
        let repo = json!({ "pulp_href": r, "name": "r", "latest_version_href": "/v/" });
        repo_action(&mut app, RepoType::Deb, Act::Distribute, repo.clone());
        let Some(Modal::Form { form, .. }) = app.modal.take() else { panic!("no form") };
        assert_eq!(keys(&form), ["dist", "base_path", "mode"]);
        repo_action(&mut app, RepoType::Deb, Act::Publish, repo);
        assert!(app.modal.is_none(), "publishes at once with the repository default");
        settle(&mut app, &mut rx).await;
        version_action(&mut app, Act::Publish, json!({ "pulp_href": format!("{r}versions/1/"), "number": 1 }));
        assert!(app.modal.is_none());
        settle(&mut app, &mut rx).await;
        let sent = posts_to(&server, &format!("{API}publications/deb/apt/")).await;
        assert_eq!(sent, [json!({ "repository_version": "/v/" }), json!({ "repository_version": format!("{r}versions/1/") })]);
    }

    #[tokio::test]
    async fn deb_rollback_keeps_the_signing_service_of_the_served_publication() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path};
        let server = deb_distribute_server().await;
        let (old, now, d) = (
            format!("{API}repositories/deb/apt/r/versions/1/"),
            format!("{API}publications/deb/apt/5/"),
            format!("{API}distributions/deb/apt/1/"),
        );
        let dist = json!({ "pulp_href": d, "name": "prod", "base_path": "prod", "base_url": "http://h/prod/", "publication": now });
        wiremock::Mock::given(method("GET"))
            .and(path(format!("{API}distributions/deb/apt/")))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(page(json!([dist]))))
            .with_priority(1)
            .mount(&server)
            .await;
        let served = json!({ "pulp_href": now, "repository_version": format!("{API}repositories/deb/apt/r/versions/2/"), "signing_service": "/ss/1/" });
        get(&server, &now, served).await;
        wiremock::Mock::given(method("PATCH"))
            .and(path(d.clone()))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({ "pulp_href": d })))
            .mount(&server)
            .await;
        let (mut app, mut rx) = connected(&server).await;
        rollback_preview(&mut app, RepoType::Deb, "prod".into(), old.clone());
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { on_yes, .. }) = app.modal.take() else { panic!("no confirm") };
        on_yes.unwrap()(&mut app);
        settle(&mut app, &mut rx).await;
        let pubs = format!("{API}publications/deb/apt/");
        assert_eq!(posts_to(&server, &pubs).await, [json!({ "repository_version": old, "signing_service": "/ss/1/" })]);
        let sent = server.received_requests().await.unwrap();
        let patch: Value = sent.iter().find(|r| r.method.as_str() == "PATCH").unwrap().body_json().unwrap();
        assert_eq!(patch["publication"], json!(format!("{pubs}9/")));
    }

    #[tokio::test]
    async fn operator_publish_always_makes_a_new_publication() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        let (pubs, r) = (format!("{API}publications/rpm/rpm/"), format!("{API}repositories/rpm/rpm/r/"));
        let v = format!("{r}versions/1/");
        get(&server, &pubs, page(json!([{ "pulp_href": format!("{pubs}1/"), "repository_version": v }]))).await;
        wiremock::Mock::given(method("POST"))
            .and(path(pubs.clone()))
            .respond_with(wiremock::ResponseTemplate::new(201).set_body_json(json!({ "pulp_href": format!("{pubs}9/") })))
            .mount(&server)
            .await;
        let repo = json!({ "pulp_href": r, "name": "r", "latest_version_href": v });
        get(&server, &format!("{API}repositories/rpm/rpm/"), page(json!([repo]))).await;
        let (mut app, mut rx) = connected(&server).await;

        repo_action(&mut app, RepoType::Rpm, Act::Publish, repo);
        settle(&mut app, &mut rx).await;
        version_action(&mut app, Act::Publish, json!({ "pulp_href": v, "number": 1 }));
        settle(&mut app, &mut rx).await;
        publication_action(&mut app, RepoType::Rpm, Act::Create, Value::Null);
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
        form.fields[0].value = "r".into();
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        assert_eq!(posts_to(&server, &pubs).await, vec![json!({ "repository_version": v }); 3], "publication 1 is not reused");
    }

    #[tokio::test]
    async fn publications_distribute_serves_the_selected_rpm_publication() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        let (pubs, dists) = (format!("{API}publications/rpm/rpm/"), format!("{API}distributions/rpm/rpm/"));
        for p in [&pubs, &dists, &format!("{API}distributions/")] {
            get(&server, p, page(json!([]))).await;
        }
        wiremock::Mock::given(method("POST"))
            .and(path(dists.clone()))
            .respond_with(wiremock::ResponseTemplate::new(201).set_body_json(json!({ "pulp_href": format!("{dists}9/") })))
            .mount(&server)
            .await;
        get(&server, &format!("{dists}9/"), json!({ "pulp_href": format!("{dists}9/"), "base_url": "http://h/d/" })).await;
        let (mut app, mut rx) = connected(&server).await;
        let selected = format!("{pubs}5/");
        let item = json!({ "pulp_href": selected, "repository_version": format!("{API}repositories/rpm/rpm/r/versions/1/") });
        publication_action(&mut app, RepoType::Rpm, Act::Distribute, item);
        let Some(Modal::Form { form, mut submit }) = app.modal.take() else { panic!("no form") };
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        let sent = server.received_requests().await.unwrap();
        assert!(!sent.iter().any(|r| r.method.as_str() == "GET" && r.url.path() == pubs), "no lookup by version");
        let pinned: Vec<Value> = posts_to(&server, &dists).await.iter().map(|d| d["publication"].clone()).collect();
        assert_eq!(pinned, [json!(selected)]);
    }

    #[tokio::test]
    async fn mirror_policies_ask_first() {
        use crate::tui::app::Modal;
        use std::{cell::Cell, rc::Rc};
        let st = std::sync::Arc::new(Store::open_in_memory().await.unwrap());
        let mut app = App::new(crate::config::Config::default(), "/dev/null".into(), st.clone(), Ok(st));
        let ran = Rc::new(Cell::new(false));
        confirm_mirror(&mut app, "additive", {
            let r = ran.clone();
            move |_| r.set(true)
        });
        assert!(ran.get() && app.modal.is_none());
        confirm_mirror(&mut app, "mirror_complete", |_| panic!("ran before the confirm"));
        let Some(Modal::Confirm { title, lines, .. }) = &app.modal else { panic!("no confirm") };
        assert_eq!((title.as_str(), lines.len()), ("Confirm mirror sync", 4));
        confirm_mirror(&mut app, "mirror", |_| {});
        let Some(Modal::Confirm { lines, .. }) = &app.modal else { panic!("no confirm") };
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn version_hrefs() {
        assert_eq!(repo_of_version("/p/repositories/rpm/rpm/1/versions/3/"), "/p/repositories/rpm/rpm/1/");
        assert_eq!(version_of("/p/r/1/", 2), "/p/r/1/versions/2/");
        assert_eq!(version_number("/p/r/1/versions/12/"), 12);
        assert_eq!(version_number("/p/r/1/versions/latest/"), 0, "not a version: sorts first");
    }

    const API: &str = "/pulp/api/v3/";

    /// An app connected to `server` as profile `lab`, with an in-memory store.
    async fn connected(server: &wiremock::MockServer) -> (App, tokio::sync::mpsc::UnboundedReceiver<super::super::app::Msg>) {
        let st = Arc::new(Store::open_in_memory().await.unwrap());
        let p = Profile { url: server.uri(), auth: AuthMethod::None, ..Default::default() };
        let mut cfg = crate::config::Config::default();
        cfg.profiles.insert("lab".into(), p.clone());
        let mut app = App::new(cfg, "/dev/null".into(), st.clone(), Ok(st));
        let client = PulpClient::new("lab", &p, &app.cfg.settings).unwrap();
        app.profile = Some("lab".into());
        app.conn = Some(super::super::app::Conn { client, caps: Default::default(), scope: "lab".into() });
        let rx = app.rx.take().unwrap();
        (app, rx)
    }

    /// Capabilities of a server with the RPM, File and Deb plugins whose OpenAPI is not read (every endpoint counts as there).
    fn plugins() -> Capabilities {
        let mut caps = Capabilities::default();
        for p in ["rpm", "file", "deb"] {
            caps.versions.insert(p.into(), "1.0.0".into());
        }
        caps
    }

    async fn settle(app: &mut App, rx: &mut tokio::sync::mpsc::UnboundedReceiver<super::super::app::Msg>) {
        while !app.jobs.is_empty() {
            app.handle_msg(rx.recv().await.unwrap());
        }
    }

    async fn get(server: &wiremock::MockServer, p: &str, body: Value) {
        use wiremock::matchers::{method, path};
        wiremock::Mock::given(method("GET"))
            .and(path(p))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    fn page(results: Value) -> Value {
        json!({ "count": results.as_array().map_or(0, Vec::len), "next": null, "results": results })
    }

    #[tokio::test]
    async fn rpm_publication_delete_names_the_distributions_that_serve_it() {
        use crate::tui::app::Modal;
        let server = wiremock::MockServer::start().await;
        let pubn = format!("{API}publications/rpm/rpm/0190aaaa-bbbb-7000/");
        let version = format!("{API}repositories/rpm/rpm/r1/versions/2/");
        let dists = page(json!([
            { "pulp_href": format!("{API}distributions/rpm/rpm/1/"), "name": "prod-os", "base_path": "prod/os",
              "base_url": "https://h/pulp/content/prod/os/", "publication": pubn },
            { "pulp_href": format!("{API}distributions/rpm/rpm/2/"), "name": "dev-os", "base_path": "dev/os", "publication": null },
        ]));
        get(&server, &format!("{API}distributions/rpm/rpm/"), dists).await;
        let (mut app, mut rx) = connected(&server).await;
        let item = json!({ "pulp_href": pubn, "repository_version": version });
        publication_action(&mut app, RepoType::Rpm, Act::Delete, item.clone());
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, typed, .. }) = &app.modal else { panic!("no confirm: {:?}", app.notices.back().map(|n| &n.text)) };
        assert_eq!(typed.as_deref(), Some("0190aaaa-bbbb"), "the operator types the publication id");
        assert!(lines.contains(&"  prod-os (https://h/pulp/content/prod/os/)".to_string()), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("dev-os")), "{lines:?}");

        // A publication that a Content View serves is refused before any dialog.
        app.modal = None;
        let st = app.store.clone();
        let (cv, env) = (st.create_cv("lab", "web", "").await.unwrap(), st.ensure_library("lab").await.unwrap());
        let b = crate::store::BindingRepo {
            env_id: env,
            cv_id: cv,
            repo_href: "/r/".into(),
            repo_name: "os".into(),
            repo_type: RepoType::Rpm,
            distribution: "d".into(),
            base_path: "b".into(),
            publication: Some(pubn.clone()),
        };
        st.save_binding_repo(&b).await.unwrap();
        publication_action(&mut app, RepoType::Rpm, Act::Delete, item);
        settle(&mut app, &mut rx).await;
        assert!(app.modal.is_none());
        let e = &app.notices.back().unwrap().text;
        assert!(e.contains("refused: Content View web serves publication 0190aaaa-bbbb in Library"), "{e}");
    }

    #[tokio::test]
    async fn version_delete_names_the_distributions_that_protect_it() {
        use crate::tui::app::Modal;
        let server = wiremock::MockServer::start().await;
        let repo = format!("{API}repositories/file/file/r/");
        let p1 = format!("{API}publications/file/file/p1/");
        let pubs = page(json!([
            { "pulp_href": p1, "repository_version": format!("{repo}versions/1/") },
            { "pulp_href": format!("{API}publications/file/file/p2/"), "repository_version": format!("{repo}versions/2/") },
        ]));
        get(&server, &format!("{API}publications/file/file/"), pubs).await;
        let dists = page(json!([
            { "pulp_href": "/d/1/", "name": "pinned", "base_path": "pinned", "base_url": "http://h/pinned/", "publication": p1 },
            { "pulp_href": "/d/2/", "name": "follower", "base_path": "follower", "repository": repo },
        ]));
        get(&server, &format!("{API}distributions/file/file/"), dists).await;
        let (mut app, mut rx) = connected(&server).await;
        let delete = |n: u32| json!({ "pulp_href": format!("{repo}versions/{n}/"), "number": n });

        // Pulp refuses these deletes (HTTP 400): no typed confirm, the operator learns what to re-point.
        version_action(&mut app, Act::Delete, delete(1));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Text { title, lines, .. }) = app.modal.take() else { panic!("no refusal dialog") };
        assert_eq!(title, "Cannot delete version 1");
        assert!(lines.contains(&"  pinned (http://h/pinned/): pinned to a publication of this version".to_string()), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("follower")), "v2 is the newest published version: {lines:?}");
        assert!(lines.iter().any(|l| l.contains("re-point")), "{lines:?}");
        version_action(&mut app, Act::Delete, delete(2));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Text { lines, .. }) = app.modal.take() else { panic!("no refusal") };
        assert!(
            lines.contains(&"  follower (follower): follows the repository; this is its newest published version".to_string()),
            "{lines:?}"
        );

        // v3 has no publication and nothing pins it: Pulp deletes it.
        version_action(&mut app, Act::Delete, delete(3));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, typed, .. }) = app.modal.take() else { panic!("no confirm") };
        assert_eq!(typed.as_deref(), Some("r@v3"));
        assert_eq!(lines[0], "Delete repository version 3 (r@v3) and its 0 publication(s).");
        assert!(!lines.iter().any(|l| l.contains("stop serving") || l.contains("switch")), "{lines:?}");
    }

    #[tokio::test]
    async fn sync_form_shows_the_default_remote() {
        use crate::tui::app::Modal;
        let server = wiremock::MockServer::start().await;
        let remote = format!("{API}remotes/rpm/rpm/1/");
        get(&server, &remote, json!({ "name": "epel" })).await;
        let (mut app, mut rx) = connected(&server).await;
        let repo = json!({ "pulp_href": format!("{API}repositories/rpm/rpm/r/"), "name": "os", "remote": remote });
        repo_action(&mut app, RepoType::Rpm, Act::Sync, repo);
        settle(&mut app, &mut rx).await;
        let Some(Modal::Form { form, .. }) = app.modal.take() else { panic!("no form") };
        assert_eq!(form.get("remote"), "epel");
        // Without a default remote the form opens at once, empty.
        repo_action(&mut app, RepoType::Rpm, Act::Sync, json!({ "pulp_href": "/r/", "name": "x", "remote": null }));
        let Some(Modal::Form { form, .. }) = app.modal.take() else { panic!("no form") };
        assert_eq!(form.get("remote"), "");
    }

    #[tokio::test]
    async fn repo_edit_shows_and_clears_the_default_remote() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{body_json, method, path};
        let server = wiremock::MockServer::start().await;
        let (remote, repo) = (format!("{API}remotes/rpm/rpm/1/"), format!("{API}repositories/rpm/rpm/r/"));
        get(&server, &remote, json!({ "name": "epel" })).await;
        wiremock::Mock::given(method("PATCH"))
            .and(path(repo.clone()))
            .and(body_json(json!({ "remote": null })))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({ "pulp_href": repo })))
            .expect(1)
            .mount(&server)
            .await;
        let (mut app, mut rx) = connected(&server).await;
        repo_action(&mut app, RepoType::Rpm, Act::Edit, json!({ "pulp_href": repo, "name": "os", "remote": remote }));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
        assert_eq!(form.get("remote"), "epel");
        form.fields.iter_mut().find(|f| f.key == "remote").unwrap().value.clear();
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        assert!(app.notices.iter().any(|n| n.text == "update repository os: done"), "{:?}", app.notices.back().map(|n| &n.text));
    }

    #[tokio::test]
    async fn binding_edit_of_a_version_bound_distribution_clears_the_version() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{body_json, method, path};
        let server = wiremock::MockServer::start().await;
        let d = format!("{API}distributions/file/file/1/");
        wiremock::Mock::given(method("PATCH"))
            .and(path(d.clone()))
            .and(body_json(json!({ "publication": null, "repository": null, "repository_version": null })))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({ "pulp_href": d })))
            .expect(1)
            .mount(&server)
            .await;
        let (mut app, mut rx) = connected(&server).await;
        let item = json!({ "pulp_href": d, "name": "replica", "base_path": "replica",
            "repository_version": format!("{API}repositories/file/file/r/versions/1/"), "pulp_labels": {} });
        distribution_action(&mut app, RepoType::File, Act::Edit, item);
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no edit form") };
        form.fields.iter_mut().find(|f| f.key == "mode").unwrap().value = "none".into();
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        assert!(app.notices.iter().any(|n| n.text == "update distribution replica: done"));
    }

    #[tokio::test]
    async fn content_view_distributions_change_only_through_the_view() {
        use crate::tui::app::Modal;
        let server = wiremock::MockServer::start().await;
        let (mut app, _rx) = connected(&server).await;
        let item = json!({ "pulp_href": format!("{API}distributions/rpm/rpm/1/"), "name": "web-os", "base_path": "dev/web/os",
            "publication": "/p/", "pulp_labels": { crate::cv::LABEL_CV: "web", crate::cv::LABEL_ENV: "dev" } });
        distribution_action(&mut app, RepoType::Rpm, Act::Edit, item.clone());
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no edit form") };
        assert!(form.notes.iter().any(|n| n.starts_with("Content View web owns this distribution")));
        form.fields.iter_mut().find(|f| f.key == "mode").unwrap().value = "none".into();
        let e = submit(&mut app, &form).unwrap_err();
        assert!(e.starts_with("refused: Content View web owns this distribution"), "{e}");
        form.fields.iter_mut().find(|f| f.key == "mode").unwrap().value = "publication of a repository version".into();
        let keep = "refused: Content View web finds this distribution by its name and pulp_tui labels; keep them";
        for (key, value) in [("name", "renamed"), ("labels", "pulp_tui_cv=web"), ("labels", "pulp_tui_cv=other,pulp_tui_env=dev")] {
            let mut edited = form.clone();
            edited.fields.iter_mut().find(|f| f.key == key).unwrap().value = value.into();
            assert_eq!(submit(&mut app, &edited).unwrap_err(), keep, "{key}={value}");
        }
        distribution_action(&mut app, RepoType::Rpm, Act::Delete, item);
        let Some(Modal::Confirm { lines, .. }) = &app.modal else { panic!("no confirm") };
        assert!(lines.iter().any(|l| l.starts_with("WARNING: Content View web serves environment dev here")), "{lines:?}");
    }

    #[tokio::test]
    async fn a_repository_binding_serves_its_newest_publication() {
        let server = wiremock::MockServer::start().await;
        let repo = format!("{API}repositories/rpm/rpm/r/");
        // v3 was synced without a publication: Pulp serves the publication of v2.
        let pubs = page(json!([
            { "pulp_href": format!("{API}publications/rpm/rpm/old/"), "repository_version": format!("{repo}versions/1/"), "pulp_created": "2026-01-02T00:00:00Z" },
            { "pulp_href": format!("{API}publications/rpm/rpm/new/"), "repository_version": format!("{repo}versions/2/"),
              "pulp_created": "2026-01-01T00:00:00Z", "layout": "flat" },
        ]));
        get(&server, &format!("{API}publications/rpm/rpm/"), pubs).await;
        let (app, _rx) = connected(&server).await;
        let c = &app.conn.as_ref().unwrap().client;
        let served = served_now(c, RepoType::Rpm, &json!({ "repository": repo })).await.unwrap().unwrap();
        assert_eq!((served.0, s(&served.1, "layout").as_str()), (format!("{repo}versions/2/"), "flat"));
        assert_eq!(served_now(c, RepoType::Rpm, &json!({ "name": "unbound" })).await.unwrap(), None);
    }

    #[tokio::test]
    async fn inspect_masks_remote_tokens() {
        use crate::tui::app::Modal;
        let server = wiremock::MockServer::start().await;
        let href = format!("{API}remotes/rpm/rpm/1/");
        let remote =
            json!({ "pulp_href": href, "name": "r", "url": "u", "headers": [{ "Authorization": "Bearer abc" }], "sles_auth_token": "tok" });
        get(&server, &href, remote.clone()).await;
        let (mut app, mut rx) = connected(&server).await;
        inspect(&mut app, &Kind::Remotes(RepoType::Rpm), remote.clone());
        settle(&mut app, &mut rx).await;
        let Some(Modal::Text { lines, .. }) = &app.modal else { panic!("no text") };
        assert!(!lines.iter().any(|l| l.contains("abc") || l.contains("\"tok\"")) && lines.iter().any(|l| l.contains("•••")), "{lines:?}");
        // A row without an href is shown without a GET: it is masked too.
        app.modal = None;
        inspect(
            &mut app,
            &Kind::Remotes(RepoType::Rpm),
            json!({ "name": "r", "headers": [{ "Authorization": "Bearer abc" }], "sles_auth_token": "tok" }),
        );
        let Some(Modal::Text { lines, .. }) = &app.modal else { panic!("no text") };
        assert!(!lines.iter().any(|l| l.contains("abc") || l.contains("\"tok\"")), "{lines:?}");
    }

    #[tokio::test]
    async fn remote_edit_clears_write_only_fields_with_a_dash() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{body_json, method, path};
        let server = wiremock::MockServer::start().await;
        let href = format!("{API}remotes/rpm/rpm/1/");
        // `-` sends null (on a file field too, it is not a path), empty is not sent, a value is sent.
        wiremock::Mock::given(method("PATCH"))
            .and(path(href.clone()))
            .and(body_json(json!({ "username": null, "ca_cert": null, "sles_auth_token": null, "proxy_username": "bob" })))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({ "pulp_href": href })))
            .expect(1)
            .mount(&server)
            .await;
        let (mut app, mut rx) = connected(&server).await;
        remote_action(
            &mut app,
            RepoType::Rpm,
            Act::Edit,
            json!({ "pulp_href": href, "name": "r", "url": "https://h/", "policy": "immediate" }),
        );
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no edit form") };
        for k in ["ca_cert", "client_key", "username", "password", "proxy_password", "sles_auth_token"] {
            assert_eq!(form.fields.iter().find(|f| f.key == k).unwrap().help, "empty = unchanged, - = clear", "{k}");
        }
        for (k, v) in [("username", "-"), ("ca_cert", "-"), ("sles_auth_token", "-"), ("proxy_username", "bob")] {
            form.fields.iter_mut().find(|f| f.key == k).unwrap().value = v.into();
        }
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        assert!(app.notices.iter().any(|n| n.text == "update remote r: done"), "{:?}", app.notices.back().map(|n| &n.text));
    }

    #[tokio::test]
    async fn snapshot_repositories_refuse_edit_and_create_is_not_affected() {
        use crate::tui::app::Modal;
        let server = wiremock::MockServer::start().await;
        let (mut app, _rx) = connected(&server).await;
        let item = json!({ "pulp_href": format!("{API}repositories/rpm/rpm/1/"), "name": "pulp-tui-cv-web-os", "pulp_labels": { "pulp_tui_cv": "web" } });
        repo_action(&mut app, RepoType::Rpm, Act::Edit, item.clone());
        assert!(app.modal.is_none(), "Edit opened a form");
        assert!(app.notices.back().is_some_and(|n| n.text.starts_with("refused:")), "{:?}", app.notices.back().map(|n| &n.text));
        // Create starts from the selected row, but makes a new repository without the label.
        repo_action(&mut app, RepoType::Rpm, Act::Create, item);
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
        form.fields.iter_mut().find(|f| f.key == "name").unwrap().value = "new".into();
        form.fields.iter_mut().find(|f| f.key == "retain_repo_versions").unwrap().value = "1".into();
        assert_eq!(submit(&mut app, &form), Ok(()));
    }

    #[tokio::test]
    async fn compare_refuses_versions_of_another_type() {
        let server = wiremock::MockServer::start().await;
        let (mut app, _rx) = connected(&server).await;
        app.list.marked = [format!("{API}repositories/file/file/f/versions/1/")].into();
        version_action(&mut app, Act::Compare, json!({ "pulp_href": format!("{API}repositories/rpm/rpm/r/versions/2/"), "number": 2 }));
        assert!(app.jobs.is_empty(), "a compare job started");
        assert_eq!(app.notices.back().map(|n| n.text.as_str()), Some("compare needs two versions of one repository type"));
    }

    #[tokio::test]
    async fn follow_mode_refuses_another_base_path() {
        use crate::tui::app::Modal;
        let server = wiremock::MockServer::start().await;
        let d = json!({ "pulp_href": format!("{API}distributions/rpm/rpm/1/"), "name": "r", "base_path": "old", "pulp_labels": {} });
        get(&server, &format!("{API}distributions/rpm/rpm/"), page(json!([d]))).await;
        let (mut app, mut rx) = connected(&server).await;
        let repo = json!({ "pulp_href": format!("{API}repositories/rpm/rpm/r/"), "name": "r", "latest_version_href": "/v/" });
        repo_action(&mut app, RepoType::Rpm, Act::Distribute, repo);
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
        form.fields.iter_mut().find(|f| f.key == "mode").unwrap().value = "follow repository latest".into();
        form.fields.iter_mut().find(|f| f.key == "base_path").unwrap().value = "new".into();
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        let e = app.notices.back().map(|n| n.text.clone()).unwrap_or_default();
        assert!(e.contains("serves old instead of new"), "{e}");
        let sent = server.received_requests().await.unwrap();
        assert!(!sent.iter().any(|r| r.method.as_str() == "PATCH"), "a PATCH was sent");
    }
}
