//! Actions behind the menus: forms, previews, confirmations and background jobs.
//! Content View and administration actions live in `actions_cv` / `actions_admin`.

use super::app::{App, Apply, Ctx, Level, Screen, item_key};
use super::form::{Field, FieldKind, Form, Source, format_labels, parse_labels};
use super::kinds::{Act, Kind, s, short};
use crate::config::{AuthMethod, Database, LOCAL_DATABASE, Profile, database_keyring_user};
use crate::filters::{Selector, filter_help, parse_rules};
use crate::pulp::client::Page;
use crate::pulp::models::{Distribution, Publication, RepoVersion, TASK_FINAL_STATES, Task, TaskGroup};
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

/// Report what a mutation returned, as `submit` does. The Task Center polls a task group as one task.
fn submitted(app: &mut App, m: Mutation, label: &str) {
    match m {
        Mutation::Task(h) | Mutation::TaskGroup(h) => {
            app.track_task(&h, label);
            app.notify(Level::Info, format!("submitted: {label} (see Tasks)"));
        }
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

/// What a distribution serves: (repository version, its publication, how). `None`: nothing, also for a
/// version binding without a publication (File, RPM and Deb serve only publications).
/// A repository binding serves the newest publication of the repository (pulpcore orders by repository
/// version, then creation), which is not the latest version when that one has no publication.
async fn served_now(c: &PulpClient, caps: &Capabilities, t: RepoType, d: &Value) -> anyhow::Result<Option<(String, Value, String)>> {
    let Some(p) = ops::served_publication(c, caps, t, d).await? else { return Ok(None) };
    let how = match (d["publication"].as_str(), d["repository_version"].as_str()) {
        (Some(x), _) => format!("pinned to publication {}", short(x)),
        (None, Some(_)) => "pinned to repository version".into(),
        (None, None) => format!("follows repository {}: newest publication {}", short(&s(d, "repository")), short(&s(&p, "pulp_href"))),
    };
    Ok(Some((s(&p, "repository_version"), p, how)))
}

pub(super) fn caps(app: &App) -> Option<std::sync::Arc<Capabilities>> {
    app.conn.as_ref().map(|c| c.caps.clone())
}

/// The connected server lacks `f`: report "not supported by this server: ..." and return true.
pub(super) fn unsupported(app: &mut App, f: Feature) -> bool {
    let Some(Err(e)) = caps(app).map(|c| c.require(f)) else { return false };
    app.error(e.to_string());
    true
}

/// The sync policies of a repository type: for RPM, the enum of the server's `sync_policy` property, if it has one.
pub fn sync_policies(caps: Option<&Capabilities>, t: RepoType) -> Vec<String> {
    match t {
        RepoType::Rpm => caps
            .and_then(|c| c.field_enum("RpmRepositorySyncURL", "sync_policy").map(|e| e.to_vec()))
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
        (Kind::Repos(..) | Kind::Remotes(_) | Kind::Distributions(_), Act::Delete) if !app.list.marked.is_empty() => {
            delete_marked(app, kind)
        }
        (Kind::Logs { .. }, Act::Inspect) => app.text(s(&item, "summary"), log_lines(&item)),
        (_, Act::Inspect) => inspect(app, &kind, item),
        (Kind::Profiles, _) => profile_action(app, act, item),
        (Kind::Repos(t, _), _) => repo_action(app, *t, act, item),
        (Kind::Remotes(t), _) => remote_action(app, *t, act, item),
        (Kind::Acs(t), _) => acs_action(app, *t, act, item),
        (Kind::Versions(_), Act::PruneOrphans) => prune_orphans(app),
        (Kind::Versions(_), _) => version_action(app, act, item),
        (Kind::Content { version, .. }, _) => content_action(app, version.clone(), act),
        (Kind::Publications(t), _) => publication_action(app, *t, act, item),
        (Kind::Distributions(t), _) => distribution_action(app, *t, act, item),
        (Kind::Tasks | Kind::ServerTasks(_) | Kind::TaskGroups, _) => task_action(app, act, item),
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
        // The same functions and file as `export-repos` / `import-repos`; the export never holds a secret.
        Act::ExportRepos => {
            let profile = if name.is_empty() { app.profile.clone().unwrap_or_default() } else { name };
            let Some(p) = app.cfg.profiles.get(&profile).cloned() else { return };
            let settings = app.cfg.settings.clone();
            let title = format!("Export repository definitions of {profile} to file");
            app.input(title, &format!("pulp-tui-{profile}-repos.json"), move |app, path| {
                let fut = async move {
                    let c = PulpClient::new(&profile, &p, &settings)?;
                    let (ex, notes) = crate::repo_export::export(&c, &Capabilities::detect(&c).await?).await?;
                    tokio::fs::write(&path, serde_json::to_string_pretty(&ex)?).await.with_context(|| format!("write {path}"))?;
                    ap(move |app| {
                        let (r, n) = (ex.remotes.len(), ex.repositories.len());
                        app.ok(format!("exported {r} remote(s) and {n} repositories of {profile} to {path} (no secrets)"));
                        if !notes.is_empty() {
                            app.text(format!("Export of {profile}"), notes.iter().map(|n| format!("note: {n}")).collect());
                        }
                    })
                };
                app.spawn("export repositories".into(), false, fut);
            });
        }
        Act::ImportRepos => {
            let profile = app.profile.clone().unwrap_or_default();
            app.input(format!("Import repository definitions into {profile} from file"), "", move |app, path| {
                app.job(format!("plan import of {path}"), move |ctx| async move {
                    let read = tokio::fs::read(&path).await.with_context(|| format!("read {path}"))?;
                    let ex: crate::repo_export::ServerExport = serde_json::from_slice(&read).with_context(|| format!("parse {path}"))?;
                    let (create, keep, notes) = crate::repo_export::plan(&ctx.client, &ctx.caps, &ex).await?;
                    let head = format!("Import {path} into {profile}: create {create}, keep {keep} existing (remotes and repositories).");
                    let lines = [vec![head, "Existing objects are never changed.".into(), String::new()], notes].concat();
                    let title = format!("Import repository definitions into {profile}");
                    ap(move |app| {
                        if create == 0 {
                            return app.text(title, lines);
                        }
                        app.confirm(title, lines, move |app| {
                            app.job(format!("import {path}"), move |ctx| async move {
                                let (created, kept, notes) = crate::repo_export::import(&ctx.client, &ctx.caps, &ex).await?;
                                ap(move |app| {
                                    let done =
                                        format!("profile {profile}: created {created}, kept {kept} existing (remotes and repositories)");
                                    app.ok(done.clone());
                                    if !notes.is_empty() {
                                        let lines = [vec![done], notes.iter().map(|n| format!("warning: {n}")).collect()].concat();
                                        app.text(format!("Import into {profile}"), lines);
                                    }
                                })
                            })
                        })
                    })
                });
            });
        }
        _ => {}
    }
}

// ---- repositories --------------------------------------------------------------------

/// Empty (the server default) and the values of the enum that property `k` of `schema` refers to, without the
/// checksums Pulp refuses to publish. Through the `$ref`: generated enum names (`LayoutD5aEnum`) can change.
fn enum_choices(caps: Option<&Capabilities>, schema: &str, k: &str) -> Vec<String> {
    let e = caps.and_then(|c| c.field_enum(schema, k)).unwrap_or_default();
    let usable = e.iter().filter(|x| !matches!(x.as_str(), "unknown" | "md5" | "sha1")).cloned();
    std::iter::once(String::new()).chain(usable).collect()
}

/// Repository form fields that only some plugins have.
fn repo_extra_fields(t: RepoType, caps: Option<&Capabilities>, v: &Value) -> Vec<Field> {
    let has = |f: &str| caps.is_none_or(|c| c.has_field(t.repository_schema(), f));
    let mut fields = Vec::new();
    match t {
        RepoType::Rpm => {
            let choices = |k: &str| enum_choices(caps, t.repository_schema(), k);
            if has("retain_package_versions") {
                fields.push(
                    Field::new("retain_package_versions", "Retain package versions", FieldKind::Number)
                        .value(s(v, "retain_package_versions"))
                        .help("0 = keep all"),
                );
            }
            for (k, label) in
                [("checksum_type", "Checksum type"), ("compression_type", "Metadata compression"), ("layout", "Package layout")]
            {
                if has(k) {
                    fields.push(Field::new(k, label, FieldKind::Choice(choices(k))).value(s(v, k)).help("empty = server default"));
                }
            }
            if has("repo_config") {
                let shown = v["repo_config"].as_object().filter(|m| !m.is_empty()).map(|_| v["repo_config"].to_string());
                let help = r#"the generated .repo file, e.g. {"gpgcheck": 1, "repo_gpgcheck": 1}"#;
                fields.push(Field::text("repo_config", "Repo config (JSON)").value(shown.unwrap_or_default()).help(help));
            }
            if has("metadata_signing_service") {
                fields.push(
                    Field::pick("metadata_signing_service", "Metadata signing service", Source::SigningServices)
                        .value(s(v, "metadata_signing_service")),
                );
            }
            if has("package_signing_service") {
                let f = Field::pick("package_signing_service", "Package signing service", Source::SigningServices);
                fields.push(f.value(s(v, "package_signing_service")));
            }
            if has("package_signing_fingerprint") {
                let f = Field::text("package_signing_fingerprint", "Package signing fingerprint");
                fields.push(f.value(s(v, "package_signing_fingerprint")).help("v4:<hex fingerprint> or keyid:<16 hex>"));
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
        RepoType::Deb => fields.extend(deb_repo_fields(&has, v)),
    }
    if has("retain_checkpoints") {
        let f = Field::new("retain_checkpoints", "Retain checkpoints", FieldKind::Number);
        fields.push(f.value(s(v, "retain_checkpoints")).help("checkpoint publications to keep; empty = all"));
    }
    fields
}

/// The Deb signing settings of repository `v`, each when the server has the field (`has`).
fn deb_repo_fields(has: &dyn Fn(&str) -> bool, v: &Value) -> Vec<Field> {
    let pick = |k: &str, label: &str| Field::pick(k, label, Source::SigningServices).value(s(v, k));
    let mut fields = Vec::new();
    if has("signing_service") {
        fields.push(pick("signing_service", "Signing service (Release)"));
    }
    if has("signing_service_release_overrides") {
        let f = Field::text("signing_service_release_overrides", "Per-release signing services");
        let help = "release=signing service href,...; a release you remove loses its override";
        fields.push(f.value(format_labels(&v["signing_service_release_overrides"])).help(help));
    }
    if has("package_signing_service") {
        fields.push(pick("package_signing_service", "Package signing service"));
    }
    if has("package_signing_fingerprint") {
        let f = Field::text("package_signing_fingerprint", "Package signing fingerprint");
        fields.push(f.value(s(v, "package_signing_fingerprint")).help("v4:<hex fingerprint> or keyid:<16 hex>"));
    }
    if has("package_signing_fingerprint_release_overrides") {
        let f = Field::text("package_signing_fingerprint_release_overrides", "Per-release package fingerprints");
        let help = "release=fingerprint,...; a release you remove loses its override";
        fields.push(f.value(format_labels(&v["package_signing_fingerprint_release_overrides"])).help(help));
    }
    if has("publish_upstream_release_fields") {
        let on = if v["publish_upstream_release_fields"] == false { "false" } else { "true" };
        fields.push(Field::new("publish_upstream_release_fields", "Publish upstream Release fields", FieldKind::Bool).value(on));
    }
    if has("excluded_package_metadata_fields") {
        let names: Vec<&str> = v["excluded_package_metadata_fields"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let f = Field::text("excluded_package_metadata_fields", "Excluded package metadata fields");
        fields.push(f.value(names.join(",")).help("field,...: left out of the Packages indices (publication default)"));
    }
    fields
}

/// `release=value,...` as Pulp's per-release overrides. pulp_deb changes only the releases that a request
/// names, so each release of `old` (the value before the edit) that `new` lacks is sent as `removed` (deleted).
fn release_overrides(new: &str, old: &str, removed: Value) -> Result<Value, String> {
    let mut out = parse_labels(old)?;
    let m = out.as_object_mut().expect("parse_labels makes an object");
    m.values_mut().for_each(|v| *v = removed.clone());
    m.extend(parse_labels(new)?.as_object().cloned().unwrap_or_default());
    Ok(out)
}

/// `v` as a JSON object; empty is the empty object.
fn json_object(v: &str) -> Result<Value, String> {
    let o: Value = if v.is_empty() { json!({}) } else { serde_json::from_str(v).map_err(|e| format!("invalid JSON: {e}"))? };
    if o.is_object() { Ok(o) } else { Err(r#"must be a JSON object, e.g. {"gpgcheck": 1}"#.into()) }
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
            ("signing_service_release_overrides", _) if !v.is_empty() || edit => {
                release_overrides(v, &fld.initial, Value::Null).map_err(|e| format!("{}: {e}", fld.label))?
            }
            // pulp_deb deletes a fingerprint override sent as "": the value is not nullable.
            ("package_signing_fingerprint_release_overrides", _) if !v.is_empty() || edit => {
                release_overrides(v, &fld.initial, "".into()).map_err(|e| format!("{}: {e}", fld.label))?
            }
            ("excluded_package_metadata_fields", _) if !v.is_empty() || edit => {
                v.split(',').map(str::trim).filter(|x| !x.is_empty()).collect::<Vec<_>>().into()
            }
            ("repo_config", _) if !v.is_empty() || edit => json_object(v).map_err(|e| format!("{}: {e}", fld.label))?,
            (_, FieldKind::Bool) => Value::Bool(v == "true"),
            (_, FieldKind::Number) if v.is_empty() && edit && matches!(fld.key.as_str(), "retain_repo_versions" | "retain_checkpoints") => {
                Value::Null
            }
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

/// Sync options, each shown when the server's sync request has it. They start at the server defaults.
fn sync_fields(t: RepoType, caps: Option<&Capabilities>) -> Vec<Field> {
    let schema = match t {
        RepoType::Rpm => "RpmRepositorySyncURL",
        RepoType::Deb => "AptRepositorySyncURL",
        RepoType::File => "FileRepositorySyncURL",
    };
    let has = |f: &str| caps.is_none_or(|c| c.has_field(schema, f));
    let mut fields = vec![];
    if has("optimize") {
        let help = "off = full re-sync, also when the upstream metadata did not change";
        fields.push(Field::new("optimize", "Optimize", FieldKind::Bool).value("true").help(help));
    }
    if t == RepoType::Rpm && has("skip_types") {
        let types = caps.and_then(|c| c.field_enum(schema, "skip_types")).map(<[String]>::to_vec);
        for ty in types.unwrap_or_else(|| vec!["srpm".into(), "treeinfo".into()]) {
            fields.push(Field::new(&format!("skip_{ty}"), &format!("Skip {ty}"), FieldKind::Bool));
        }
    }
    fields
}

/// The sync options the operator changed: `optimize` turned off, the checked `skip_types`.
fn sync_options(f: &Form) -> Value {
    let mut b = json!({});
    if f.changed("optimize") {
        b["optimize"] = f.bool("optimize").into();
    }
    let skip: Vec<&str> = f.fields.iter().filter(|x| x.value == "true").filter_map(|x| x.key.strip_prefix("skip_")).collect();
    if !skip.is_empty() {
        b["skip_types"] = skip.into();
    }
    b
}

fn repo_sync(app: &mut App, t: RepoType, href: String, name: String, item: &Value) {
    let caps = caps(app);
    let policies = sync_policies(caps.as_deref(), t);
    let default = item["remote"].as_str().map(str::to_string);
    let note = if default.is_some() { "repository has a default remote" } else { "repository has NO default remote: pick one" };
    let label = format!("remote of {name}");
    let (autopublish, retain) = (item["autopublish"] == true, item["retain_package_versions"].as_u64().unwrap_or(0));
    let open = move |app: &mut App, current: String| {
        let mut fields = vec![
            Field::pick("remote", "Remote", Source::Remotes(t)).help("empty = the repository's default remote").value(current),
            Field::new("policy", "Sync policy", FieldKind::Choice(policies)),
        ];
        fields.extend(sync_fields(t, caps.as_deref()));
        app.form(Form::new(format!("Sync {name}"), fields).note(note), move |app, f| {
            let (remote, policy) = (f.opt("remote").map(str::to_string), f.get("policy").to_string());
            let (href, name, options) = (href.clone(), name.clone(), sync_options(f));
            // pulp_rpm sync() answers HTTP 400 to these pairs; say it before any request.
            if policy == "mirror_complete" && options.get("skip_types").is_some() {
                return Err("Pulp refuses skip types with mirror_complete: clear the Skip fields or choose another sync policy".into());
            }
            if policy == "mirror_complete" && autopublish {
                return Err(
                    "Pulp refuses mirror_complete with autopublish: turn autopublish off (Edit) or choose another sync policy".into()
                );
            }
            if matches!(policy.as_str(), "mirror_complete" | "mirror_content_only") && retain > 0 {
                return Err(
                    "Pulp refuses mirror policies with retain_package_versions above 0: set it to 0 (Edit) or choose additive".into()
                );
            }
            let go = move |app: &mut App| {
                submit(app, format!("sync {name} ({policy})"), move |c| async move {
                    let r = match remote {
                        Some(r) => Some(href_by_name(&c, t.remotes(), &r).await?),
                        None => None,
                    };
                    Ok(ops::sync_with(&c, t, &href, r.as_deref(), &policy, options).await?)
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

/// Confirmation lines for deleting repository `href`. Refuses one with Content View snapshots.
async fn repo_delete_impact(ctx: &Ctx, t: RepoType, href: &str, name: &str) -> anyhow::Result<Vec<String>> {
    let c = &ctx.client;
    // Fails closed: without the Content View store the snapshot check cannot run.
    let st = ctx.cv()?.store;
    let protected = st.protected_versions(&ctx.scope).await?.iter().filter(|v| v.starts_with(href)).count();
    if protected > 0 {
        bail!("refused: {protected} version(s) of {name} are Content View snapshots; delete those Content View versions first");
    }
    let mut sources = vec![];
    for cv in st.content_views(&ctx.scope).await? {
        if st.cv_repos(cv.id).await?.iter().any(|r| r.repo_href == href) {
            sources.push(cv.name);
        }
    }
    let pubs: Vec<Publication> = c.list_all(&c.api(t.publications()), &[("repository", href.to_string())], 100_000).await?;
    let versions: Page<Value> = c.list(&format!("{href}versions/"), &[("limit", "1".into())]).await?;
    let users = served_by(&ops::distributions(c, t).await?, |d| {
        d.repository.as_deref() == Some(href)
            || d.repository_version.as_ref().is_some_and(|v| v.starts_with(href))
            || pubs.iter().any(|p| d.publication.as_ref() == Some(&p.pulp_href))
    });
    let mut lines = vec![format!("Deletes repository {name} with all {} versions and {} publications.", versions.count, pubs.len())];
    if users.is_empty() {
        lines.push("No distribution serves it.".into());
    } else {
        lines.push(format!("WARNING: {} distribution(s) serve it and stop serving:", users.len()));
        lines.extend(users);
    }
    if !sources.is_empty() {
        lines.push(format!("WARNING: Content View(s) {} use it as a source; their next publish fails.", sources.join(", ")));
    }
    Ok(lines)
}

async fn remote_delete_impact(c: &PulpClient, t: RepoType, href: &str, name: &str) -> anyhow::Result<Vec<String>> {
    let users: Page<Value> = c.list(&c.api(t.repositories()), &[("remote", href.to_string()), ("limit", "100".into())]).await?;
    Ok(vec![
        format!("Delete remote {name}. Repository content and versions are not affected."),
        format!(
            "{} repository(ies) use it as default remote: {}",
            users.count,
            users.results.iter().map(|r| s(r, "name")).collect::<Vec<_>>().join(", ")
        ),
    ])
}

fn distribution_delete_lines(item: &Value) -> Vec<String> {
    let mut lines =
        vec![format!("Delete distribution {}.", s(item, "name")), format!("{} will stop serving content.", s(item, "base_url"))];
    if let Some(cv) = cv_owner(item) {
        let env = item["pulp_labels"].get(crate::cv::LABEL_ENV).and_then(Value::as_str).unwrap_or("?");
        lines.push(format!("WARNING: Content View {cv} serves environment {env} here; the view still records this binding."));
    }
    lines
}

/// Delete the marked repositories, remotes or distributions: one impact report, one typed confirmation.
fn delete_marked(app: &mut App, kind: Kind) {
    let (t, what) = match kind {
        Kind::Repos(t, _) => (t, "repositories"),
        Kind::Remotes(t) => (t, "remotes"),
        Kind::Distributions(t) => (t, "distributions"),
        _ => return,
    };
    let hrefs: Vec<String> = app.list.marked.iter().cloned().collect();
    let n = hrefs.len();
    app.job(format!("impact of deleting {n} {what}"), move |ctx| async move {
        let c = &ctx.client;
        let mut lines = vec![format!("Deletes {n} {what}:"), String::new()];
        let mut targets = vec![];
        for href in hrefs {
            // Marks can be on other pages: fetch each object.
            let item: Value = c.get(&href, &[]).await?;
            let name = s(&item, "name");
            lines.extend(match kind {
                Kind::Repos(..) => repo_delete_impact(&ctx, t, &href, &name).await?,
                Kind::Remotes(_) => remote_delete_impact(c, t, &href, &name).await?,
                _ => distribution_delete_lines(&item),
            });
            lines.push(String::new());
            targets.push((href, name));
        }
        ap(move |app| {
            let typed = format!("delete {n} {what}");
            app.confirm_danger(format!("Delete {n} {what}"), lines, &typed.clone(), move |app| {
                app.job(typed, move |ctx| async move {
                    let (mut done, mut failed) = (vec![], vec![]);
                    for (href, name) in targets {
                        // One failure does not stop the others; each is reported.
                        match ctx.client.delete(&href).await {
                            Ok(m) => done.push((href, m, format!("delete {name}"))),
                            Err(e) => failed.push(format!("{name}: {e}")),
                        }
                    }
                    ap(move |app| {
                        for (href, m, label) in done {
                            app.list.marked.remove(&href);
                            submitted(app, m, &label);
                        }
                        if !failed.is_empty() {
                            app.error(format!("delete failed (still marked): {}", failed.join("; ")));
                        }
                    })
                })
            });
        })
    });
}

fn repo_action(app: &mut App, t: RepoType, act: Act, item: Value) {
    let href = s(&item, "pulp_href");
    let name = s(&item, "name");
    if let Some(e) = snapshot_refusal(&item)
        && matches!(act, Act::Sync | Act::Upload | Act::UploadComps | Act::UploadAdvisory | Act::CopyContent | Act::Reclaim | Act::Edit)
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
                let lines = repo_delete_impact(&ctx, t, &href, &name).await?;
                ap(move |app| {
                    app.confirm_danger(format!("Delete repository {name}"), lines, &name.clone(), move |app| {
                        submit(app, format!("delete repository {name}"), move |c| async move { Ok(c.delete(&href).await?) })
                    });
                })
            });
        }
        Act::Sync => repo_sync(app, t, href, name, &item),
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
        Act::Reclaim => reclaim(app, href),
        Act::UploadComps | Act::UploadAdvisory => {
            let comps = act == Act::UploadComps;
            let (what, ep) = if comps { ("comps.xml", rpm::COMPS) } else { ("advisory", rpm::ADVISORIES) };
            if caps(app).is_some_and(|c| !c.has_operation(ep, "post")) {
                return app.error(format!("not supported by this server: {what} upload ({ep})"));
            }
            let mut fields = vec![Field::text("path", "Local file path").required()];
            if comps {
                fields.push(Field::new("replace", "Replace", FieldKind::Bool).help("on = first remove the comps units the repository has"));
            }
            let note = if comps {
                "Adds the package groups, categories, environments and langpacks of the file."
            } else {
                "One advisory as JSON (updateinfo fields): id, issued and updated are required, and each package needs sum_type."
            };
            app.form(Form::new(format!("Upload {what} to {name}"), fields).note(note), move |app, f| {
                let path = std::path::PathBuf::from(f.get("path"));
                if !path.is_file() {
                    return Err(format!("{} is not a readable file", path.display()));
                }
                let (href, replace, label) = (href.clone(), f.bool("replace"), format!("upload {what} to {name}"));
                app.job(label.clone(), move |ctx| async move {
                    let c = &ctx.client;
                    let m = if comps {
                        rpm::upload_comps(c, &path, &href, replace).await?
                    } else {
                        rpm::upload_advisory(c, &path, &href).await?
                    };
                    ap(move |app| follow(app, m, label.clone(), move |app, t| new_version(app, &label, &t)))
                });
                Ok(())
            });
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

/// Reclaim the disk space of `repo`: ask which versions keep their files, load the impact (its versions, its remote
/// and the remote's policy), then the confirm typed with the repository name.
fn reclaim(app: &mut App, repo: String) {
    if caps(app).is_some_and(|c| !c.has_endpoint(ops::RECLAIM_SPACE)) {
        return app.error(format!("not supported by this server: reclaim space ({})", ops::RECLAIM_SPACE));
    }
    let keep = Field::text("keep", "Keep versions").help("numbers, e.g. 3,4: these versions keep their files; empty = none");
    app.form(Form::new("Reclaim disk space", vec![keep]), move |app, f| {
        let keep: Vec<u64> = f
            .get("keep")
            .split([',', ' '])
            .filter(|n| !n.is_empty())
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|_| "Keep versions: version numbers, e.g. 3,4".to_string())?;
        let repo = repo.clone();
        app.job("impact of reclaiming space", move |ctx| async move {
            let c = &ctx.client;
            let r: Value = c.get(&repo, &[]).await?;
            if let Some(e) = snapshot_refusal(&r) {
                bail!(e);
            }
            let name = s(&r, "name");
            let versions: Vec<Value> = c.list_all(&format!("{repo}versions/"), &[("fields", "number".into())], 100_000).await?;
            let numbers: Vec<u64> = versions.iter().filter_map(|v| v["number"].as_u64()).collect();
            if let Some(n) = keep.iter().find(|n| !numbers.contains(n)) {
                bail!("refused: {name} has no version {n}");
            }
            let remote = match r["remote"].as_str() {
                Some(h) => {
                    let v: Value = c.get(h, &[("fields", "name,policy".into())]).await?;
                    format!("Default remote: {} (policy {}).", s(&v, "name"), s(&v, "policy"))
                }
                None => "No default remote: a file comes again from the remote of the sync that added it.".into(),
            };
            let kept = match keep.as_slice() {
                [] => "No version keeps its files.".to_string(),
                k => format!("Versions that keep their files: {}", k.iter().map(u64::to_string).collect::<Vec<_>>().join(", ")),
            };
            let lines = vec![
                format!("Reclaim disk space of {name}: Pulp deletes the stored files of its synced content that no other repository has."),
                format!("The content stays in all {} versions: a client request downloads a file again from its remote", numbers.len()),
                "(on demand), so the upstream must still serve it. Uploaded content (no remote) keeps its files.".into(),
                remote,
                kept,
            ];
            let keep: Vec<String> = keep.iter().map(|n| version_of(&repo, *n as i64)).collect();
            ap(move |app| {
                app.confirm_danger(format!("Reclaim space of {name}"), lines, &name.clone(), move |app| {
                    let label = format!("reclaim space of {name}");
                    app.job(label.clone(), move |ctx| async move {
                        let m = ops::reclaim_space(&ctx.client, &repo, &keep).await?;
                        ap(move |app| follow(app, m, label.clone(), move |app, t| reclaimed(app, &label, &t)))
                    })
                })
            })
        });
        Ok(())
    });
}

/// The reclaim task ended: the files its progress report deleted.
fn reclaimed(app: &mut App, label: &str, t: &Task) {
    let done = t.progress_reports.iter().find(|r| r.code == "reclaim-space.artifact").and_then(|r| r.done).unwrap_or(0);
    app.ok(format!("{label}: deleted {done} file(s)"));
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

/// Publication options of the server's publication schema. Each starts empty: the repository setting or the server default.
fn publication_fields(caps: Option<&Capabilities>, t: RepoType) -> Vec<Field> {
    let schema = t.publication_schema();
    let default = "empty = repository setting or server default";
    let choice = |k: &str, label: &str| Field::new(k, label, FieldKind::Choice(enum_choices(caps, schema, k))).help(default);
    // Not Bool: empty must stay "not sent", and Pulp's default differs per flag.
    let flag =
        |k: &str, label: &str| Field::new(k, label, FieldKind::Choice(["", "true", "false"].map(String::from).to_vec())).help(default);
    let mut all = match t {
        RepoType::Rpm => vec![
            choice("checksum_type", "Checksum type"),
            choice("compression_type", "Metadata compression"),
            choice("layout", "Package layout"),
            Field::text("repo_config", "Repo config (JSON)").help(r#"e.g. {"gpgcheck": 1}; empty = repository setting"#),
        ],
        RepoType::File => vec![],
        RepoType::Deb => vec![
            flag("simple", "Simple (all packages in one component)"),
            flag("structured", "Structured (the upstream components)"),
            flag("publish_legacy_release_files", "Legacy Release files"),
            choice("layout", "Package layout"),
            flag("publish_upstream_release_fields", "Publish upstream Release fields"),
            Field::text("excluded_package_metadata_fields", "Excluded package metadata fields")
                .help("field,...; empty = repository setting"),
        ],
    };
    all.push(flag("checkpoint", "Checkpoint").help("true = a checkpoint publication of the latest version; empty = no"));
    all.into_iter().filter(|f| caps.is_none_or(|c| c.has_field(schema, &f.key))).collect()
}

/// The publication options (and signing service) that the operator set; empty ones are not sent.
fn publication_options(f: &Form) -> Result<Value, String> {
    let mut b = json!({});
    for fld in f.fields.iter().filter(|x| !matches!(x.key.as_str(), "repo" | "number")) {
        let v = fld.value.trim();
        if v.is_empty() {
            continue;
        }
        b[fld.key.as_str()] = match fld.key.as_str() {
            "repo_config" => json_object(v).map_err(|e| format!("{}: {e}", fld.label))?,
            "excluded_package_metadata_fields" => v.split(',').map(str::trim).filter(|x| !x.is_empty()).collect::<Vec<_>>().into(),
            _ if v == "true" || v == "false" => (v == "true").into(),
            _ => v.into(),
        };
    }
    // pulp_deb refuses a publication with neither mode (HTTP 400); `simple` is off by default.
    if b["structured"] == false && b["simple"] != true {
        return Err("Structured: off needs Simple on (Pulp publishes in at least one mode)".into());
    }
    Ok(b)
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

/// Remote form fields that only some servers or plugins have; `v` is the remote being edited (`Null` on create).
fn remote_extra_fields(t: RepoType, caps: Option<&Capabilities>, v: &Value) -> Vec<Field> {
    let has = |f: &str| caps.is_none_or(|c| c.has_field(t.remote_schema(), f));
    let write_only = if v.is_null() { "" } else { WRITE_ONLY };
    // Floats on the server: `s()` would show `3600.0`, which is not a whole number.
    // shortcut: whole seconds only (a Number field); a fractional server value must be retyped on edit. Add a decimal kind when needed.
    let secs = |k: &str, label: &str, help: &str| {
        let f = Field::new(k, label, FieldKind::Number).value(v[k].as_f64().map(|x| x.to_string()).unwrap_or_default());
        f.help(help)
    };
    let headers = if v.is_null() { "Name: value; Name: value" } else { "Name: value; ...; empty = unchanged, - = clear" };
    let common = [
        secs("total_timeout", "Total timeout (s)", "seconds one download may take (large packages need more); empty = server default"),
        secs("connect_timeout", "Connect timeout (s)", "seconds; empty = server default"),
        secs("sock_connect_timeout", "Socket connect timeout (s)", "seconds; empty = server default"),
        secs("sock_read_timeout", "Socket read timeout (s)", "seconds; empty = server default"),
        Field::new("max_retries", "Max retries", FieldKind::Number).value(s(v, "max_retries")).help("empty = server default (3)"),
        // Readable on the server, but it can hold an Authorization token: never shown, as the password.
        Field::new("headers", "Headers", FieldKind::Secret).help(headers),
    ];
    let mut fields: Vec<Field> = match t {
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
            let names: Vec<&str> =
                v["excluded_package_metadata_fields"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let all = [
                text("distributions", "Distributions").required().help("space-separated, e.g. bookworm bookworm-updates"),
                text("components", "Components").help("space-separated; empty = all"),
                text("architectures", "Architectures").help("space-separated; empty = all"),
                flag("sync_sources", "Sync source packages"),
                flag("sync_udebs", "Sync udebs"),
                flag("sync_installer", "Sync installer files"),
                flag("ignore_missing_package_indices", "Ignore missing package indices"),
                Field::text("gpgkey", "GPG public key (file path)").help(write_only),
                Field::text("excluded_package_metadata_fields", "Excluded package metadata fields")
                    .value(names.join(","))
                    .help("field,...: dropped from the package indices at sync (Package, Version, ... never)"),
            ];
            all.into_iter().filter(|f| has(&f.key)).collect()
        }
    };
    fields.extend(common.into_iter().filter(|f| has(&f.key)));
    fields
}

/// `Name: value; Name: value` as Pulp's remote headers: one object per header. An error never shows the value.
// shortcut: a value cannot hold `;` (a Cookie with several pairs); accept JSON when someone needs it.
fn parse_headers(v: &str) -> Result<Value, String> {
    let token = |k: &str| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c));
    let mut out = vec![];
    for (i, part) in v.split(';').map(str::trim).filter(|p| !p.is_empty()).enumerate() {
        match part.split_once(':').map(|(k, v)| (k.trim(), v.trim())) {
            Some((k, v)) if token(k) => out.push(json!({ k: v })),
            _ => return Err(format!("header {} must be Name: value (separate headers with ;)", i + 1)),
        }
    }
    if out.is_empty() {
        return Err("header 1 must be Name: value (separate headers with ;)".into());
    }
    Ok(out.into())
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
            // pulpcore: `headers` is not nullable; an empty list removes them.
            ("headers", _) if val.trim() == "-" => b["headers"] = json!([]),
            ("headers", _) => b["headers"] = parse_headers(&val).map_err(|e| format!("{}: {e}", fld.label))?,
            ("excluded_package_metadata_fields", _) => {
                b[fld.key.as_str()] = val.split(',').map(str::trim).filter(|x| !x.is_empty()).collect::<Vec<_>>().into()
            }
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
    // Pulp checks a file:// path against its ALLOWED_IMPORT_PATHS.
    url_with_scheme(f.get("url"), &["http", "https", "file"])?;
    // Pulp stores any proxy text; a bad one fails only at sync time. An unchanged one on edit is not sent.
    if let Some(p) = b["proxy_url"].as_str() {
        url_with_scheme(p, &["http", "https"]).map_err(|e| format!("Proxy URL: {e}"))?;
    }
    Ok((b, files))
}

/// `u` parsed as a URL with one of `schemes`. A slip like `host:8000/x` parses too, with the scheme `host`.
pub(super) fn url_with_scheme(u: &str, schemes: &[&str]) -> Result<url::Url, String> {
    let url = url::Url::parse(u).map_err(|e| format!("invalid URL: {e}"))?;
    if !schemes.contains(&url.scheme()) {
        return Err(format!("invalid URL: use {}:// (not {}:)", schemes.join("://, "), url.scheme()));
    }
    Ok(url)
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
                .and_then(|c| c.field_enum(t.remote_schema(), "policy").map(|e| e.to_vec()))
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
            let lines = remote_delete_impact(&ctx.client, t, &href, &name).await?;
            ap(move |app| {
                app.confirm_danger(format!("Delete remote {name}"), lines, &name.clone(), move |app| {
                    submit(app, format!("delete remote {name}"), move |c| async move { Ok(c.delete(&href).await?) })
                })
            })
        }),
        _ => {}
    }
}

// ---- alternate content sources -------------------------------------------------------------

/// `path,...` (or space-separated) as the paths of an ACS; empty = the remote URL itself.
fn acs_paths(v: &str) -> Vec<String> {
    v.split([',', ' ']).map(str::trim).filter(|p| !p.is_empty()).map(str::to_string).collect()
}

fn acs_action(app: &mut App, t: RepoType, act: Act, item: Value) {
    let (href, name) = (s(&item, "pulp_href"), s(&item, "name"));
    match act {
        Act::Create | Act::Edit => acs_edit(app, t, act == Act::Edit, item),
        // A task group: the Task Center follows it and reports how it ended.
        Act::Refresh => {
            submit(app, format!("refresh ACS {name}"), move |c| async move { Ok(c.post(&format!("{href}refresh/"), &json!({})).await?) })
        }
        Act::Delete => {
            let n = item["paths"].as_array().map_or(0, Vec::len);
            let mut lines = vec![
                format!("Delete alternate content source {name} and its {n} path(s). Its remote and the content stay."),
                "Pulp also deletes the hidden repositories of its refreshes. Syncs then download from their remotes only.".into(),
            ];
            match t {
                RepoType::Rpm | RepoType::File => {}
                RepoType::Deb => {
                    lines.push(format!("pulp_deb keeps its hidden repository {name}--repository; a new ACS {name} uses it again."))
                }
            }
            app.confirm_danger(format!("Delete ACS {name}"), lines, &name.clone(), move |app| {
                submit(app, format!("delete ACS {name}"), move |c| async move { Ok(c.delete(&href).await?) })
            });
        }
        _ => {}
    }
}

/// ACS Create, or Edit of `item`. The remote field holds a name, as in the repository form.
fn acs_edit(app: &mut App, t: RepoType, edit: bool, item: Value) {
    let (href, name) = (s(&item, "pulp_href"), s(&item, "name"));
    let current = item["remote"].as_str().filter(|_| edit).map(str::to_string);
    let label = format!("remote of {name}");
    let open = move |app: &mut App, remote: String| {
        let paths: Vec<&str> = item["paths"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let mut fields = vec![
            Field::text("name", "Name").required().value(s(&item, "name")),
            Field::pick("remote", "Remote", Source::Remotes(t)).required().value(remote).help("Pulp takes only an on_demand remote"),
        ];
        match t {
            RepoType::Rpm | RepoType::File => {
                let help = "path,... added to the remote URL; empty = the remote URL";
                fields.push(Field::text("paths", "Paths").value(paths.join(",")).help(help));
            }
            RepoType::Deb => {} // pulp_deb refreshes the remote URL and ignores paths
        }
        let title = if edit { format!("Edit ACS {name}") } else { format!("New {} alternate content source", t.label()) };
        let form = Form::new(title, fields).note("Refresh (R) indexes the source; then syncs download the files it has from it.");
        app.form(form, move |app, f| {
            let mut b = json!({});
            for k in ["name", "remote"] {
                if !edit || f.changed(k) {
                    b[k] = f.get(k).into();
                }
            }
            let new = acs_paths(f.get("paths"));
            let old = f.fields.iter().find(|x| x.key == "paths").map(|x| acs_paths(&x.initial)).unwrap_or_default();
            if f.fields.iter().any(|x| x.key == "paths") && (!edit || f.changed("paths")) {
                b["paths"] = new.clone().into();
            }
            let removed: Vec<String> = old.into_iter().filter(|p| !new.contains(p)).collect();
            let (href, label) = (href.clone(), format!("{} ACS {}", if edit { "update" } else { "create" }, f.get("name")));
            let me = href.clone();
            let go = move |app: &mut App| {
                submit(app, label, move |c| async move {
                    if let Some(r) = b["remote"].as_str().map(str::to_string) {
                        b["remote"] = href_by_name(&c, t.remotes(), &r).await?.into();
                    }
                    Ok(if edit { c.patch(&href, &b).await? } else { c.post(&c.api(t.acs()), &b).await? })
                })
            };
            if removed.is_empty() {
                go(app);
                return Ok(());
            }
            // pulpcore 3.123 deletes a removed path from every ACS with that path (`_update_paths`).
            app.job("alternate content sources with the removed paths", move |ctx| async move {
                let c = &ctx.client;
                let mut lines = vec![];
                for u in RepoType::ALL.into_iter().filter(|u| ctx.caps.supports(u.feature()) && ctx.caps.has_endpoint(u.acs())) {
                    let all: Vec<Value> = c.list_all(&c.api(u.acs()), &[("fields", "pulp_href,name,paths".into())], 100_000).await?;
                    for a in all.iter().filter(|a| s(a, "pulp_href") != me) {
                        let lost: Vec<&str> = a["paths"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .filter(|p| removed.iter().any(|r| r == p))
                            .collect();
                        if !lost.is_empty() {
                            lines.push(format!("  {} ({}): {}", s(a, "name"), u.label(), lost.join(", ")));
                        }
                    }
                }
                ap(move |app| {
                    if lines.is_empty() {
                        return go(app);
                    }
                    lines.insert(
                        0,
                        "Pulp deletes a removed path from every alternate content source that has it. These lose it too:".into(),
                    );
                    app.confirm("Remove ACS paths", lines, go);
                })
            });
            Ok(())
        });
    };
    match current {
        // The picker holds names: read the current remote's name so the field shows it.
        Some(r) => app.job(label, move |ctx| async move {
            let v = ctx.client.get::<Value>(&r, &[("fields", "name".into())]).await?;
            ap(move |app| open(app, s(&v, "name")))
        }),
        None => open(app, String::new()),
    }
}

// ---- versions and content ------------------------------------------------------------------

/// The units added and removed from version `a` to `b`, with a warning for a version of which Pulp
/// counts other units than the compare lists (a content type it does not know, or two units with one key).
async fn compare_versions(c: &PulpClient, caps: &Capabilities, t: RepoType, a: &str, b: &str) -> anyhow::Result<Vec<String>> {
    let (ka, kb) = (ops::all_content_keys(c, caps, t, a).await?, ops::all_content_keys(c, caps, t, b).await?);
    let mut lines = ops::diff(&ka, &kb).lines(&short(a), &short(b));
    let mut warnings: Vec<String> = ops::not_compared(caps, t).into_iter().collect();
    for (v, keys) in [(a, &ka), (b, &kb)] {
        let n = c.get::<RepoVersion>(v, &[]).await?.content_summary.present_count();
        if n != keys.len() as u64 {
            warnings.push(format!(
                "warning: Pulp counts {n} units in {}, this compare lists {}: the difference can be incomplete",
                short(v),
                keys.len()
            ));
        }
    }
    lines.splice(1..1, warnings);
    Ok(lines)
}

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
        Act::Restore => restore_preview(app, t, href, number),
        Act::Repair => repair(app, t, href, number),
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
                let lines = compare_versions(&ctx.client, &ctx.caps, t, &a, &b).await?;
                ap(move |app| app.text("Version comparison", lines))
            });
        }
        Act::Delete => app.job(format!("impact of deleting version {number}"), move |ctx| async move {
            let c = &ctx.client;
            // Fails closed: without the Content View store the snapshot check cannot run.
            if ctx.cv()?.store.protected_versions(&ctx.scope).await?.contains(&href) {
                bail!("refused: this version is a snapshot of a Content View version (delete the CV version instead)");
            }
            let (pubs, by) = ops::version_protection(c, &ctx.caps, t, &href).await?;
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

/// Repair of `version`: ask how deep to check, show what the version has, then repair on confirmation.
fn repair(app: &mut App, t: RepoType, version: String, number: String) {
    if caps(app).is_some_and(|c| !c.has_endpoint(t.version_repair())) {
        return app.error(format!("not supported by this server: repository version repair ({})", t.version_repair()));
    }
    let help = "on = read every file and compare its digest (slow); off = check only that each file exists";
    let verify = Field::new("verify_checksums", "Verify checksums", FieldKind::Bool).value("true").help(help);
    app.form(Form::new(format!("Repair version {number}"), vec![verify]), move |app, f| {
        let (version, number, verify) = (version.clone(), number.clone(), f.bool("verify_checksums"));
        app.job(format!("impact of repairing version {number}"), move |ctx| async move {
            let n = ctx.client.get::<RepoVersion>(&version, &[]).await?.content_summary.present_count();
            if n == 0 {
                bail!("refused: version {number} has no content; Pulp would repair every file of the server");
            }
            let lines = vec![
                format!("Repair version {number} ({}): Pulp checks the files of its {n} content unit(s).", short(&version)),
                if verify { "It reads every file and compares its digest (slow)." } else { "It checks only that each file exists." }.into(),
                "It downloads a missing or corrupt file again from a remote that has it,".into(),
                "and deletes a corrupt file that no remote can give again.".into(),
            ];
            ap(move |app| {
                app.confirm(format!("Repair version {number}"), lines, move |app| {
                    let label = format!("repair version {number}");
                    app.job(label.clone(), move |ctx| async move {
                        let m = ops::repair_version(&ctx.client, &version, verify).await?;
                        ap(move |app| follow(app, m, label.clone(), move |app, t| repaired(app, &label, &t)))
                    })
                })
            })
        });
        Ok(())
    });
}

/// The repair task ended: what its progress reports counted.
fn repaired(app: &mut App, label: &str, t: &Task) {
    let done = |code: &str| t.progress_reports.iter().find(|r| r.code == code).and_then(|r| r.done).unwrap_or(0);
    let (missing, corrupt, fixed) = (done("repair.missing"), done("repair.corrupted"), done("repair.repaired"));
    let text = format!("{label}: {missing} missing, {corrupt} corrupt, {fixed} repaired");
    if fixed < missing + corrupt { app.notify(Level::Warn, format!("{text}; see the task for the others")) } else { app.ok(text) }
}

/// Show what a distribution serves now and what it will serve, then re-point on confirmation.
fn rollback_preview(app: &mut App, t: RepoType, dist: String, version: String) {
    app.job(format!("rollback preview {dist}"), move |ctx| async move {
        let c = &ctx.client;
        let d: Value =
            ops::find_by_name(c, &c.api(t.distributions()), &dist).await?.ok_or_else(|| anyhow!("no {} distribution {dist}", t.label()))?;
        cv_distribution_refusal(&d)?;
        // The new publication keeps the signing service of the served one (a Deb publication can have its own).
        let (current, signing) = match served_now(c, &ctx.caps, t, &d).await? {
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
                    let check = ops::verify_served(c, t, &s(&d, "base_url"), &version, &pv).await;
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

/// Preview a new latest version with the content of `version` (what changes from the latest version), then make
/// it on confirmation: `modify/` with `base_version`. The latest version itself is refused: nothing to restore.
fn restore_preview(app: &mut App, t: RepoType, version: String, number: String) {
    app.job(format!("restore preview v{number}"), move |ctx| async move {
        let c = &ctx.client;
        let repo = repo_of_version(&version);
        let r: Value = c.get(&repo, &[]).await?;
        if let Some(e) = snapshot_refusal(&r) {
            bail!(e);
        }
        let (name, latest) = (s(&r, "name"), s(&r, "latest_version_href"));
        if latest == version {
            bail!("version {number} is already the latest version of {name}");
        }
        let drops: &[&str] = if t == RepoType::Deb { &deb::NEW_VERSION_DROPS } else { &[] };
        let new = ops::content_keys_except(c, &ctx.caps, t, &version, drops).await?;
        let d = ops::diff(&ops::all_content_keys(c, &ctx.caps, t, &latest).await?, &new);
        let mut diff = d.lines(&format!("latest {}", short(&latest)), &format!("new (= v{number})"));
        diff.splice(1..1, ops::not_compared(&ctx.caps, t));
        let lines = restore_lines(t, &r, &number, diff);
        let typed = name.clone();
        ap(move |app| {
            app.confirm_prod(format!("Restore version {number} as latest"), lines, &typed, move |app| {
                let label = format!("restore {name} v{number} as latest");
                app.job(label.clone(), move |ctx| async move {
                    let m = ops::restore_version(&ctx.client, &repo, &version).await?;
                    ap(move |app| follow(app, m, label.clone(), move |app, t| new_version(app, &label, &t)))
                })
            })
        })
    });
}

/// The restore confirm: what it does, how it differs from Rollback, what serves it, and the content `diff`.
fn restore_lines(t: RepoType, r: &Value, number: &str, diff: Vec<String>) -> Vec<String> {
    let mut lines = vec![
        format!("Make a new latest version of {} with the content of version {number}; newer versions are kept.", s(r, "name")),
        "Rollback (b) instead re-points one distribution and makes no version. After a restore, the".into(),
        "distributions that follow the repository and the next sync, copy and publish use the new version.".into(),
        if r["autopublish"] == true {
            "Autopublish is on: Pulp publishes the new version at once.".into()
        } else {
            "Autopublish is off: publish the new version (P) to serve it.".into()
        },
    ];
    if let Some(n) = r["retain_repo_versions"].as_i64() {
        lines.push(format!("retain_repo_versions = {n}: Pulp then deletes the versions older than the newest {n}."));
    }
    if t == RepoType::Deb {
        lines.push(
            "pulp_deb removes Release files and package and installer file indices from every new version; the next publish writes new ones."
                .into(),
        );
    }
    lines.push(String::new());
    lines.extend(diff);
    lines
}

/// A task that changes a repository ended (restore, upload): the new version, or none when the latest one already had that content.
fn new_version(app: &mut App, label: &str, t: &Task) {
    match t.created_matching("/versions/") {
        Some(v) => app.ok(format!("{label}: new version {}", version_number(&v))),
        None => app.ok(format!("{label}: no new version (the latest version already has this content)")),
    }
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
            fields.extend(publication_fields(caps(app).as_deref(), t));
            let form = Form::new(format!("New {} publication", t.label()), fields);
            app.form(form, move |app, f| {
                let (repo, number, options) = (f.get("repo").to_string(), f.number("number"), publication_options(f)?);
                // Pulp takes `checkpoint` only with `repository`, and then publishes the latest version.
                let checkpoint = options["checkpoint"] == true;
                if checkpoint && number.is_some() {
                    return Err("Checkpoint publishes the latest version: leave Version number empty".into());
                }
                app.job(format!("publish {repo}"), move |ctx| async move {
                    let c = &ctx.client;
                    let r: Value =
                        ops::find_by_name(c, &c.api(t.repositories()), &repo).await?.ok_or_else(|| anyhow!("no repository {repo}"))?;
                    let v = match number {
                        Some(n) => version_of(&s(&r, "pulp_href"), n),
                        None => r["latest_version_href"].as_str().ok_or_else(|| anyhow!("no versions"))?.to_string(),
                    };
                    let mut body = ops::publication_body(c, t, &v, None).await?;
                    if checkpoint {
                        body.as_object_mut().expect("an object").remove("repository_version");
                        body["repository"] = s(&r, "pulp_href").into();
                    }
                    body.as_object_mut().expect("an object").extend(options.as_object().cloned().unwrap_or_default());
                    let p = ops::create(c, &c.api(t.publications()), &body, "/publications/", &ctx.on_task()).await?;
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

/// Distribution flags (key, label, help), each shown when the server's distribution schema has it.
const DIST_FLAGS: [(&str, &str, &str); 3] = [
    ("hidden", "Hidden", "not listed by the content app; the URL still serves"),
    ("checkpoint", "Checkpoint", "serves the repository's checkpoint publications at <base path>/<timestamp>/"),
    ("generate_repo_config", "Serve config.repo", "RPM: a .repo file for clients"),
];

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
            let Some((version, publication, how)) = served_now(c, &ctx.caps, t, &d).await? else {
                bail!("distribution serves nothing (no binding, or no publication of its repository or version)");
            };
            let res = ops::verify_served(c, t, &s(&d, "base_url"), &version, &publication).await;
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
            let caps = caps(app);
            for (k, label, help) in DIST_FLAGS {
                if caps.as_ref().is_none_or(|c| c.has_field(t.distribution_schema(), k))
                    && (k != "generate_repo_config" || t == RepoType::Rpm)
                {
                    let on = if v[k] == true { "true" } else { "false" };
                    fields.push(Field::new(k, label, FieldKind::Bool).value(on).help(help));
                }
            }
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
                for (k, ..) in DIST_FLAGS {
                    // Create: only what is on (off is Pulp's default).
                    if if edit { f.changed(k) } else { f.bool(k) } {
                        b[k] = f.bool(k).into();
                    }
                }
                let (mode, repo, n) = (f.get("mode").to_string(), f.opt("repo").map(str::to_string), f.number("number"));
                // Pulp refuses it with a publication or version binding (HTTP 400).
                if f.bool("checkpoint") && !mode.starts_with("follow") {
                    return Err(
                        "Checkpoint needs Serve = follow repository latest: Pulp serves the checkpoint publications of a repository".into(),
                    );
                }
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
            let lines = distribution_delete_lines(&item);
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
        Act::Filter => {
            let Kind::ServerTasks(q) = app.list.kind.clone() else { return };
            let get = |k: &str| q.iter().find(|(p, _)| *p == k).map(|(_, v)| v.clone()).unwrap_or_default();
            let states = ["", "waiting", "running", "completed", "failed", "canceled", "canceling", "skipped"];
            let fields = vec![
                Field::new("state", "State", FieldKind::Choice(states.map(String::from).to_vec())).value(get("state")),
                Field::text("name", "Name contains").value(app.list.filter.clone()),
                Field::text("reserved_resources", "Resource href")
                    .value(get("reserved_resources"))
                    .help("tasks that locked it, e.g. a repository"),
                Field::text("started_at__gte", "Started at or after")
                    .value(get("started_at__gte"))
                    .help("2026-11-01 or 2026-11-01T08:00:00Z"),
            ];
            app.form(Form::new("Filter server tasks", fields).note("An empty field does not filter."), |app, f| {
                let q = ["state", "reserved_resources", "started_at__gte"].into_iter().filter_map(|k| f.opt(k).map(|v| (k, v.to_string())));
                let k = Kind::ServerTasks(q.collect());
                (app.list.kind, app.screen) = (k.clone(), Screen::List(k));
                (app.list.filter, app.list.offset, app.list.selected) = (f.get("name").to_string(), 0, 0);
                app.reload();
                Ok(())
            });
        }
        Act::Delete if !unsupported(app, Feature::TaskDelete) => {
            let (name, state) = (s(&item, "name"), s(&item, "state"));
            if !TASK_FINAL_STATES.contains(&state.as_str()) {
                return app.error(format!("task {name} is {state}: only a finished task can be deleted (cancel it first)"));
            }
            let lines = vec![
                format!("Delete the record of task {name} ({href}, {state}) on Pulp."),
                "What the task made stays; its state, progress and error are gone.".into(),
            ];
            app.confirm("Delete server task", lines, move |app| {
                submit(app, format!("delete task {name}"), move |c| async move { Ok(c.delete(&href).await?) })
            });
        }
        Act::PurgeTasks if !unsupported(app, Feature::TaskPurge) => {
            let fields = vec![
                Field::text("finished_before", "Finished before").required().help("2026-10-01 or 2026-10-01T08:00:00"),
                Field::text("states", "States").required().value("completed").help("comma separated: completed, failed, canceled, skipped"),
            ];
            app.form(Form::new("Purge finished tasks", fields), |app, f| {
                let before = f.get("finished_before").to_string();
                let states: Vec<String> = f.get("states").split(',').map(str::trim).filter(|v| !v.is_empty()).map(String::from).collect();
                if let Some(bad) = states.iter().find(|v| !TASK_FINAL_STATES.contains(&v.as_str())) {
                    return Err(format!("{bad} is not a final state ({})", TASK_FINAL_STATES.join(", ")));
                }
                purge_tasks(app, before, states);
                Ok(())
            });
        }
        Act::CancelTask => {
            let note = [
                "This is a server-side cancellation (not just closing the client).".to_string(),
                "Pulp decides the final state; work already done may remain (e.g. downloaded artifacts).".into(),
            ];
            if !href.contains("/task-groups/") {
                let lines = [vec![format!("Ask Pulp to cancel task {href}.")], note.to_vec()].concat();
                app.confirm("Cancel server task", lines, move |app| cancel_task(app, href));
            } else if !unsupported(app, Feature::TaskGroupCancel) {
                app.job("read task group", move |ctx| async move {
                    let g: TaskGroup = ctx.client.get(&href, &[]).await?;
                    let (active, all) = (g.active(), g.tasks.len());
                    let head =
                        format!("Ask Pulp to cancel the whole task group {}: {active} of {all} task(s) are not finished.", g.description);
                    let lines = [vec![head], note.to_vec()].concat();
                    ap(move |app| app.confirm("Cancel task group", lines, move |app| cancel_task(app, href)))
                });
            }
        }
        _ => {}
    }
}

/// Count the tasks that `tasks/purge/` would delete (the same filter as Pulp's purge), then purge after the typed profile name.
fn purge_tasks(app: &mut App, before: String, states: Vec<String>) {
    let profile = app.profile.clone().unwrap_or_default();
    app.job("count tasks to purge", move |ctx| async move {
        let c = &ctx.client;
        let q = [("finished_at__lt", before.clone()), ("state__in", states.join(",")), ("limit", "1".into())];
        let n = c.list::<Value>(&c.api("tasks/"), &q).await?.count;
        let lines = vec![
            format!("Purge on {profile}: {n} task(s) in state {} that finished before {before}.", states.join(", ")),
            "Pulp deletes their records (not what they made), only the ones that you may delete.".into(),
            "Pulp cannot undo this.".into(),
        ];
        ap(move |app| {
            let typed = profile.clone();
            app.confirm_danger("Purge finished tasks", lines, &typed, move |app| {
                let label = format!("purge tasks on {profile}");
                app.job(label.clone(), move |ctx| async move {
                    let c = &ctx.client;
                    let m = c.post(&c.api("tasks/purge/"), &json!({ "finished_before": before, "states": states })).await?;
                    ap(move |app| {
                        follow(app, m, label.clone(), move |app, t| {
                            let done = |code: &str| t.progress_reports.iter().find(|r| r.code == code).and_then(|r| r.done).unwrap_or(0);
                            let failed = done("purge.tasks.error");
                            let kept = if failed > 0 { format!(", {failed} could not be deleted") } else { String::new() };
                            app.ok(format!("{label}: deleted {} task(s){kept}", done("purge.tasks.key.core.Task")));
                        })
                    })
                })
            })
        })
    });
}

/// PATCH `state: canceled` on a task or a task group, then report the state that Pulp gives.
fn cancel_task(app: &mut App, href: String) {
    app.job("cancel task", move |ctx| async move {
        let res = ctx.client.patch(&href, &json!({ "state": "canceled" })).await;
        let t = ops::get_task(&ctx.client, &href).await?;
        ap(move |app| {
            match res {
                Ok(_) => app.notify(Level::Info, format!("cancel requested; Pulp reports state {}", t.state)),
                Err(e) => app.error(format!("Pulp refused cancellation ({e}); task state is {}", t.state)),
            }
            app.handle_msg(super::app::Msg::Task(t, None));
            app.reload();
        })
    });
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
    fn deb_repository_signing_fields_follow_the_server_schema() {
        let keys = |f: &[Field]| f.iter().map(|f| f.key.clone()).collect::<Vec<_>>();
        let all = ["signing_service", "signing_service_release_overrides", "package_signing_service", "package_signing_fingerprint"];
        let more = ["package_signing_fingerprint_release_overrides", "publish_upstream_release_fields", "excluded_package_metadata_fields"];
        let all = [&all[..], &more[..], &["retain_checkpoints"]].concat();
        assert_eq!(keys(&repo_extra_fields(RepoType::Deb, None, &Value::Null)), all);
        // pulp_deb before package signing (3.5, 3.8) has only the Release signing fields.
        let mut caps = Capabilities::default();
        let old = ["signing_service", "signing_service_release_overrides", "publish_upstream_release_fields"];
        caps.schemas.insert("deb.AptRepository".into(), old.map(String::from).into());
        assert_eq!(keys(&repo_extra_fields(RepoType::Deb, Some(&caps), &Value::Null)), old);
    }

    #[test]
    fn deb_repository_body_sends_the_signing_settings_and_on_edit_only_the_changes() {
        let set = |f: &mut Form, k: &str, v: &str| f.fields.iter_mut().find(|x| x.key == k).unwrap().value = v.into();
        let mut new = repo_form(RepoType::Deb, None, None, "");
        set(&mut new, "name", "apt");
        assert_eq!(new.get("publish_upstream_release_fields"), "true", "Pulp's default for a new repository");
        set(&mut new, "signing_service", "/s/1/");
        set(&mut new, "signing_service_release_overrides", "bookworm=/s/2/");
        let b = repo_body(&new, false).unwrap();
        assert_eq!(
            (&b["signing_service"], &b["signing_service_release_overrides"], &b["publish_upstream_release_fields"]),
            (&json!("/s/1/"), &json!({ "bookworm": "/s/2/" }), &json!(true))
        );
        assert!(b.get("package_signing_service").is_none() && b.get("package_signing_fingerprint").is_none(), "{b}");

        let item = json!({ "name": "apt", "signing_service": "/s/1/", "signing_service_release_overrides": { "bookworm": "/s/2/", "trixie": "/s/2/" },
            "package_signing_fingerprint": "v4:AB", "publish_upstream_release_fields": false, "pulp_labels": {} });
        let mut f = repo_form(RepoType::Deb, None, Some(&item), "");
        assert_eq!(repo_body(&f, true).unwrap(), json!({}), "untouched: nothing is sent");
        assert_eq!(f.get("signing_service_release_overrides"), "bookworm=/s/2/,trixie=/s/2/");
        assert_eq!((f.get("publish_upstream_release_fields"), f.get("package_signing_fingerprint")), ("false", "v4:AB"), "as stored");
        set(&mut f, "signing_service", "");
        set(&mut f, "signing_service_release_overrides", "trixie=/s/3/");
        set(&mut f, "package_signing_fingerprint", "");
        // pulp_deb changes only the releases a PATCH names: a removed release is sent as null.
        assert_eq!(
            repo_body(&f, true).unwrap(),
            json!({ "signing_service": null, "signing_service_release_overrides": { "bookworm": null, "trixie": "/s/3/" },
                "package_signing_fingerprint": null })
        );
        set(&mut f, "signing_service_release_overrides", "trixie");
        let e = repo_body(&f, true).unwrap_err();
        assert!(e.starts_with("Per-release signing services: ") && e.contains("must be key=value"), "{e}");
    }

    #[test]
    fn deb_repository_body_sends_fingerprint_overrides_and_excluded_fields() {
        let set = |f: &mut Form, k: &str, v: &str| f.fields.iter_mut().find(|x| x.key == k).unwrap().value = v.into();
        let (fp, ex) = ("package_signing_fingerprint_release_overrides", "excluded_package_metadata_fields");
        let mut new = repo_form(RepoType::Deb, None, None, "");
        set(&mut new, "name", "apt");
        let b = repo_body(&new, false).unwrap();
        assert!(b.get(fp).is_none() && b.get(ex).is_none(), "empty on create: not sent: {b}");
        set(&mut new, fp, "bookworm=v4:AB");
        set(&mut new, ex, "Phased-Update-Percentage, X-Note,");
        let b = repo_body(&new, false).unwrap();
        assert_eq!((&b[fp], &b[ex]), (&json!({ "bookworm": "v4:AB" }), &json!(["Phased-Update-Percentage", "X-Note"])));

        let item = json!({ "name": "apt", fp: { "bookworm": "v4:AB", "trixie": "v4:CD" }, ex: ["X-Note"], "pulp_labels": {} });
        let mut f = repo_form(RepoType::Deb, None, Some(&item), "");
        assert_eq!((f.get(fp), f.get(ex)), ("bookworm=v4:AB,trixie=v4:CD", "X-Note"), "as stored");
        assert_eq!(repo_body(&f, true).unwrap(), json!({}), "untouched: nothing is sent");
        // pulp_deb changes only the releases a PATCH names and deletes one sent as "" (its fingerprint is not nullable).
        set(&mut f, fp, "trixie=v4:EF");
        set(&mut f, ex, "");
        assert_eq!(repo_body(&f, true).unwrap(), json!({ fp: { "bookworm": "", "trixie": "v4:EF" }, ex: [] }));
        set(&mut f, fp, "trixie");
        let e = repo_body(&f, true).unwrap_err();
        assert!(e.starts_with("Per-release package fingerprints: ") && e.contains("must be key=value"), "{e}");
    }

    #[test]
    fn rpm_repository_fields_follow_the_server_schema() {
        let keys = |f: &[Field]| f.iter().map(|f| f.key.clone()).collect::<Vec<_>>();
        let mut caps = Capabilities::default();
        // Generated enum names (`LayoutD5aEnum` on pulp_rpm 3.39) can change: the choices come through the property's `$ref`.
        caps.load_openapi(&json!({ "components": { "schemas": {
            "rpm.RpmRepository": { "properties": {
                "checksum_type": { "allOf": [{ "$ref": "#/components/schemas/PackageChecksumType1aEnum" }], "nullable": true },
                "compression_type": { "allOf": [{ "$ref": "#/components/schemas/CompressionType2bEnum" }], "nullable": true },
                "layout": { "allOf": [{ "$ref": "#/components/schemas/Layout3cEnum" }], "nullable": true },
                "repo_config": {}, "package_signing_service": { "type": "string" }, "package_signing_fingerprint": { "type": "string" },
                "retain_checkpoints": { "type": "integer" } } },
            "PackageChecksumType1aEnum": { "enum": ["unknown", "md5", "sha1", "sha256", "sha512"] },
            "CompressionType2bEnum": { "enum": ["zstd", "gz", "none"] },
            "Layout3cEnum": { "enum": ["nested_alphabetically", "flat"] }
        } } }));
        let fields = repo_extra_fields(RepoType::Rpm, Some(&caps), &Value::Null);
        let new = ["checksum_type", "compression_type", "layout", "repo_config", "package_signing_service", "package_signing_fingerprint"];
        assert_eq!(keys(&fields), [&new[..], &["retain_checkpoints"]].concat());
        let choices = |k: &str| fields.iter().find(|f| f.key == k).unwrap().kind.clone();
        let c = |v: &[&str]| FieldKind::Choice(v.iter().map(|x| x.to_string()).collect());
        assert_eq!(choices("checksum_type"), c(&["", "sha256", "sha512"]), "empty = server default; no weak checksums");
        assert_eq!(choices("compression_type"), c(&["", "zstd", "gz", "none"]));
        assert_eq!(choices("layout"), c(&["", "nested_alphabetically", "flat"]));
        assert!(matches!(choices("package_signing_service"), FieldKind::Pick { source: Source::SigningServices, .. }));
        // pulp_rpm before these fields: none of them is shown.
        let mut old = Capabilities::default();
        old.schemas.insert("rpm.RpmRepository".into(), ["name", "retain_package_versions"].map(String::from).into());
        assert_eq!(keys(&repo_extra_fields(RepoType::Rpm, Some(&old), &Value::Null)), ["retain_package_versions"]);
    }

    #[test]
    fn rpm_repository_body_sends_the_publish_and_signing_settings() {
        let set = |f: &mut Form, k: &str, v: &str| f.fields.iter_mut().find(|x| x.key == k).unwrap().value = v.into();
        let mut new = repo_form(RepoType::Rpm, None, None, "");
        set(&mut new, "name", "os");
        let b = repo_body(&new, false).unwrap();
        for k in
            ["compression_type", "layout", "repo_config", "package_signing_service", "package_signing_fingerprint", "retain_checkpoints"]
        {
            assert!(b.get(k).is_none(), "empty on create: {k} not sent: {b}");
        }
        for (k, v) in [("compression_type", "zstd"), ("layout", "flat"), ("repo_config", r#"{"gpgcheck": 1, "repo_gpgcheck": 0}"#)] {
            set(&mut new, k, v);
        }
        for (k, v) in [("package_signing_service", "/s/3/"), ("package_signing_fingerprint", "v4:AB"), ("retain_checkpoints", "2")] {
            set(&mut new, k, v);
        }
        let b = repo_body(&new, false).unwrap();
        assert_eq!(
            [&b["compression_type"], &b["layout"], &b["repo_config"], &b["package_signing_service"], &b["package_signing_fingerprint"]],
            [&json!("zstd"), &json!("flat"), &json!({ "gpgcheck": 1, "repo_gpgcheck": 0 }), &json!("/s/3/"), &json!("v4:AB")]
        );
        assert_eq!(b["retain_checkpoints"], 2);

        let item = json!({ "name": "os", "compression_type": "gz", "layout": "flat", "repo_config": { "gpgcheck": 1 },
            "package_signing_service": "/s/3/", "package_signing_fingerprint": "v4:AB", "retain_checkpoints": 3, "pulp_labels": {} });
        let mut f = repo_form(RepoType::Rpm, None, Some(&item), "");
        assert_eq!((f.get("repo_config"), f.get("retain_checkpoints")), (r#"{"gpgcheck":1}"#, "3"), "as stored");
        assert_eq!(repo_body(&f, true).unwrap(), json!({}), "untouched: nothing is sent");
        set(&mut f, "layout", "nested_alphabetically");
        assert_eq!(repo_body(&f, true).unwrap(), json!({ "layout": "nested_alphabetically" }), "only the change");
        for k in
            ["compression_type", "layout", "repo_config", "package_signing_service", "package_signing_fingerprint", "retain_checkpoints"]
        {
            set(&mut f, k, "");
        }
        // pulp_rpm: `repo_config` is not nullable (an empty document is its default); the others are.
        assert_eq!(
            repo_body(&f, true).unwrap(),
            json!({ "compression_type": null, "layout": null, "repo_config": {}, "package_signing_service": null,
                "package_signing_fingerprint": null, "retain_checkpoints": null })
        );
        set(&mut f, "repo_config", "gpgcheck=1");
        let e = repo_body(&f, true).unwrap_err();
        assert!(e.starts_with("Repo config (JSON): "), "{e}");
        set(&mut f, "repo_config", "[1]");
        assert_eq!(repo_body(&f, true).unwrap_err(), "Repo config (JSON): must be a JSON object, e.g. {\"gpgcheck\": 1}");
        set(&mut f, "repo_config", "");
        set(&mut f, "retain_checkpoints", "two");
        assert_eq!(repo_body(&f, true).unwrap_err(), "Retain checkpoints must be a number");
    }

    #[test]
    fn every_repository_type_with_the_field_retains_checkpoints() {
        let set = |f: &mut Form, k: &str, v: &str| f.fields.iter_mut().find(|x| x.key == k).unwrap().value = v.into();
        for t in [RepoType::Rpm, RepoType::File, RepoType::Deb] {
            let mut new = repo_form(t, None, None, "");
            set(&mut new, "name", "r");
            assert!(repo_body(&new, false).unwrap().get("retain_checkpoints").is_none(), "{t:?}: empty on create is not sent");
            set(&mut new, "retain_checkpoints", "4");
            assert_eq!(repo_body(&new, false).unwrap()["retain_checkpoints"], 4, "{t:?}");
            let item = json!({ "name": "r", "retain_checkpoints": 4, "pulp_labels": {} });
            let mut f = repo_form(t, None, Some(&item), "");
            assert_eq!(repo_body(&f, true).unwrap(), json!({}), "{t:?}: untouched");
            set(&mut f, "retain_checkpoints", "");
            assert_eq!(repo_body(&f, true).unwrap(), json!({ "retain_checkpoints": null }), "{t:?}: cleared = keep all");
            // A server without checkpoints: no field.
            let mut caps = Capabilities::default();
            caps.schemas.insert(t.repository_schema().into(), ["name"].map(String::from).into());
            assert!(repo_form(t, Some(&caps), None, "").fields.iter().all(|f| f.key != "retain_checkpoints"), "{t:?}");
        }
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

    /// A remote form of type `t` for `v` (`Null` = create) with the fields that depend on the server.
    fn remote_form(t: RepoType, caps: Option<&Capabilities>, v: &Value) -> Form {
        let mut fields = vec![Field::text("name", "Name").value("r"), Field::text("url", "URL").value("https://h/")];
        fields.extend(remote_extra_fields(t, caps, v));
        Form::new("t", fields)
    }

    #[test]
    fn remote_timeouts_retries_and_headers_follow_the_server_schema() {
        let keys = |f: &Form| f.fields.iter().map(|x| x.key.clone()).collect::<Vec<_>>();
        let common = ["total_timeout", "connect_timeout", "sock_connect_timeout", "sock_read_timeout", "max_retries", "headers"];
        assert_eq!(keys(&remote_form(RepoType::File, None, &Value::Null))[2..], common);
        let deb = keys(&remote_form(RepoType::Deb, None, &Value::Null));
        assert_eq!((deb[2].as_str(), &deb[deb.len() - 6..]), ("distributions", &common.map(String::from)[..]), "Deb fields first");
        assert!(deb.contains(&"excluded_package_metadata_fields".to_string()));
        // A server without these fields: none of them.
        let mut caps = Capabilities::default();
        caps.schemas.insert("file.FileRemote".into(), ["name", "url"].map(String::from).into());
        assert_eq!(keys(&remote_form(RepoType::File, Some(&caps), &Value::Null)), ["name", "url"]);
        // Edit: the values as stored, but never the headers (they can hold an Authorization token).
        let item = json!({ "name": "r", "url": "https://h/", "connect_timeout": 10.0, "sock_read_timeout": null, "max_retries": 5,
            "headers": [{ "Authorization": "Bearer t" }], "excluded_package_metadata_fields": ["X-A", "X-B"] });
        let f = remote_form(RepoType::Deb, None, &item);
        assert_eq!(
            ["connect_timeout", "sock_read_timeout", "max_retries", "headers", "excluded_package_metadata_fields"].map(|k| f.get(k)),
            ["10", "", "5", "", "X-A,X-B"]
        );
        assert_eq!(f.fields.iter().find(|x| x.key == "headers").unwrap().kind, FieldKind::Secret);
    }

    #[test]
    fn remote_body_sends_timeouts_retries_headers_and_excluded_fields() {
        let set = |f: &mut Form, k: &str, v: &str| f.fields.iter_mut().find(|x| x.key == k).unwrap().value = v.into();
        let mut new = remote_form(RepoType::Deb, None, &Value::Null);
        set(&mut new, "distributions", "bookworm");
        let (b, _) = remote_body(&new, false).unwrap();
        for k in
            ["connect_timeout", "sock_connect_timeout", "sock_read_timeout", "max_retries", "headers", "excluded_package_metadata_fields"]
        {
            assert!(b.get(k).is_none(), "empty on create: {k} not sent: {b}");
        }
        set(&mut new, "connect_timeout", "5");
        set(&mut new, "max_retries", "2");
        set(&mut new, "headers", "Authorization: Bearer t; X-Org: a:b;");
        set(&mut new, "excluded_package_metadata_fields", "Phased-Update-Percentage, X-Note,");
        let (b, _) = remote_body(&new, false).unwrap();
        assert_eq!(
            [&b["connect_timeout"], &b["max_retries"], &b["headers"], &b["excluded_package_metadata_fields"]],
            [
                &json!(5),
                &json!(2),
                &json!([{ "Authorization": "Bearer t" }, { "X-Org": "a:b" }]),
                &json!(["Phased-Update-Percentage", "X-Note"])
            ]
        );
        assert!(b.get("sock_read_timeout").is_none());

        let item = json!({ "name": "r", "url": "https://h/", "distributions": "bookworm", "connect_timeout": 10.0, "max_retries": 5,
            "headers": [{ "Authorization": "Bearer t" }], "excluded_package_metadata_fields": ["X-A"] });
        let mut f = remote_form(RepoType::Deb, None, &item);
        assert_eq!(remote_body(&f, true).unwrap().0, json!({}), "untouched, and empty headers keep the server value");
        set(&mut f, "connect_timeout", "");
        set(&mut f, "max_retries", "");
        set(&mut f, "excluded_package_metadata_fields", "");
        set(&mut f, "headers", "-");
        // pulpcore: the headers list is not nullable; `[]` removes all.
        assert_eq!(
            remote_body(&f, true).unwrap().0,
            json!({ "connect_timeout": null, "max_retries": null, "excluded_package_metadata_fields": [], "headers": [] })
        );
        for bad in ["Bearer secret-token", ": x", "Bad Name: x"] {
            set(&mut f, "headers", bad);
            let e = remote_body(&f, true).unwrap_err();
            assert_eq!(e, "Headers: header 1 must be Name: value (separate headers with ;)");
        }
    }

    #[test]
    fn remote_url_needs_a_scheme_pulp_can_download() {
        let form = |url: &str| Form::new("t", vec![Field::text("name", "Name").value("r"), Field::text("url", "URL").value(url)]);
        // A slip like `ners.internal:8000/...` is a valid URL whose scheme is `ners.internal`; the sync fails later.
        for (slip, scheme) in [("foo:8000/x", "foo"), ("ners.internal:8000/file/PULP_MANIFEST", "ners.internal")] {
            assert_eq!(
                remote_body(&form(slip), false).unwrap_err(),
                format!("invalid URL: use http://, https://, file:// (not {scheme}:)")
            );
        }
        // Pulp downloads file:// from its ALLOWED_IMPORT_PATHS (the server checks the path).
        for ok in ["https://h/repo/", "http://h:8000/x", "file:///srv/import/x"] {
            assert_eq!(remote_body(&form(ok), false).unwrap().0["url"], ok);
        }
    }

    #[test]
    fn remote_proxy_url_needs_http_or_https() {
        let form = |proxy: &str| {
            let fields = vec![
                Field::text("name", "Name").value("r"),
                Field::text("url", "URL").value("https://h/"),
                Field::text("proxy_url", "Proxy URL"),
            ];
            let mut f = Form::new("t", fields);
            f.fields[2].value = proxy.into();
            f
        };
        // `proxy.corp:3128` is a valid URL with the scheme `proxy.corp`: Pulp stores it and the sync fails later.
        for (slip, scheme) in [("proxy.corp:3128", "proxy.corp"), ("socks5://p:1080", "socks5")] {
            assert_eq!(
                remote_body(&form(slip), false).unwrap_err(),
                format!("Proxy URL: invalid URL: use http://, https:// (not {scheme}:)")
            );
        }
        assert_eq!(remote_body(&form("not a url"), false).unwrap_err(), "Proxy URL: invalid URL: relative URL without a base");
        assert_eq!(remote_body(&form("http://proxy.corp:3128"), false).unwrap().0["proxy_url"], "http://proxy.corp:3128");
        // On edit an unchanged value is not sent, so it is not checked; a cleared one is null.
        let mut f = Form::new(
            "t",
            vec![Field::text("url", "URL").value("https://h/"), Field::text("proxy_url", "Proxy URL").value("proxy.corp:3128")],
        );
        assert_eq!(remote_body(&f, true).unwrap().0, json!({}));
        f.fields[1].value.clear();
        assert_eq!(remote_body(&f, true).unwrap().0, json!({ "proxy_url": null }));
    }

    #[tokio::test]
    async fn remote_policy_choices_come_from_the_policy_property() {
        use crate::tui::app::Modal;
        let server = wiremock::MockServer::start().await;
        let (mut app, _rx) = connected(&server).await;
        let mut caps = plugins();
        // The schema generator names the enum by a hash (`Policy692Enum` today): another server version can rename it.
        caps.load_openapi(&json!({ "components": { "schemas": {
            "file.FileRemote": { "properties": { "policy": { "allOf": [{ "$ref": "#/components/schemas/PolicyA1bEnum" }] } } },
            "PolicyA1bEnum": { "enum": ["immediate", "on_demand", "streamed", "later"] }
        } } }));
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        remote_action(&mut app, RepoType::File, Act::Create, Value::Null);
        let Some(Modal::Form { form, .. }) = app.modal.take() else { panic!("no form") };
        let policy = form.fields.iter().find(|f| f.key == "policy").unwrap();
        assert_eq!(policy.kind, FieldKind::Choice(["immediate", "on_demand", "streamed", "later"].map(String::from).to_vec()));
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
    fn sync_form_reads_its_enums_through_the_properties() {
        // The schema generator can name an enum by a hash: only the property name is stable.
        let mut caps = Capabilities::default();
        caps.load_openapi(&json!({ "components": { "schemas": {
            "RpmRepositorySyncURL": { "properties": {
                "sync_policy": { "allOf": [{ "$ref": "#/components/schemas/SyncPolicy1a2Enum" }] },
                "skip_types": { "type": "array", "items": { "$ref": "#/components/schemas/SkipTypes3b4Enum" } }
            } },
            "SyncPolicy1a2Enum": { "enum": ["additive", "mirror_complete", "mirror_content_only", "later"] },
            "SkipTypes3b4Enum": { "enum": ["srpm", "treeinfo", "later"] }
        } } }));
        assert_eq!(sync_policies(Some(&caps), RepoType::Rpm), ["additive", "mirror_complete", "mirror_content_only", "later"]);
        let keys: Vec<String> = sync_fields(RepoType::Rpm, Some(&caps)).into_iter().map(|f| f.key).collect();
        assert_eq!(keys, ["skip_srpm", "skip_treeinfo", "skip_later"]);
    }

    #[test]
    fn deb_remote_fields_follow_the_server_schema_and_prefill_on_edit() {
        let keys = |f: &[Field]| f.iter().map(|f| f.key.clone()).collect::<Vec<_>>();
        let all = ["distributions", "components", "architectures", "sync_sources", "sync_udebs", "sync_installer"];
        let all = [&all[..], &["ignore_missing_package_indices", "gpgkey", "excluded_package_metadata_fields"]].concat();
        let fields = remote_extra_fields(RepoType::Deb, None, &Value::Null);
        assert_eq!(keys(&fields)[..all.len()], all, "then the fields of all remote types");
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
        assert_eq!(keys(&form)[..3], ["repo", "number", "signing_service"], "then the publication options");
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
    async fn publication_options_follow_the_server_schema_and_send_only_what_is_set() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        let (rpm, deb) = (format!("{API}repositories/rpm/rpm/r/"), format!("{API}repositories/deb/apt/r/"));
        for (t, r) in [("rpm/rpm", &rpm), ("deb/apt", &deb)] {
            let repo = json!({ "pulp_href": r, "name": "r", "latest_version_href": format!("{r}versions/2/") });
            get(&server, &format!("{API}repositories/{t}/"), page(json!([repo]))).await;
            wiremock::Mock::given(method("POST"))
                .and(path(format!("{API}publications/{t}/")))
                .respond_with(
                    wiremock::ResponseTemplate::new(201).set_body_json(json!({ "pulp_href": format!("{API}publications/{t}/9/") })),
                )
                .mount(&server)
                .await;
        }
        let (mut app, mut rx) = connected(&server).await;
        let mut caps = Capabilities::default();
        // Generated enum names (`LayoutD5aEnum`, `LayoutDa3Enum`) can change: the choices come through the property's `$ref`.
        caps.load_openapi(&json!({ "components": { "schemas": {
            "rpm.RpmPublication": { "properties": { "repository_version": {}, "checkpoint": { "type": "boolean" }, "repo_config": {},
                "checksum_type": { "allOf": [{ "$ref": "#/components/schemas/Checksum1Enum" }] },
                "compression_type": { "allOf": [{ "$ref": "#/components/schemas/Compression2Enum" }] },
                "layout": { "allOf": [{ "$ref": "#/components/schemas/Layout3Enum" }] } } },
            "file.FilePublication": { "properties": { "repository_version": {}, "manifest": {}, "checkpoint": {} } },
            "Checksum1Enum": { "enum": ["unknown", "md5", "sha1", "sha256"] },
            "Compression2Enum": { "enum": ["zstd", "gz"] },
            "Layout3Enum": { "enum": ["nested_alphabetically", "flat"] }
        } } }));
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        let open = |app: &mut App, t: RepoType| {
            publication_action(app, t, Act::Create, Value::Null);
            let Some(Modal::Form { form, submit }) = app.modal.take() else { panic!("no form") };
            (form, submit)
        };
        let (mut form, mut submit) = open(&mut app, RepoType::Rpm);
        assert_eq!(keys(&form), ["repo", "number", "checksum_type", "compression_type", "layout", "repo_config", "checkpoint"]);
        let kind = |f: &Form, k: &str| f.fields.iter().find(|x| x.key == k).unwrap().kind.clone();
        let c = |v: &[&str]| FieldKind::Choice(v.iter().map(|x| x.to_string()).collect());
        assert_eq!(kind(&form, "checksum_type"), c(&["", "sha256"]));
        assert_eq!(kind(&form, "layout"), c(&["", "nested_alphabetically", "flat"]));
        assert_eq!(kind(&form, "checkpoint"), c(&["", "true", "false"]), "empty = server default");
        assert!(form.fields[2..].iter().all(|f| f.value.is_empty()), "every option starts at the server default");
        assert_eq!(keys(&open(&mut app, RepoType::File).0), ["repo", "number", "checkpoint"]);

        let set = |form: &mut Form, values: &[(&str, &str)]| {
            for (k, v) in values {
                form.fields.iter_mut().find(|f| f.key == *k).unwrap().value = v.to_string();
            }
        };
        set(&mut form, &[("repo", "r")]);
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        set(&mut form, &[("compression_type", "zstd"), ("layout", "flat"), ("repo_config", r#"{"gpgcheck": 1}"#), ("checkpoint", "false")]);
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        set(&mut form, &[("repo_config", "gpgcheck=1")]);
        assert!(submit(&mut app, &form).unwrap_err().starts_with("Repo config (JSON): "));
        // Pulp takes `checkpoint` only with `repository` and then publishes the latest version.
        set(&mut form, &[("repo_config", ""), ("layout", ""), ("compression_type", ""), ("checkpoint", "true"), ("number", "1")]);
        assert_eq!(submit(&mut app, &form).unwrap_err(), "Checkpoint publishes the latest version: leave Version number empty");
        set(&mut form, &[("number", "")]);
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        let v2 = format!("{rpm}versions/2/");
        assert_eq!(
            posts_to(&server, &format!("{API}publications/rpm/rpm/")).await,
            [
                json!({ "repository_version": v2 }),
                json!({ "repository_version": v2, "compression_type": "zstd", "layout": "flat", "repo_config": { "gpgcheck": 1 }, "checkpoint": false }),
                json!({ "repository": rpm, "checkpoint": true }),
            ]
        );

        app.conn.as_mut().unwrap().caps = Default::default();
        let (mut form, mut submit) = open(&mut app, RepoType::Deb);
        let deb_keys = ["simple", "structured", "publish_legacy_release_files", "layout", "publish_upstream_release_fields"];
        assert_eq!(keys(&form)[3..], [&deb_keys[..], &["excluded_package_metadata_fields", "checkpoint"]].concat());
        set(&mut form, &[("repo", "r"), ("structured", "false")]);
        assert_eq!(submit(&mut app, &form).unwrap_err(), "Structured: off needs Simple on (Pulp publishes in at least one mode)");
        set(&mut form, &[("simple", "true"), ("publish_legacy_release_files", "true"), ("excluded_package_metadata_fields", "X-A, X-B,")]);
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        assert_eq!(
            posts_to(&server, &format!("{API}publications/deb/apt/")).await,
            [json!({ "repository_version": format!("{deb}versions/2/"), "simple": true, "structured": false,
                "publish_legacy_release_files": true, "excluded_package_metadata_fields": ["X-A", "X-B"] })]
        );
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
        // The app is on the Dashboard, which a finished task reloads; every Pulp answers status/.
        get(server, &format!("{API}status/"), json!({ "versions": [] })).await;
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
    async fn restore_makes_a_new_latest_version_from_the_chosen_one_after_a_preview() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path, query_param};
        let server = wiremock::MockServer::start().await;
        let (repo, snap) = (format!("{API}repositories/file/file/r/"), format!("{API}repositories/file/file/snap/"));
        let v = |n: u32| format!("{repo}versions/{n}/");
        let r = json!({ "pulp_href": repo, "name": "docs", "latest_version_href": v(3), "autopublish": false, "retain_repo_versions": 2 });
        get(&server, &repo, r).await;
        get(&server, &snap, json!({ "pulp_href": snap, "name": "s", "latest_version_href": format!("{snap}versions/2/"), "pulp_labels": { "pulp_tui_cv": "web" } }))
            .await;
        let file = |p: &str| json!({ "pulp_href": format!("{API}content/file/files/{p}/"), "relative_path": p, "sha256": "ab" });
        for (n, files) in [(1, json!([file("a.txt")])), (3, json!([file("a.txt"), file("b.txt")]))] {
            wiremock::Mock::given(method("GET"))
                .and(path(format!("{API}content/file/files/")))
                .and(query_param("repository_version", v(n)))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(page(files)))
                .mount(&server)
                .await;
        }
        wiremock::Mock::given(method("POST"))
            .and(path(format!("{repo}modify/")))
            .respond_with(wiremock::ResponseTemplate::new(202).set_body_json(json!({ "task": format!("{API}tasks/1/") })))
            .mount(&server)
            .await;
        get(
            &server,
            &format!("{API}tasks/1/"),
            json!({ "pulp_href": format!("{API}tasks/1/"), "state": "completed", "created_resources": [v(4)] }),
        )
        .await;
        let (mut app, mut rx) = connected(&server).await;
        let last = |app: &App| app.notices.back().map(|n| n.text.clone()).unwrap_or_default();

        // Nothing to restore from the latest version itself, and a Content View snapshot changes only through its view.
        for (item, want) in [
            (json!({ "pulp_href": v(3), "number": 3 }), "version 3 is already the latest version of docs"),
            (json!({ "pulp_href": format!("{snap}versions/1/"), "number": 1 }), "refused: s is a snapshot repository of Content View web"),
        ] {
            version_action(&mut app, Act::Restore, item);
            settle(&mut app, &mut rx).await;
            assert!(app.modal.is_none() && last(&app).contains(want), "{}", last(&app));
        }

        version_action(&mut app, Act::Restore, json!({ "pulp_href": v(1), "number": 1 }));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { title, lines, on_yes, .. }) = app.modal.take() else { panic!("no confirm: {}", last(&app)) };
        assert_eq!(title, "Restore version 1 as latest");
        assert!(lines.contains(&"- file b.txt  (ab)".to_string()), "the preview shows what the new version loses: {lines:?}");
        for word in ["Rollback (b)", "publish", "retain_repo_versions = 2"] {
            assert!(lines.iter().any(|l| l.contains(word)), "{word}: {lines:?}");
        }
        assert!(posts_to(&server, &format!("{repo}modify/")).await.is_empty(), "nothing is sent before the confirm");
        on_yes.unwrap()(&mut app);
        settle(&mut app, &mut rx).await;
        assert_eq!(posts_to(&server, &format!("{repo}modify/")).await, [json!({ "base_version": v(1) })]);
        assert_eq!(last(&app), "restore docs v1 as latest: new version 4");
        // pulpcore makes no version when the latest one already has that content.
        new_version(&mut app, "restore docs v2 as latest", &Task { state: "completed".into(), ..Default::default() });
        assert_eq!(last(&app), "restore docs v2 as latest: no new version (the latest version already has this content)");
    }

    /// pulp_deb removes its Release files and indices from every new version: the preview does not promise them.
    #[tokio::test]
    async fn a_deb_restore_preview_leaves_out_the_units_pulp_deb_drops() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path, path_regex, query_param};
        let server = wiremock::MockServer::start().await;
        let repo = format!("{API}repositories/deb/apt/r/");
        let (v1, v2) = (format!("{repo}versions/1/"), format!("{repo}versions/2/"));
        get(&server, &repo, json!({ "pulp_href": repo, "name": "deb", "latest_version_href": v2, "autopublish": false })).await;
        let unit =
            |p: &str, id: &str| json!({ "pulp_href": format!("{API}content/deb/{p}/{id}/"), "relative_path": format!("dists/b/{id}") });
        for (p, id) in [("release_files", "Release"), ("package_indices", "Packages"), ("installer_file_indices", "SHA256SUMS")] {
            wiremock::Mock::given(method("GET"))
                .and(path(format!("{API}content/deb/{p}/")))
                .and(query_param("repository_version", v1.as_str()))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(page(json!([unit(p, id)]))))
                .mount(&server)
                .await;
        }
        wiremock::Mock::given(method("GET"))
            .and(path(format!("{API}content/deb/generic_contents/")))
            .and(query_param("repository_version", v1.as_str()))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(page(json!([unit("generic_contents", "g")]))))
            .mount(&server)
            .await;
        wiremock::Mock::given(method("GET"))
            .and(path_regex("/content/deb/"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(page(json!([]))))
            .mount(&server)
            .await;
        let (mut app, mut rx) = connected(&server).await;
        version_action(&mut app, Act::Restore, json!({ "pulp_href": v1, "number": 1 }));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, .. }) = app.modal.take() else { panic!("no confirm") };
        assert!(lines.contains(&"+ generic content dists/b/g  (g)".to_string()), "{lines:?}");
        for word in ["release file", "package index", "installer file index"] {
            assert!(!lines.iter().any(|l| l.starts_with('+') && l.contains(word)), "the new version lacks the {word}: {lines:?}");
        }
        let note = "pulp_deb removes Release files and package and installer file indices from every new version; the next publish writes new ones.";
        assert!(lines.contains(&note.to_string()), "{lines:?}");
    }

    #[test]
    fn the_restore_confirm_says_what_serves_the_new_version() {
        let lines = |r: Value| restore_lines(RepoType::File, &r, "2", vec!["+ file a.txt".into()]);
        let off = lines(json!({ "name": "docs", "autopublish": false, "retain_repo_versions": 3 }));
        assert_eq!(
            off,
            [
                "Make a new latest version of docs with the content of version 2; newer versions are kept.",
                "Rollback (b) instead re-points one distribution and makes no version. After a restore, the",
                "distributions that follow the repository and the next sync, copy and publish use the new version.",
                "Autopublish is off: publish the new version (P) to serve it.",
                "retain_repo_versions = 3: Pulp then deletes the versions older than the newest 3.",
                "",
                "+ file a.txt",
            ]
        );
        let on = lines(json!({ "name": "docs", "autopublish": true }));
        assert_eq!(on[3..], ["Autopublish is on: Pulp publishes the new version at once.", "", "+ file a.txt"], "no retain line: keep all");
    }

    #[tokio::test]
    async fn the_task_center_tracks_watches_cancels_and_dismisses_a_replication_group() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        let (group, task) = (format!("{API}task-groups/1/"), format!("{API}tasks/2/"));
        // After the cancel, Pulp reports the group with its sync canceled; the task had ended before the cancel.
        let tasks = json!([{ "name": "pulp_rpm.app.tasks.synchronizing.synchronize", "state": "canceled" }]);
        let g =
            json!({ "pulp_href": group, "description": "Replication of up", "all_tasks_dispatched": true, "canceled": 1, "tasks": tasks });
        get(&server, &group, g).await;
        get(&server, &task, json!({ "pulp_href": task, "name": "sync", "state": "completed" })).await;
        for (href, status) in [(&group, 200), (&task, 409)] {
            wiremock::Mock::given(method("PATCH"))
                .and(path(href.as_str()))
                .respond_with(wiremock::ResponseTemplate::new(status).set_body_json(json!({ "detail": "task already finished" })))
                .mount(&server)
                .await;
        }
        let (mut app, mut rx) = connected(&server).await;
        app.open(Screen::List(Kind::Tasks));
        let texts = |app: &App| app.notices.iter().map(|n| (n.level, n.text.clone())).collect::<Vec<_>>();

        task_action(&mut app, Act::TrackTask, json!({ "pulp_href": group, "name": "Replication of up" }));
        assert_eq!(texts(&app).last(), Some(&(Level::Ok, "tracking task".into())));
        // A server task has a name, no label.
        task_action(&mut app, Act::WatchTask, json!({ "pulp_href": task, "name": "sync" }));
        let Some(Modal::Text { title, watch, .. }) = app.modal.take() else { panic!("no watch view") };
        assert_eq!((title.as_str(), watch.as_deref()), ("Watch task sync", Some(task.as_str())));
        let rows = |app: &App| app.tasks.items.iter().map(|t| (t.href.clone(), t.state.clone())).collect::<Vec<_>>();
        assert_eq!(rows(&app), [(task.clone(), "waiting".into()), (group.clone(), "waiting".into())], "the newest on top");

        for href in [&group, &task] {
            task_action(&mut app, Act::CancelTask, json!({ "pulp_href": href }));
            settle(&mut app, &mut rx).await; // the group is read before its confirmation
            let Some(Modal::Confirm { title, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirmation") };
            assert_eq!(title, if href == &group { "Cancel task group" } else { "Cancel server task" });
            yes(&mut app);
            settle(&mut app, &mut rx).await;
        }
        let patches: Vec<_> = server.received_requests().await.unwrap().into_iter().filter(|r| r.method.as_str() == "PATCH").collect();
        assert!(patches.iter().all(|r| r.body_json::<Value>().unwrap() == json!({ "state": "canceled" })), "{patches:?}");
        assert_eq!(patches.iter().map(|r| r.url.path()).collect::<Vec<_>>(), [group.as_str(), task.as_str()]);
        // The group is read as one task: its state, and the counts as the error of a canceled group.
        let n = texts(&app);
        assert!(n.contains(&(Level::Info, "cancel requested; Pulp reports state canceled".into())), "{n:?}");
        let counts = "completed 0  failed 0  canceled 1  skipped 0";
        assert!(n.contains(&(Level::Warn, format!("task canceled: Replication of up: {counts}"))), "{n:?}");
        let refused = n.iter().find(|(l, t)| *l == Level::Error && t.starts_with("Pulp refused cancellation (")).expect("refusal");
        assert!(refused.1.ends_with("); task state is completed"), "{refused:?}");
        assert_eq!(rows(&app), [(task.clone(), "completed".into()), (group.clone(), "canceled".into())]);

        task_action(&mut app, Act::DismissTasks, Value::Null);
        assert_eq!(texts(&app).last(), Some(&(Level::Ok, "dismissed 2 finished task(s) from the list (Pulp keeps its records)".into())));
        assert!(app.tasks.items.is_empty() && app.list.items.is_empty());
    }

    #[tokio::test]
    async fn server_tasks_filter_by_state_name_resource_and_start_time() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path, query_param};
        let server = wiremock::MockServer::start().await;
        let repo = format!("{API}repositories/rpm/rpm/1/");
        let failed =
            json!({ "pulp_href": format!("{API}tasks/9/"), "name": "pulp_rpm.app.tasks.synchronizing.synchronize", "state": "failed" });
        wiremock::Mock::given(method("GET"))
            .and(path(format!("{API}tasks/")))
            .and(query_param("state", "failed"))
            .and(query_param("name__contains", "sync"))
            .and(query_param("reserved_resources", repo.as_str()))
            .and(query_param("started_at__gte", "2026-11-01"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(page(json!([failed]))))
            .mount(&server)
            .await;
        get(&server, &format!("{API}tasks/"), page(json!([]))).await;
        let (mut app, mut rx) = connected(&server).await;
        app.open(Screen::List(Kind::ServerTasks(vec![])));
        settle(&mut app, &mut rx).await;
        let filter = Kind::ServerTasks(vec![]).actions().into_iter().find(|a| a.key() == 'F').expect("F on Server tasks");
        assert!(!filter.mutates(&Kind::ServerTasks(vec![]), false) && !filter.needs_item(), "read-only mode and an empty list keep it");
        let form = |app: &mut App| {
            run(app, filter, Value::Null);
            let Some(Modal::Form { form, submit }) = app.modal.take() else { panic!("no form") };
            (form, submit)
        };
        let (mut f, mut submit) = form(&mut app);
        assert_eq!(keys(&f), ["state", "name", "reserved_resources", "started_at__gte"]);
        for (k, v) in [("state", "failed"), ("name", "sync"), ("reserved_resources", repo.as_str()), ("started_at__gte", "2026-11-01")] {
            f.fields.iter_mut().find(|x| x.key == k).unwrap().value = v.into();
        }
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        assert_eq!(app.list.items, [failed], "the server filtered: {:?}", app.list.error);
        assert!(app.list.kind.title().starts_with("Server tasks (state=failed, reserved_resources="), "{}", app.list.kind.title());

        // The form shows the filters in use; empty fields filter nothing.
        let (mut f, mut submit) = form(&mut app);
        assert_eq!(f.fields.iter().map(|x| x.value.as_str()).collect::<Vec<_>>(), ["failed", "sync", repo.as_str(), "2026-11-01"]);
        f.fields.iter_mut().for_each(|x| x.value.clear());
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        assert_eq!((app.list.kind.clone(), app.list.filter.as_str()), (Kind::ServerTasks(vec![]), ""));
        assert!(app.list.items.is_empty());
    }

    #[tokio::test]
    async fn purge_counts_the_matching_tasks_then_purges_after_the_typed_name() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{body_json, method, path, query_param};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path(format!("{API}tasks/")))
            .and(query_param("finished_at__lt", "2026-10-01"))
            .and(query_param("state__in", "completed,failed"))
            .and(query_param("limit", "1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({ "count": 7, "next": null, "results": [] })))
            .mount(&server)
            .await;
        wiremock::Mock::given(method("POST"))
            .and(path(format!("{API}tasks/purge/")))
            .and(body_json(json!({ "finished_before": "2026-10-01", "states": ["completed", "failed"] })))
            .respond_with(wiremock::ResponseTemplate::new(202).set_body_json(json!({ "task": format!("{API}tasks/1/") })))
            .expect(1)
            .mount(&server)
            .await;
        let reports = json!([{ "code": "purge.tasks.key.core.Task", "message": "Purged task-objects of type core.Task", "done": 7 },
                             { "code": "purge.tasks.error", "message": "Tasks failed to purge", "done": 0 }]);
        get(
            &server,
            &format!("{API}tasks/1/"),
            json!({ "pulp_href": format!("{API}tasks/1/"), "state": "completed", "progress_reports": reports }),
        )
        .await;
        let (mut app, mut rx) = connected(&server).await;
        app.list.kind = Kind::ServerTasks(vec![]);
        let purge = app.list.kind.actions().into_iter().find(|a| a.key() == 'P').expect("P on Server tasks");
        assert!(purge.mutates(&app.list.kind, false) && !purge.needs_item());
        run(&mut app, purge, Value::Null);
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
        assert_eq!(
            (keys(&form), form.get("states")),
            (vec!["finished_before".to_string(), "states".into()], "completed"),
            "Pulp's default"
        );
        form.fields[0].value = "2026-10-01".into();
        form.fields[1].value = "completed, running".into();
        assert_eq!(submit(&mut app, &form), Err("running is not a final state (completed, failed, canceled, skipped)".into()));
        form.fields[1].value = "completed, failed".into();
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, typed, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm") };
        assert_eq!(typed.as_deref(), Some("lab"), "the operator types the profile name");
        assert_eq!(lines[0], "Purge on lab: 7 task(s) in state completed, failed that finished before 2026-10-01.");
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        assert_eq!(app.notices.back().map(|n| n.text.as_str()), Some("purge tasks on lab: deleted 7 task(s)"));

        // A server without the endpoint: refused before the form.
        let mut caps = plugins();
        caps.paths.insert("tasks/".into());
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        run(&mut app, purge, Value::Null);
        assert!(app.modal.is_none());
        assert_eq!(app.notices.back().map(|n| n.text.as_str()), Some("not supported by this server: task purge (tasks/purge/)"));
    }

    #[tokio::test]
    async fn delete_marked_distributions_confirms_once_and_keeps_the_failed_mark() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        let (d1, d2) = (format!("{API}distributions/file/file/1/"), format!("{API}distributions/file/file/2/"));
        get(&server, &d1, json!({ "pulp_href": d1, "name": "one", "base_url": "http://x/one/" })).await;
        get(&server, &d2, json!({ "pulp_href": d2, "name": "two", "base_url": "http://x/two/" })).await;
        let accepted = wiremock::ResponseTemplate::new(202).set_body_json(json!({ "task": format!("{API}tasks/1/") }));
        wiremock::Mock::given(method("DELETE")).and(path(d1.as_str())).respond_with(accepted).expect(1).mount(&server).await;
        let refused = wiremock::ResponseTemplate::new(500).set_body_json(json!({ "detail": "boom" }));
        wiremock::Mock::given(method("DELETE")).and(path(d2.as_str())).respond_with(refused).expect(1).mount(&server).await;
        get(&server, &format!("{API}tasks/1/"), json!({ "pulp_href": format!("{API}tasks/1/"), "state": "completed" })).await;
        let (mut app, mut rx) = connected(&server).await;
        app.list.kind = Kind::Distributions(RepoType::File);
        app.list.marked = [d1.clone(), d2.clone()].into();
        run(&mut app, Act::Delete, json!({ "pulp_href": d1, "name": "one" }));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { title, lines, typed, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm") };
        assert_eq!((title.as_str(), typed.as_deref()), ("Delete 2 distributions", Some("delete 2 distributions")));
        assert!(lines.contains(&"Delete distribution one.".into()) && lines.contains(&"Delete distribution two.".into()), "{lines:?}");
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        assert_eq!(app.list.marked, [d2].into(), "only the failed one stays marked");
        assert!(app.notices.iter().any(|n| n.level == Level::Error && n.text.starts_with("delete failed (still marked): two:")));
    }

    #[tokio::test]
    async fn delete_of_a_server_task_refuses_an_unfinished_one() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        let task = format!("{API}tasks/5/");
        wiremock::Mock::given(method("DELETE"))
            .and(path(task.as_str()))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        get(&server, &format!("{API}tasks/"), page(json!([]))).await;
        let (mut app, mut rx) = connected(&server).await;
        app.list.kind = Kind::ServerTasks(vec![]);
        assert!(app.list.kind.actions().contains(&Act::Delete));
        for state in ["waiting", "running", "canceling"] {
            run(&mut app, Act::Delete, json!({ "pulp_href": task, "name": "sync", "state": state }));
            assert!(app.modal.is_none(), "{state}");
            let e = app.notices.back().map(|n| n.text.clone()).unwrap_or_default();
            assert_eq!(e, format!("task sync is {state}: only a finished task can be deleted (cancel it first)"));
        }
        run(&mut app, Act::Delete, json!({ "pulp_href": task, "name": "sync", "state": "failed" }));
        let Some(Modal::Confirm { lines, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm") };
        assert_eq!(lines[0], format!("Delete the record of task sync ({task}, failed) on Pulp."));
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        assert_eq!(app.notices.back().map(|n| n.text.as_str()), Some("delete task sync: done"));

        // A server without DELETE on a task.
        let mut caps = plugins();
        caps.load_openapi(&json!({ "paths": { "{task_href}": { "get": {}, "patch": {} } } }));
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        run(&mut app, Act::Delete, json!({ "pulp_href": task, "name": "sync", "state": "failed" }));
        assert!(app.modal.is_none());
        assert_eq!(app.notices.back().map(|n| n.text.as_str()), Some("not supported by this server: task delete (DELETE on a task)"));
    }

    #[tokio::test]
    async fn workers_screen_lists_the_workers_read_only_and_needs_the_endpoint() {
        use wiremock::matchers::{method, path, query_param};
        let server = wiremock::MockServer::start().await;
        let w = json!({ "pulp_href": format!("{API}workers/1/"), "name": "411@pulp1", "last_heartbeat": "2026-11-01T19:34:54.468059Z",
            "current_task": format!("{API}tasks/0190abcd-1234-5678/") });
        wiremock::Mock::given(method("GET"))
            .and(path(format!("{API}workers/")))
            .and(query_param("ordering", "name"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(page(json!([w]))))
            .mount(&server)
            .await;
        let (mut app, mut rx) = connected(&server).await;
        assert_eq!(Kind::TaskGroups.toggle(), Some(Kind::Workers), "t on Task groups");
        app.open(Screen::List(Kind::Workers));
        settle(&mut app, &mut rx).await;
        let cols: Vec<(&str, String)> = Kind::Workers.columns().iter().map(|c| (c.title, (c.get)(&app.list.items[0]))).collect();
        assert_eq!(
            cols,
            [("name", "411@pulp1".into()), ("last heartbeat", "2026-11-01 19:34:54".into()), ("current task", "0190abcd-1234".into())]
        );
        assert_eq!(Kind::Workers.actions(), [Act::Inspect], "read-only");
        assert!(Kind::Workers.searchable());

        let mut caps = plugins();
        caps.paths.insert("tasks/".into());
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        app.reload();
        assert_eq!(app.list.error.as_deref(), Some("not supported by this server: workers API (workers/)"));
    }

    #[tokio::test]
    async fn task_groups_screen_lists_counts_and_cancels_a_group() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{body_json, method, path, query_param};
        let server = wiremock::MockServer::start().await;
        let group = format!("{API}task-groups/1/");
        let g = json!({ "pulp_href": group, "description": "Replication of up", "waiting": 1, "running": 2, "completed": 3,
            "failed": 0, "canceled": 0, "skipped": 0 });
        wiremock::Mock::given(method("GET"))
            .and(path(format!("{API}task-groups/")))
            .and(query_param("exclude_fields", "tasks"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(page(json!([g]))))
            .mount(&server)
            .await;
        let mut full = g.clone();
        full["tasks"] = json!([{ "state": "waiting" }, { "state": "running" }, { "state": "running" }, { "state": "completed" }]);
        get(&server, &group, full).await;
        wiremock::Mock::given(method("PATCH"))
            .and(path(group.as_str()))
            .and(body_json(json!({ "state": "canceled" })))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(g.clone()))
            .expect(1)
            .mount(&server)
            .await;
        let (mut app, mut rx) = connected(&server).await;
        let order: Vec<Kind> = std::iter::successors(Some(Kind::Tasks), |k| k.toggle().filter(|n| *n != Kind::Tasks)).collect();
        assert_eq!(order, [Kind::Tasks, Kind::ServerTasks(vec![]), Kind::TaskGroups, Kind::Workers], "t goes round");
        app.open(Screen::List(Kind::TaskGroups));
        settle(&mut app, &mut rx).await;
        let row = app.list.items[0].clone();
        let cols: Vec<String> = Kind::TaskGroups.columns().iter().map(|c| format!("{}={}", c.title, (c.get)(&row))).collect();
        assert_eq!(cols, ["description=Replication of up", "waiting=1", "running=2", "completed=3", "failed=0", "canceled=0", "skipped=0"]);
        assert_eq!(Kind::TaskGroups.actions(), [Act::CancelTask, Act::Inspect]);
        assert!(!Kind::TaskGroups.searchable(), "task-groups/ has no filters");

        run(&mut app, Act::CancelTask, row);
        settle(&mut app, &mut rx).await; // the group is read before its confirmation
        let Some(Modal::Confirm { title, lines, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirmation") };
        assert_eq!(title, "Cancel task group");
        assert_eq!(lines[0], "Ask Pulp to cancel the whole task group Replication of up: 3 of 4 task(s) are not finished.");
        yes(&mut app);
        settle(&mut app, &mut rx).await;

        let mut caps = plugins();
        caps.paths.insert("tasks/".into());
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        app.reload();
        assert_eq!(app.list.error.as_deref(), Some("not supported by this server: task groups API (task-groups/)"));
    }

    /// Answer the open path input with `path`.
    fn type_path(app: &mut App, path: &str) {
        let Some(crate::tui::app::Modal::Input { on_done: Some(done), .. }) = app.modal.take() else { panic!("no path input") };
        done(app, path.into());
    }

    #[tokio::test]
    async fn servers_export_and_import_repository_definitions_as_the_cli_does() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        get(&server, &format!("{API}status/"), json!({ "versions": [{ "component": "rpm", "version": "3.29.0" }] })).await;
        let schemas = json!({ "rpm.RpmRemote": { "properties": { "name": {}, "url": {}, "headers": {}, "sles_auth_token": {} } },
            "rpm.RpmRepository": { "properties": { "name": {}, "description": {}, "remote": {} } } });
        get(&server, &format!("{API}docs/api.json"), json!({ "paths": {}, "components": { "schemas": schemas } })).await;
        let remote = json!({ "pulp_href": format!("{API}remotes/rpm/rpm/1/"), "name": "os", "url": "https://mirror.example/os/",
            "headers": [{ "Authorization": "Bearer secret-1" }], "sles_auth_token": "secret-2" });
        get(&server, &format!("{API}remotes/rpm/rpm/"), page(json!([remote]))).await;
        let repo = json!({ "pulp_href": format!("{API}repositories/rpm/rpm/1/"), "name": "app", "description": "d", "remote": format!("{API}remotes/rpm/rpm/1/") });
        get(&server, &format!("{API}repositories/rpm/rpm/"), page(json!([repo]))).await;
        wiremock::Mock::given(method("POST"))
            .and(path(format!("{API}remotes/rpm/rpm/")))
            .respond_with(wiremock::ResponseTemplate::new(201).set_body_json(json!({ "pulp_href": format!("{API}remotes/rpm/rpm/2/") })))
            .expect(1)
            .mount(&server)
            .await;
        let (mut app, mut rx) = connected(&server).await;
        let caps = Capabilities::detect(&app.conn.as_ref().unwrap().client).await.unwrap();
        app.conn.as_mut().unwrap().caps = Arc::new(caps.clone());
        app.list.kind = Kind::Profiles;
        let act = |key| Kind::Profiles.actions().into_iter().find(|a| a.key() == key).unwrap();
        let (export, import) = (act('X'), act('J'));
        assert!(!export.mutates(&Kind::Profiles, false) && import.mutates(&Kind::Profiles, false), "import creates objects on Pulp");

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("repos.json").display().to_string();
        run(&mut app, export, json!({ "name": "lab" }));
        type_path(&mut app, &file);
        settle(&mut app, &mut rx).await;
        let written = std::fs::read_to_string(&file).unwrap();
        let (cli, _) = crate::repo_export::export(&app.conn.as_ref().unwrap().client, &caps).await.unwrap();
        assert_eq!(written, serde_json::to_string_pretty(&cli).unwrap(), "the file of export-repos");
        assert!(!written.contains("secret-"), "no secret: {written}");
        let Some(Modal::Text { lines, .. }) = app.modal.take() else {
            panic!("no notes: {:?}", app.notices.iter().map(|n| &n.text).collect::<Vec<_>>())
        };
        assert!(lines.contains(&"note: RPM remote os: headers left out (it can hold a secret)".to_string()), "{lines:?}");

        // The same file plus a new remote: the plan comes first, the import after the confirmation.
        let mut ex = cli.clone();
        ex.remotes.push(crate::repo_export::Entry { name: "new-os".into(), ..ex.remotes[0].clone() });
        std::fs::write(&file, serde_json::to_vec(&ex).unwrap()).unwrap();
        run(&mut app, import, Value::Null);
        type_path(&mut app, &file);
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { title, lines, on_yes: Some(yes), .. }) = app.modal.take() else {
            panic!("no plan: {:?}", app.notices.iter().map(|n| &n.text).collect::<Vec<_>>())
        };
        assert_eq!(title, "Import repository definitions into lab");
        assert_eq!(lines[0], format!("Import {file} into lab: create 1, keep 2 existing (remotes and repositories)."));
        assert!(lines.contains(&"RPM remote new-os: create".to_string()), "{lines:?}");
        let posts = |reqs: Vec<wiremock::Request>| reqs.iter().filter(|r| r.method.as_str() == "POST").count();
        assert_eq!(posts(server.received_requests().await.unwrap()), 0, "the plan sends nothing");
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        let texts: Vec<&String> = app.notices.iter().map(|n| &n.text).collect();
        assert!(texts.contains(&&"profile lab: created 1, kept 2 existing (remotes and repositories)".to_string()), "{texts:?}");
    }

    #[tokio::test]
    async fn cancel_of_a_task_group_names_the_group_and_needs_the_server_cancel() {
        use crate::tui::app::Modal;
        let server = wiremock::MockServer::start().await;
        let group = format!("{API}task-groups/1/");
        let tasks = json!([{ "state": "completed" }, { "state": "running" }, { "state": "waiting" }]);
        let g =
            json!({ "pulp_href": group, "description": "Replication of up", "waiting": 1, "running": 1, "completed": 1, "tasks": tasks });
        get(&server, &group, g).await;
        let (mut app, mut rx) = connected(&server).await;
        task_action(&mut app, Act::CancelTask, json!({ "pulp_href": group }));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { title, lines, .. }) = app.modal.take() else { panic!("no confirmation") };
        assert_eq!(title, "Cancel task group");
        assert_eq!(lines[0], "Ask Pulp to cancel the whole task group Replication of up: 2 of 3 task(s) are not finished.");

        // An older pulpcore has no PATCH on a task group: refused before any request.
        let mut caps = plugins();
        caps.load_openapi(&json!({ "paths": { "{task_group_href}": { "get": {} } } }));
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        let asked = server.received_requests().await.unwrap().len();
        task_action(&mut app, Act::CancelTask, json!({ "pulp_href": group }));
        assert!(app.modal.is_none() && app.jobs.is_empty(), "a confirmation or job started");
        let e = app.notices.back().map(|n| n.text.clone()).unwrap_or_default();
        assert!(e.starts_with("not supported by this server: "), "{e}");
        assert_eq!(server.received_requests().await.unwrap().len(), asked);
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
    async fn sync_sends_optimize_and_skip_types_only_when_the_operator_sets_them() {
        use crate::tui::app::Modal;
        use wiremock::matchers::method;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(202).set_body_json(json!({ "task": format!("{API}tasks/1/") })))
            .mount(&server)
            .await;
        let remote = format!("{API}remotes/file/file/7/");
        get(&server, &format!("{API}remotes/file/file/"), page(json!([{ "pulp_href": remote, "name": "web" }]))).await;
        let (mut app, mut rx) = connected(&server).await;
        let none: &[(&str, &str)] = &[];
        let (rpm, deb) = (&["remote", "policy", "optimize", "skip_srpm", "skip_treeinfo"][..], &["remote", "policy", "optimize"][..]);
        let off = [("optimize", "false"), ("skip_srpm", "true")];
        for (t, set, fields, want) in [
            (RepoType::Rpm, none, rpm, json!({ "sync_policy": "additive" })),
            (RepoType::Rpm, &off, rpm, json!({ "sync_policy": "additive", "optimize": false, "skip_types": ["srpm"] })),
            (RepoType::Deb, &off[..1], deb, json!({ "mirror": false, "optimize": false })),
            (RepoType::File, none, deb, json!({ "mirror": false })),
            (RepoType::File, &off[..1], deb, json!({ "mirror": false, "optimize": false })),
            // A remote picked by name is sent as its href.
            (RepoType::File, &[("remote", "web")], deb, json!({ "remote": remote, "mirror": false })),
        ] {
            let repo = format!("{API}{}r/", t.repositories());
            repo_action(&mut app, t, Act::Sync, json!({ "pulp_href": repo, "name": "os", "remote": null }));
            let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
            assert_eq!(keys(&form), fields, "{t:?}");
            for (k, v) in set {
                form.fields.iter_mut().find(|f| f.key == *k).unwrap().value = v.to_string();
            }
            submit(&mut app, &form).unwrap();
            settle(&mut app, &mut rx).await;
            assert_eq!(posts_to(&server, &format!("{repo}sync/")).await.last(), Some(&want), "{t:?} {set:?}");
        }
        // Pulp answers 400 to skip types with mirror_complete: the form refuses them and sends nothing.
        let repo = format!("{API}repositories/rpm/rpm/m/");
        repo_action(&mut app, RepoType::Rpm, Act::Sync, json!({ "pulp_href": repo, "name": "os", "remote": null }));
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
        for (k, v) in [("policy", "mirror_complete"), ("skip_srpm", "true")] {
            form.fields.iter_mut().find(|f| f.key == k).unwrap().value = v.into();
        }
        let e = "Pulp refuses skip types with mirror_complete: clear the Skip fields or choose another sync policy";
        assert_eq!(submit(&mut app, &form), Err(e.into()));
        assert!(app.modal.is_none(), "no mirror confirm");
        settle(&mut app, &mut rx).await;
        assert!(posts_to(&server, &format!("{repo}sync/")).await.is_empty());

        // A server whose sync schemas lack the fields (older plugin) shows none of them.
        let mut caps = plugins();
        for s in ["RpmRepositorySyncURL", "AptRepositorySyncURL", "FileRepositorySyncURL"] {
            caps.schemas.insert(s.into(), ["remote", "mirror", "sync_policy"].map(String::from).into());
        }
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        for t in [RepoType::Rpm, RepoType::Deb, RepoType::File] {
            repo_action(&mut app, t, Act::Sync, json!({ "pulp_href": "/r/", "name": "os", "remote": null }));
            let Some(Modal::Form { form, .. }) = app.modal.take() else { panic!("no form") };
            assert_eq!(keys(&form), ["remote", "policy"], "{t:?}");
        }
    }

    /// pulp_rpm `viewsets/repository.py` refuses these repository settings with a mirror policy (HTTP 400).
    #[tokio::test]
    async fn rpm_sync_form_refuses_mirror_policies_that_the_repository_settings_forbid() {
        use crate::tui::app::Modal;
        let server = wiremock::MockServer::start().await;
        let (mut app, _rx) = connected(&server).await;
        let autopublish = "Pulp refuses mirror_complete with autopublish: turn autopublish off (Edit) or choose another sync policy";
        let retain = "Pulp refuses mirror policies with retain_package_versions above 0: set it to 0 (Edit) or choose additive";
        for (settings, policy, want) in [
            (json!({ "autopublish": true }), "mirror_complete", Some(autopublish)),
            (json!({ "autopublish": true }), "mirror_content_only", None),
            (json!({ "retain_package_versions": 2 }), "mirror_complete", Some(retain)),
            (json!({ "retain_package_versions": 2 }), "mirror_content_only", Some(retain)),
            (json!({ "retain_package_versions": 0 }), "mirror_content_only", None),
        ] {
            let mut item = json!({ "pulp_href": format!("{API}repositories/rpm/rpm/r/"), "name": "os", "remote": null });
            item.as_object_mut().unwrap().extend(settings.as_object().unwrap().clone());
            repo_action(&mut app, RepoType::Rpm, Act::Sync, item);
            let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
            form.fields.iter_mut().find(|f| f.key == "policy").unwrap().value = policy.into();
            let got = submit(&mut app, &form);
            app.modal = None; // the mirror confirm of an accepted form
            assert_eq!(got, want.map_or(Ok(()), |e| Err(e.into())), "{settings} {policy}");
        }
        assert!(server.received_requests().await.unwrap().iter().all(|r| r.method.as_str() == "GET"), "a sync was sent");
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
    async fn distribution_hidden_checkpoint_and_repo_config_follow_the_schema_and_send_only_changes() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        let (d, rpm) = (format!("{API}distributions/file/file/1/"), format!("{API}distributions/rpm/rpm/"));
        for (verb, p) in [("PATCH", d.clone()), ("POST", rpm.clone())] {
            wiremock::Mock::given(method(verb))
                .and(path(p))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({ "pulp_href": d })))
                .mount(&server)
                .await;
        }
        let (mut app, mut rx) = connected(&server).await;
        let flags = |app: &mut App, t: RepoType| {
            distribution_action(app, t, Act::Create, Value::Null);
            let Some(Modal::Form { form, .. }) = app.modal.take() else { panic!("no form") };
            keys(&form).into_iter().filter(|k| ["hidden", "checkpoint", "generate_repo_config"].contains(&k.as_str())).collect::<Vec<_>>()
        };
        assert_eq!(flags(&mut app, RepoType::Rpm), ["hidden", "checkpoint", "generate_repo_config"]);
        assert_eq!(flags(&mut app, RepoType::File), ["hidden", "checkpoint"], "generate_repo_config is RPM only");
        let mut caps = Capabilities::default();
        caps.schemas.insert("file.FileDistribution".into(), ["name", "base_path"].map(String::from).into());
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        assert!(flags(&mut app, RepoType::File).is_empty(), "a server without the fields");
        app.conn.as_mut().unwrap().caps = Default::default();

        let set = |form: &mut Form, k: &str, v: &str| form.fields.iter_mut().find(|f| f.key == k).unwrap().value = v.into();
        let item = json!({ "pulp_href": d, "name": "docs", "base_path": "docs", "repository": "/r/", "hidden": true,
            "checkpoint": false, "pulp_labels": {} });
        distribution_action(&mut app, RepoType::File, Act::Edit, item);
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no edit form") };
        assert_eq!((form.get("hidden"), form.get("checkpoint")), ("true", "false"), "as stored");
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        set(&mut form, "hidden", "false");
        set(&mut form, "checkpoint", "true");
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        // Pulp refuses `checkpoint` without a repository binding (HTTP 400): say it before any request.
        set(&mut form, "mode", "publication of a repository version");
        set(&mut form, "repo", "r");
        let e = submit(&mut app, &form).unwrap_err();
        assert_eq!(e, "Checkpoint needs Serve = follow repository latest: Pulp serves the checkpoint publications of a repository");

        distribution_action(&mut app, RepoType::Rpm, Act::Create, Value::Null);
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
        for (k, v) in [("name", "el9"), ("base_path", "el9"), ("mode", "none"), ("generate_repo_config", "true")] {
            set(&mut form, k, v);
        }
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;

        let sent = server.received_requests().await.unwrap();
        let bodies: Vec<Value> = sent.iter().filter(|r| r.method.as_str() != "GET").map(|r| r.body_json().unwrap()).collect();
        let flag = |b: &Value| ["hidden", "checkpoint", "generate_repo_config"].map(|k| b.get(k).cloned());
        assert_eq!(flag(&bodies[0]), [None, None, None], "untouched edit: not sent");
        assert_eq!(flag(&bodies[1]), [Some(json!(false)), Some(json!(true)), None], "the changes");
        assert_eq!(flag(&bodies[2]), [None, None, Some(json!(true))], "create: only what is on");
        assert_eq!(bodies.len(), 3);
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
        let served = served_now(c, &Capabilities::default(), RepoType::Rpm, &json!({ "repository": repo })).await.unwrap().unwrap();
        assert_eq!((served.0, s(&served.1, "layout").as_str()), (format!("{repo}versions/2/"), "flat"));
        assert_eq!(served_now(c, &Capabilities::default(), RepoType::Rpm, &json!({ "name": "unbound" })).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_version_binding_serves_the_newest_publication_of_that_version() {
        use wiremock::matchers::{method, path, query_param};
        let server = wiremock::MockServer::start().await;
        let v = format!("{API}repositories/file/file/r/versions/2/");
        let pubs = page(json!([
            { "pulp_href": "/p/old/", "repository_version": v, "pulp_created": "2026-01-01T00:00:00Z", "manifest": "OLD" },
            { "pulp_href": "/p/new/", "repository_version": v, "pulp_created": "2026-01-02T00:00:00Z", "manifest": "CUSTOM" },
        ]));
        wiremock::Mock::given(method("GET"))
            .and(path(format!("{API}publications/file/file/")))
            .and(query_param("repository_version", v.as_str()))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(pubs))
            .mount(&server)
            .await;
        let (app, _rx) = connected(&server).await;
        let c = &app.conn.as_ref().unwrap().client;
        let (version, p, how) =
            served_now(c, &Capabilities::default(), RepoType::File, &json!({ "repository_version": v })).await.unwrap().unwrap();
        assert_eq!((version, s(&p, "manifest"), how.as_str()), (v.clone(), "CUSTOM".into(), "pinned to repository version"));
    }

    #[tokio::test]
    async fn verify_compares_the_manifest_that_the_distribution_serves() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        let (repo, dist) = (format!("{API}repositories/file/file/r/"), format!("{API}distributions/file/file/d/"));
        let base = format!("{}/pulp/content/docs/", server.uri());
        get(&server, &dist, json!({ "pulp_href": dist, "name": "docs", "base_url": base, "repository": repo })).await;
        // The repository was published with its custom manifest name.
        let p = json!({ "pulp_href": "/p/1/", "repository_version": format!("{repo}versions/1/"), "pulp_created": "2026-01-01T00:00:00Z", "manifest": "CUSTOM" });
        get(&server, &format!("{API}publications/file/file/"), page(json!([p]))).await;
        get(
            &server,
            &format!("{API}content/file/files/"),
            page(json!([{ "pulp_href": "/f/a/", "relative_path": "a.txt", "sha256": "ab" }])),
        )
        .await;
        wiremock::Mock::given(method("GET"))
            .and(path("/pulp/content/docs/CUSTOM"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("a.txt,ab,3\n"))
            .mount(&server)
            .await;
        let (mut app, mut rx) = connected(&server).await;
        distribution_action(&mut app, RepoType::File, Act::Verify, json!({ "pulp_href": dist, "name": "docs" }));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Text { title, lines, .. }) = &app.modal else { panic!("no result: {:?}", app.notices.back().map(|n| &n.text)) };
        assert_eq!(title, "Verify docs");
        assert_eq!(lines.first().map(String::as_str), Some("docs: follows repository r: newest publication 1"), "{lines:?}");
        assert_eq!(lines.last().map(String::as_str), Some("OK: CUSTOM matches 1 files"), "{lines:?}");
    }

    #[tokio::test]
    async fn deb_verify_expects_a_newer_verbatim_publication_of_a_followed_repository() {
        use crate::tui::app::Modal;
        let server = wiremock::MockServer::start().await;
        let (repo, dist) = (format!("{API}repositories/deb/apt/r/"), format!("{API}distributions/deb/apt/d/"));
        get(&server, &dist, json!({ "pulp_href": dist, "name": "apt", "base_url": "http://h/apt/", "repository": repo })).await;
        let apt =
            json!({ "pulp_href": "/apt/1/", "repository_version": format!("{repo}versions/1/"), "pulp_created": "2026-01-02T00:00:00Z" });
        get(&server, &format!("{API}publications/deb/apt/"), page(json!([apt]))).await;
        // pulpcore picks from all Publication types: the verbatim publication of v2 is served.
        let verbatim = json!({ "pulp_href": "/verbatim/2/", "repository_version": format!("{repo}versions/2/"), "pulp_created": "2026-01-01T00:00:00Z" });
        get(&server, &format!("{API}publications/deb/verbatim/"), page(json!([verbatim]))).await;
        let (mut app, mut rx) = connected(&server).await;
        distribution_action(&mut app, RepoType::Deb, Act::Verify, json!({ "pulp_href": dist, "name": "apt" }));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Text { lines, .. }) = &app.modal else { panic!("no result: {:?}", app.notices.back().map(|n| &n.text)) };
        assert!(lines.contains(&"expected content: r@v2".to_string()), "{lines:?}");
    }

    #[tokio::test]
    async fn verify_of_a_version_without_a_publication_says_nothing_is_served() {
        let server = wiremock::MockServer::start().await;
        let dist = format!("{API}distributions/file/file/d/");
        let (v, base) = (format!("{API}repositories/file/file/r/versions/3/"), format!("{}/pulp/content/docs/", server.uri()));
        get(&server, &dist, json!({ "pulp_href": dist, "name": "docs", "base_url": base, "repository_version": v })).await;
        // pulp_file, pulp_rpm and pulp_deb serve only publications: every path of this distribution is 404.
        get(&server, &format!("{API}publications/file/file/"), page(json!([]))).await;
        let (mut app, mut rx) = connected(&server).await;
        distribution_action(&mut app, RepoType::File, Act::Verify, json!({ "pulp_href": dist, "name": "docs" }));
        settle(&mut app, &mut rx).await;
        assert!(app.modal.is_none(), "no OK result");
        let e = app.notices.iter().find(|n| n.level == Level::Error).expect("no error").text.clone();
        assert!(e.contains("distribution serves nothing"), "{e}");
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

    /// Pulp counts every unit of a version: a compare that lists fewer says so, not only `+0 added`.
    #[tokio::test]
    async fn compare_warns_when_pulp_counts_units_it_does_not_list() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path, path_regex, query_param};
        let server = wiremock::MockServer::start().await;
        let r = format!("{API}repositories/rpm/rpm/r/");
        let (v1, v2) = (format!("{r}versions/1/"), format!("{r}versions/2/"));
        let pkg = json!({ "pulp_href": "/p/", "name": "bash", "epoch": "0", "version": "5.2", "release": "1", "arch": "x86_64" });
        wiremock::Mock::given(method("GET"))
            .and(path(format!("{API}content/rpm/packages/")))
            .and(query_param("repository_version", v2.as_str()))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(page(json!([pkg]))))
            .mount(&server)
            .await;
        wiremock::Mock::given(method("GET"))
            .and(path_regex("/content/rpm/"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(page(json!([]))))
            .mount(&server)
            .await;
        let count = |n: u64| json!({ "count": n, "href": "/c/" });
        get(&server, &v1, json!({ "pulp_href": v1, "number": 1, "content_summary": { "present": {} } })).await;
        let present = json!({ "rpm.package": count(1), "rpm.unknown": count(2) });
        get(&server, &v2, json!({ "pulp_href": v2, "number": 2, "content_summary": { "present": present } })).await;
        let (mut app, mut rx) = connected(&server).await;
        app.list.marked = [v1.clone()].into();
        version_action(&mut app, Act::Compare, json!({ "pulp_href": v2, "number": 2 }));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Text { lines, .. }) = &app.modal else { panic!("no comparison") };
        assert_eq!(
            lines[..4],
            [
                "r@v1 -> r@v2: +1 added, -0 removed",
                "warning: Pulp counts 3 units in r@v2, this compare lists 1: the difference can be incomplete",
                "",
                "+ package bash-5.2-1.x86_64"
            ]
        );
    }

    /// An older plugin can lack a content endpoint: compare the other types and name the skipped one.
    #[tokio::test]
    async fn compare_skips_a_content_type_that_the_server_lacks() {
        use crate::tui::app::Modal;
        use wiremock::matchers::{method, path_regex};
        let server = wiremock::MockServer::start().await;
        let r = format!("{API}repositories/rpm/rpm/r/");
        let (v1, v2) = (format!("{r}versions/1/"), format!("{r}versions/2/"));
        wiremock::Mock::given(method("GET"))
            .and(path_regex("/content/rpm/modulemd_obsoletes/"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .with_priority(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(method("GET"))
            .and(path_regex("/content/rpm/"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(page(json!([]))))
            .mount(&server)
            .await;
        for (v, n) in [(&v1, 1), (&v2, 2)] {
            get(&server, v, json!({ "pulp_href": v, "number": n, "content_summary": { "present": {} } })).await;
        }
        let (mut app, mut rx) = connected(&server).await;
        let mut caps = plugins();
        let paths: serde_json::Map<String, Value> = RepoType::Rpm
            .content_types()
            .iter()
            .filter(|ct| ct.path != "content/rpm/modulemd_obsoletes/")
            .map(|ct| (format!("{API}{}", ct.path), json!({})))
            .collect();
        caps.load_openapi(&json!({ "paths": paths }));
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        app.list.marked = [v1.clone()].into();
        version_action(&mut app, Act::Compare, json!({ "pulp_href": v2, "number": 2 }));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Text { lines, .. }) = &app.modal else { panic!("no comparison: {:?}", app.notices.back().map(|n| &n.text)) };
        assert_eq!(lines[..2], ["r@v1 -> r@v2: +0 added, -0 removed", "not compared (not on this server): modulemd obsoletes"]);
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

    async fn answer(server: &wiremock::MockServer, verb: &str, p: &str, status: u16, body: Value) {
        use wiremock::matchers::{method, path};
        let r = wiremock::ResponseTemplate::new(status).set_body_json(body);
        wiremock::Mock::given(method(verb)).and(path(p)).respond_with(r).mount(server).await;
    }

    fn set(f: &mut Form, k: &str, v: &str) {
        f.fields.iter_mut().find(|x| x.key == k).unwrap().value = v.into();
    }

    #[tokio::test]
    async fn acs_create_sends_the_remote_and_paths_and_a_refresh_ends_with_its_tasks() {
        use crate::tui::app::Modal;
        let server = wiremock::MockServer::start().await;
        let (remote, acs, group) = (format!("{API}remotes/rpm/rpm/1/"), format!("{API}acs/rpm/rpm/1/"), format!("{API}task-groups/1/"));
        get(&server, &format!("{API}remotes/rpm/rpm/"), page(json!([{ "pulp_href": remote, "name": "cdn" }]))).await;
        answer(&server, "POST", &format!("{API}acs/rpm/rpm/"), 201, json!({ "pulp_href": acs })).await;
        answer(&server, "POST", &format!("{acs}refresh/"), 202, json!({ "task_group": group })).await;
        // pulp_rpm never sets `all_tasks_dispatched` on a refresh group.
        let g = json!({ "pulp_href": group, "description": "Refreshing 2 alternate content source path(s).", "all_tasks_dispatched": false,
            "completed": 2, "tasks": [{ "state": "completed" }, { "state": "completed" }] });
        get(&server, &group, g).await;
        let (mut app, mut rx) = connected(&server).await;
        app.list.kind = Kind::Acs(RepoType::Deb);
        run(&mut app, Act::Create, Value::Null);
        let Some(Modal::Form { form, .. }) = app.modal.take() else { panic!("no form") };
        assert_eq!(keys(&form), ["name", "remote"], "pulp_deb refreshes the remote URL only");

        app.list.kind = Kind::Acs(RepoType::Rpm);
        run(&mut app, Act::Create, Value::Null);
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
        assert_eq!(keys(&form), ["name", "remote", "paths"]);
        assert!(form.fields[1].help.contains("on_demand"), "{}", form.fields[1].help);
        for (k, v) in [("name", "cdn-acs"), ("remote", "cdn"), ("paths", "rpm/, rpm2/")] {
            set(&mut form, k, v);
        }
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        let body = json!({ "name": "cdn-acs", "remote": remote, "paths": ["rpm/", "rpm2/"] });
        assert_eq!(posts_to(&server, &format!("{API}acs/rpm/rpm/")).await, [body]);

        run(&mut app, Act::Refresh, json!({ "pulp_href": acs, "name": "cdn-acs" }));
        settle(&mut app, &mut rx).await;
        assert_eq!(posts_to(&server, &format!("{acs}refresh/")).await, [json!({})]);
        let row = |app: &App| app.tasks.items.iter().find(|t| t.href == group).map(|t| (t.label.clone(), t.state.clone()));
        assert_eq!(row(&app), Some(("refresh ACS cdn-acs".into(), "waiting".into())), "the Task Center tracks the group");
        app.tick();
        let wait = async {
            while !app.tasks.items.iter().any(|t| t.href == group && t.is_final()) {
                app.handle_msg(rx.recv().await.unwrap());
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), wait).await.expect("the group never ended");
        assert_eq!(row(&app), Some(("refresh ACS cdn-acs".into(), "completed".into())));
        assert!(app.notices.iter().any(|n| n.level == Level::Ok && n.text == "task completed: refresh ACS cdn-acs"));
    }

    #[tokio::test]
    async fn rpm_comps_and_advisory_uploads_send_the_file_and_report_the_new_version() {
        use crate::tui::app::Modal;
        let server = wiremock::MockServer::start().await;
        let repo = format!("{API}repositories/rpm/rpm/r/");
        for (n, coll, made) in
            [(1, "rpm/comps/", "content/rpm/packagegroups/9/"), (2, "content/rpm/advisories/", "content/rpm/advisories/9/")]
        {
            answer(&server, "POST", &format!("{API}{coll}"), 202, json!({ "task": format!("{API}tasks/{n}/") })).await;
            let created = json!([format!("{API}{made}"), format!("{repo}versions/{}/", n + 1)]);
            get(
                &server,
                &format!("{API}tasks/{n}/"),
                json!({ "pulp_href": format!("{API}tasks/{n}/"), "state": "completed", "created_resources": created }),
            )
            .await;
        }
        let dir = tempfile::tempdir().unwrap();
        let (comps, advisory) = (dir.path().join("comps.xml"), dir.path().join("errata.json"));
        std::fs::write(&comps, "<comps></comps>").unwrap();
        std::fs::write(&advisory, r#"{"id": "X-1"}"#).unwrap();
        let (mut app, mut rx) = connected(&server).await;
        let item = json!({ "pulp_href": repo, "name": "os" });
        let last = |app: &App| app.notices.back().map(|n| n.text.clone()).unwrap_or_default();
        for t in [RepoType::File, RepoType::Deb] {
            assert!(!Kind::Repos(t, false).actions().iter().any(|a| matches!(a, Act::UploadComps | Act::UploadAdvisory)), "{t:?}");
        }
        let rpm = Kind::Repos(RepoType::Rpm, false).actions();
        assert!(rpm.contains(&Act::UploadComps) && rpm.contains(&Act::UploadAdvisory));
        assert!(Act::UploadComps.mutates(&Kind::Repos(RepoType::Rpm, false), false));
        assert!(Act::UploadAdvisory.mutates(&Kind::Repos(RepoType::Rpm, false), false));

        repo_action(&mut app, RepoType::Rpm, Act::UploadComps, item.clone());
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
        assert_eq!(keys(&form), ["path", "replace"]);
        set(&mut form, "path", &dir.path().join("none.xml").display().to_string());
        assert!(submit(&mut app, &form).unwrap_err().contains("is not a readable file"));
        set(&mut form, "path", &comps.display().to_string());
        set(&mut form, "replace", "true");
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        assert_eq!(last(&app), "upload comps.xml to os: new version 2");

        repo_action(&mut app, RepoType::Rpm, Act::UploadAdvisory, item.clone());
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
        assert_eq!(keys(&form), ["path"]);
        set(&mut form, "path", &advisory.display().to_string());
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        assert_eq!(last(&app), "upload advisory to os: new version 3");

        let sent = server.received_requests().await.unwrap();
        let body = |p: &str| {
            let r = sent.iter().find(|r| r.method.as_str() == "POST" && r.url.path() == format!("{API}{p}")).expect(p);
            String::from_utf8_lossy(&r.body).replace("\r\n", "\n")
        };
        let field = |k: &str, v: &str| format!("name=\"{k}\"\n\n{v}\n");
        let c = body("rpm/comps/");
        assert!(c.contains(&field("repository", &repo)) && c.contains(&field("replace", "true")), "{c}");
        assert!(c.contains("filename=\"comps.xml\"") && c.contains("<comps></comps>"), "{c}");
        let a = body("content/rpm/advisories/");
        assert!(a.contains(&field("repository", &repo)) && !a.contains("replace") && a.contains(r#"{"id": "X-1"}"#), "{a}");

        // A server without the upload endpoint: refused before the form.
        let mut caps = plugins();
        caps.load_openapi(&json!({ "paths": { "/pulp/api/v3/content/rpm/advisories/": { "get": {} } } }));
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        for (act, want) in [
            (Act::UploadComps, "not supported by this server: comps.xml upload (rpm/comps/)"),
            (Act::UploadAdvisory, "not supported by this server: advisory upload (content/rpm/advisories/)"),
        ] {
            repo_action(&mut app, RepoType::Rpm, act, item.clone());
            assert!(app.modal.is_none(), "{act:?}");
            assert_eq!(last(&app), want);
        }
    }

    #[tokio::test]
    async fn reclaim_space_shows_the_remote_and_kept_versions_before_the_typed_confirm() {
        use crate::tui::app::Modal;
        let server = wiremock::MockServer::start().await;
        let (repo, remote) = (format!("{API}repositories/rpm/rpm/r/"), format!("{API}remotes/rpm/rpm/up/"));
        let item = json!({ "pulp_href": repo, "name": "os", "remote": remote, "latest_version_href": format!("{repo}versions/3/") });
        get(&server, &repo, item.clone()).await;
        get(&server, &remote, json!({ "pulp_href": remote, "name": "up", "policy": "immediate" })).await;
        get(&server, &format!("{repo}versions/"), page(json!([{ "number": 3 }, { "number": 2 }, { "number": 1 }, { "number": 0 }]))).await;
        answer(&server, "POST", &format!("{API}repositories/reclaim_space/"), 202, json!({ "task": format!("{API}tasks/1/") })).await;
        let reports = json!([{ "code": "reclaim-space.artifact", "message": "Reclaim disk space", "total": 5, "done": 5 }]);
        get(
            &server,
            &format!("{API}tasks/1/"),
            json!({ "pulp_href": format!("{API}tasks/1/"), "state": "completed", "progress_reports": reports }),
        )
        .await;
        let (mut app, mut rx) = connected(&server).await;
        let k = Kind::Repos(RepoType::Rpm, false);
        app.list.kind = k.clone();
        assert!(k.actions().contains(&Act::Reclaim) && Act::Reclaim.mutates(&k, false));
        let last = |app: &App| app.notices.back().map(|n| n.text.clone()).unwrap_or_default();
        let reclaim = |app: &mut App, keep: &str| {
            run(app, Act::Reclaim, item.clone());
            let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
            assert_eq!(keys(&form), ["keep"]);
            set(&mut form, "keep", keep);
            submit(app, &form)
        };
        assert_eq!(reclaim(&mut app, "x"), Err("Keep versions: version numbers, e.g. 3,4".into()));
        reclaim(&mut app, "2, 9").unwrap();
        settle(&mut app, &mut rx).await;
        assert!(app.modal.is_none());
        assert!(last(&app).ends_with("refused: os has no version 9"), "{}", last(&app));

        let posts = async || posts_to(&server, &format!("{API}repositories/reclaim_space/")).await;
        for (keep, kept, body) in [
            (
                "2",
                "Versions that keep their files: 2",
                json!({ "repo_hrefs": [repo], "repo_versions_keeplist": [format!("{repo}versions/2/")] }),
            ),
            ("", "No version keeps its files.", json!({ "repo_hrefs": [repo] })),
        ] {
            reclaim(&mut app, keep).unwrap();
            settle(&mut app, &mut rx).await;
            let Some(Modal::Confirm { typed, lines, on_yes: Some(yes), .. }) = app.modal.take() else {
                panic!("no confirm: {}", last(&app))
            };
            assert_eq!(typed.as_deref(), Some("os"));
            for want in ["in all 4 versions", "up (policy immediate)", kept] {
                assert!(lines.iter().any(|l| l.contains(want)), "{want}: {lines:?}");
            }
            let n = posts().await.len();
            yes(&mut app);
            settle(&mut app, &mut rx).await;
            assert_eq!(posts().await[n..], [body]);
            assert_eq!(last(&app), "reclaim space of os: deleted 5 file(s)");
        }

        let snapshot = json!({ "pulp_href": repo, "name": "s", "pulp_labels": { "pulp_tui_cv": "web" } });
        run(&mut app, Act::Reclaim, snapshot);
        assert!(app.modal.is_none() && last(&app).starts_with("refused: s is a snapshot repository"), "{}", last(&app));
        let mut caps = plugins();
        caps.load_openapi(&json!({ "paths": { "/pulp/api/v3/repositories/": {} } }));
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        run(&mut app, Act::Reclaim, item.clone());
        assert!(app.modal.is_none());
        assert_eq!(last(&app), "not supported by this server: reclaim space (repositories/reclaim_space/)");
    }

    /// pulpcore 3.123 repairs every artifact of the domain for a version without content: such a version is refused.
    #[tokio::test]
    async fn repair_checks_the_version_first_and_reports_what_pulp_repaired() {
        use crate::tui::app::Modal;
        let server = wiremock::MockServer::start().await;
        let repo = format!("{API}repositories/rpm/rpm/r/");
        let (empty, full) = (format!("{repo}versions/0/"), format!("{repo}versions/2/"));
        get(&server, &empty, json!({ "pulp_href": empty, "number": 0, "content_summary": { "present": {} } })).await;
        let present = json!({ "rpm.package": { "count": 3, "href": "" } });
        get(&server, &full, json!({ "pulp_href": full, "number": 2, "content_summary": { "present": present } })).await;
        answer(&server, "POST", &format!("{full}repair/"), 202, json!({ "task": format!("{API}tasks/1/") })).await;
        let reports = json!([
            { "code": "repair.missing", "message": "Identify missing units", "done": 1 },
            { "code": "repair.corrupted", "message": "Identify corrupted units", "done": 1 },
            { "code": "repair.repaired", "message": "Repair corrupted units", "done": 1 },
        ]);
        get(
            &server,
            &format!("{API}tasks/1/"),
            json!({ "pulp_href": format!("{API}tasks/1/"), "state": "completed", "progress_reports": reports }),
        )
        .await;
        let (mut app, mut rx) = connected(&server).await;
        app.list.kind = Kind::Versions(None);
        assert!(Kind::Versions(None).actions().contains(&Act::Repair) && Act::Repair.mutates(&Kind::Versions(None), false));
        let last = |app: &App| app.notices.back().map(|n| (n.level, n.text.clone())).unwrap();
        let repair = |app: &mut App, v: &str, n: u64| {
            run(app, Act::Repair, json!({ "pulp_href": v, "number": n }));
            let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
            assert_eq!((keys(&form), form.get("verify_checksums")), (vec!["verify_checksums".to_string()], "true"), "Pulp's default");
            set(&mut form, "verify_checksums", "false");
            submit(app, &form).unwrap();
        };

        repair(&mut app, &empty, 0);
        settle(&mut app, &mut rx).await;
        assert!(app.modal.is_none());
        assert_eq!(
            last(&app).1,
            "impact of repairing version 0: refused: version 0 has no content; Pulp would repair every file of the server"
        );

        repair(&mut app, &full, 2);
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { title, lines, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm: {:?}", last(&app)) };
        assert_eq!(title, "Repair version 2");
        assert!(lines[0].contains("its 3 content unit(s)") && lines[1].contains("only that each file exists"), "{lines:?}");
        assert!(posts_to(&server, &format!("{full}repair/")).await.is_empty(), "nothing is sent before the confirm");
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        assert_eq!(posts_to(&server, &format!("{full}repair/")).await, [json!({ "verify_checksums": false })]);
        assert_eq!(last(&app), (Level::Warn, "repair version 2: 1 missing, 1 corrupt, 1 repaired; see the task for the others".into()));

        let mut caps = plugins();
        caps.load_openapi(&json!({ "paths": { "{file_file_repository_version_href}repair/": {} } }));
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        run(&mut app, Act::Repair, json!({ "pulp_href": full, "number": 2 }));
        assert!(app.modal.is_none());
        assert_eq!(last(&app).1, "not supported by this server: repository version repair ({rpm_rpm_repository_version_href}repair/)");
    }

    /// pulpcore 3.123 deletes a path that an edit removes from every ACS that has the same path.
    #[tokio::test]
    async fn acs_edit_names_the_other_sources_that_lose_a_removed_path() {
        use crate::tui::app::Modal;
        let server = wiremock::MockServer::start().await;
        let (remote, acs) = (format!("{API}remotes/rpm/rpm/1/"), format!("{API}acs/rpm/rpm/1/"));
        get(&server, &remote, json!({ "pulp_href": remote, "name": "cdn" })).await;
        let item = json!({ "pulp_href": acs, "name": "cdn-acs", "remote": remote, "paths": ["rpm/", "rpm2/"] });
        get(&server, &format!("{API}acs/rpm/rpm/"), page(json!([item, { "name": "other", "paths": ["rpm/"] }]))).await;
        get(&server, &format!("{API}acs/file/file/"), page(json!([{ "name": "files", "paths": ["iso/"] }]))).await;
        get(&server, &format!("{API}acs/deb/deb/"), page(json!([]))).await;
        answer(&server, "PATCH", &acs, 202, json!({ "task": format!("{API}tasks/1/") })).await;
        answer(&server, "DELETE", &acs, 202, json!({ "task": format!("{API}tasks/2/") })).await;
        let (mut app, mut rx) = connected(&server).await;
        app.conn.as_mut().unwrap().caps = Arc::new(plugins());
        app.list.kind = Kind::Acs(RepoType::Rpm);
        let patches = async |server: &wiremock::MockServer| -> Vec<Value> {
            let sent = server.received_requests().await.unwrap();
            sent.iter().filter(|r| r.method.as_str() == "PATCH").map(|r| r.body_json().unwrap()).collect()
        };
        let edit = async |app: &mut App, rx: &mut tokio::sync::mpsc::UnboundedReceiver<super::super::app::Msg>, k: &str, v: &str| {
            run(app, Act::Edit, item.clone());
            settle(app, rx).await; // the name of the remote is read first
            let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
            assert_eq!((form.get("remote"), form.get("paths")), ("cdn", "rpm/,rpm2/"), "as stored");
            set(&mut form, k, v);
            submit(app, &form).unwrap();
            settle(app, rx).await;
        };

        edit(&mut app, &mut rx, "name", "cdn-new").await;
        assert!(app.modal.is_none(), "no path removed: sent at once");
        assert_eq!(patches(&server).await, [json!({ "name": "cdn-new" })], "only what changed");

        edit(&mut app, &mut rx, "paths", "rpm2/").await;
        let Some(Modal::Confirm { lines, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no warning") };
        assert!(lines.iter().any(|l| l.contains("other (RPM): rpm/")), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("files") || l.contains("cdn-acs (")), "{lines:?}");
        assert_eq!(patches(&server).await.len(), 1, "nothing is sent before the confirm");
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        assert_eq!(patches(&server).await[1], json!({ "paths": ["rpm2/"] }));

        run(&mut app, Act::Delete, item.clone());
        let Some(Modal::Confirm { typed, lines, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm") };
        assert_eq!(typed.as_deref(), Some("cdn-acs"));
        assert!(lines.iter().any(|l| l.contains("2 path(s)")), "{lines:?}");
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        let sent = server.received_requests().await.unwrap();
        assert_eq!(sent.iter().filter(|r| r.method.as_str() == "DELETE" && r.url.path() == acs).count(), 1);
    }
}
