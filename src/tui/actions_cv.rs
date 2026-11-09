//! Content View, CV version and lifecycle environment actions.

use super::actions::{ap, caps, confirm_mirror, sync_policies, unsupported};
use super::app::{App, Ctx, Level, NO_PROFILE, unavailable};
use super::form::{Field, FieldKind, Form, PickItem, Source};
use super::kinds::{Act, Kind, s};
use crate::cv::{CvService, Drift, Fix, LeftOut, Mode, check_repo_clash};
use crate::filters::{format_rules, parse_rules};
use crate::pulp::ops::{self, OnTask};
use crate::pulp::{Mutation, PulpClient, RepoType};
use crate::store::{Binding, BindingRepo, Component, CvRepo, CvVersion, CvVersionRepo, Environment, LIBRARY, Store};
use anyhow::{anyhow, bail};
use futures::future::join_all;
use serde_json::Value;
use std::future::Future;
use std::sync::Arc;

pub fn run(app: &mut App, act: Act, item: Value) {
    let Some(profile) = app.profile.clone() else {
        app.error(NO_PROFILE);
        return;
    };
    let Some(scope) = cv_scope(app, &profile) else { return };
    match app.list.kind.clone() {
        Kind::ContentViews => cv_action(app, &scope, act, item),
        Kind::CvVersions { cv, name } => version_action(app, &scope, cv, name, act, item),
        Kind::Environments => env_action(app, &scope, act, item),
        _ => {}
    }
}

/// The Content View store; when the shared database is unavailable, an error notice and `None`.
pub fn cv_store(app: &mut App) -> Option<Arc<Store>> {
    match app.cv_store.clone() {
        Ok(st) => Some(st),
        Err(e) => {
            app.error(unavailable(e).to_string());
            None
        }
    }
}

/// The scope of profile `name` (`App::scope`) while the Content View store is available; else an error notice and `None`.
fn cv_scope(app: &mut App, name: &str) -> Option<String> {
    cv_store(app)?;
    match app.scope(name) {
        Ok(scope) => Some(scope),
        Err(e) => {
            app.error(e);
            None
        }
    }
}

fn local<F, Fut>(app: &mut App, label: &str, f: F)
where
    F: FnOnce(Arc<Store>) -> Fut,
    Fut: Future<Output = anyhow::Result<String>> + Send + 'static,
{
    let Some(st) = cv_store(app) else { return };
    let fut = f(st);
    let fut = async move {
        let msg = fut.await?;
        ap(move |app| {
            app.ok(msg);
            app.reload();
        })
    };
    app.spawn(label.into(), false, fut);
}

/// Definitions for the picked repositories. Filter rules are typed, so they need repositories of one type.
fn new_cv_repos(picked: &[PickItem], rules: &str) -> Result<Vec<CvRepo>, String> {
    if picked.is_empty() {
        return Err("select at least one repository".into());
    }
    let mut repos = vec![];
    for i in picked {
        let repo_type = RepoType::from_href(&i.value).ok_or_else(|| format!("unknown repository type for {}", i.label))?;
        repos.push(CvRepo { repo_href: i.value.clone(), repo_name: i.label.clone(), repo_type, filters: vec![] });
    }
    if rules.replace(';', "").trim().is_empty() {
        return Ok(repos);
    }
    let t = repos[0].repo_type;
    if repos.iter().any(|r| r.repo_type != t) {
        return Err("filter rules need repositories of one type; add repositories of different types separately".into());
    }
    let filters = parse_rules(rules, t)?;
    for r in &mut repos {
        r.filters = filters.clone();
    }
    Ok(repos)
}

/// Components with all versions of each (newest first), for the pin forms.
type Parts = Vec<(Component, Vec<CvVersion>)>;

async fn with_versions(st: &Store, components: Vec<Component>) -> anyhow::Result<Parts> {
    let mut out = vec![];
    for c in components {
        let versions = st.versions(c.cv_id).await?;
        out.push((c, versions));
    }
    Ok(out)
}

/// Choices of a pin: `latest`, then the published versions, newest first.
fn pin_choices(versions: &[CvVersion]) -> Vec<String> {
    let published = versions.iter().filter(|v| v.state == "published").map(|v| format!("v{}", v.number));
    std::iter::once("latest".to_string()).chain(published).collect()
}

/// The version a choice pins; `None` is `latest`.
fn pin_of(choice: &str, versions: &[CvVersion]) -> Option<i64> {
    versions.iter().find(|v| v.state == "published" && format!("v{}", v.number) == choice).map(|v| v.id)
}

/// The choice that shows a pin (a version id; `None` is `latest`).
fn pin_choice(pinned: Option<i64>, versions: &[CvVersion]) -> String {
    versions.iter().find(|v| v.state == "published" && Some(v.id) == pinned).map_or("latest".into(), |v| format!("v{}", v.number))
}

/// One Choice per component, keyed by the component's view id, set to its current pin.
fn pin_fields(parts: &Parts) -> Vec<Field> {
    let field = |(c, vs): &(Component, Vec<CvVersion>)| {
        Field::new(&c.cv_id.to_string(), &c.name, FieldKind::Choice(pin_choices(vs))).value(pin_choice(c.pinned, vs))
    };
    parts.iter().map(field).collect()
}

/// `(view id, pinned version)` of each component in a form made with `pin_fields`; `changed` keeps only the edited ones.
fn chosen_pins(f: &Form, parts: &Parts, changed: bool) -> Vec<(i64, Option<i64>)> {
    let key = |c: &Component| c.cv_id.to_string();
    parts.iter().filter(|(c, _)| !changed || f.changed(&key(c))).map(|(c, vs)| (c.cv_id, pin_of(f.get(&key(c)), vs))).collect()
}

/// Second step of "Add components": pin each new component, check the repositories, then add them.
fn pin_form(app: &mut App, composite: i64, name: String, parts: Parts) {
    let form = Form::new(format!("Pin components of {name}"), pin_fields(&parts));
    app.form(form, move |app, f| {
        let pins = chosen_pins(f, &parts, false);
        let new: Vec<(i64, String)> = parts.iter().map(|(c, _)| (c.cv_id, c.name.clone())).collect();
        local(app, "add components", async move |st| {
            // The definitions as they are now: the same repository twice, or one path segment twice, cannot be served.
            let mut repos = vec![];
            for c in st.components(composite).await? {
                repos.push((c.name, st.cv_repos(c.cv_id).await?));
            }
            for (id, name) in new {
                repos.push((name, st.cv_repos(id).await?));
            }
            check_repo_clash(&repos)?;
            for (id, pin) in &pins {
                st.set_component(composite, *id, *pin).await?;
            }
            Ok(format!("added {} component(s) (takes effect on next publish)", pins.len()))
        });
        Ok(())
    });
}

/// Run `go` after the typed confirmation of a protected Library (as a promotion to a protected environment
/// asks), else after the plain confirmation `title` (none: at once).
fn guard_library(app: &mut App, protected: bool, lines: Vec<String>, title: Option<String>, go: impl FnOnce(&mut App) + 'static) {
    match (protected, title) {
        (true, _) => app.confirm_danger(format!("{LIBRARY} is protected"), lines, LIBRARY, go),
        (false, Some(title)) => app.confirm(title, lines, go),
        (false, None) => go(app),
    }
}

/// The notice of a publish with `note` (why Library does not serve the version) and the results of the auto
/// publish that followed: a warning when there is a note or one failed.
fn auto_notice(msg: String, note: Option<String>, auto: Vec<Result<String, String>>) -> (Level, String) {
    let level = if note.is_some() || auto.iter().any(Result::is_err) { Level::Warn } else { Level::Ok };
    let results: Vec<String> = auto.into_iter().map(|r| r.unwrap_or_else(|e| e)).collect();
    let auto = (!results.is_empty()).then(|| format!("auto publish: {}", results.join("; ")));
    (level, [Some(msg), note, auto].into_iter().flatten().collect::<Vec<_>>().join("; "))
}

/// What happened to one repository of a "Sync repositories" run.
#[derive(Debug, PartialEq)]
enum RepoSync {
    /// The sync task (href) is running.
    Submitted(String),
    /// The task finished and created this repository version (href).
    NewVersion(String),
    NoChange,
    /// The error text: a rejected request, a failed, canceled or skipped task, or a task that could not be read.
    Failed(String),
    /// No default remote.
    Skipped,
}

async fn start_sync(c: &PulpClient, r: &CvRepo, policy: &str) -> RepoSync {
    match c.get::<Value>(&r.repo_href, &[("fields", "remote".into())]).await {
        Err(e) => RepoSync::Failed(e.to_string()),
        Ok(v) if !v["remote"].is_string() => RepoSync::Skipped,
        // No remote in the request: Pulp uses the default remote of the repository.
        Ok(_) => match ops::sync(c, r.repo_type, &r.repo_href, None, policy).await {
            Ok(Mutation::Task(h)) => RepoSync::Submitted(h),
            Ok(_) => RepoSync::Failed("server returned no task".into()),
            Err(e) => RepoSync::Failed(e.to_string()),
        },
    }
}

/// Send the syncs of all repositories at once. A failure of one does not stop the others; nothing is retried.
async fn start_syncs(c: &PulpClient, repos: &[CvRepo], policy: &str) -> Vec<(String, RepoSync)> {
    join_all(repos.iter().map(async |r| (r.repo_name.clone(), start_sync(c, r, policy).await))).await
}

async fn finish_sync(c: &PulpClient, state: RepoSync, on: OnTask<'_>) -> RepoSync {
    let RepoSync::Submitted(h) = state else { return state };
    match ops::wait_task(c, &h, on).await {
        Ok(t) => t.created_matching("/versions/").map_or(RepoSync::NoChange, RepoSync::NewVersion),
        // `TaskUnknown` says when reading the task failed, not the task: it may still run, or may be done.
        Err(e) => RepoSync::Failed(e.to_string()),
    }
}

/// Wait for all submitted syncs at once; the result of each comes from its finished task.
async fn finish_syncs(c: &PulpClient, rows: Vec<(String, RepoSync)>, on: OnTask<'_>) -> Vec<(String, RepoSync)> {
    join_all(rows.into_iter().map(async |(name, state)| (name, finish_sync(c, state, on).await))).await
}

fn sync_summary(rows: &[(String, RepoSync)]) -> String {
    let n = |p: fn(&RepoSync) -> bool| rows.iter().filter(|(_, r)| p(r)).count();
    let synced = n(|r| matches!(r, RepoSync::NewVersion(_) | RepoSync::NoChange));
    format!("{synced} synced, {} failed, {} skipped", n(|r| matches!(r, RepoSync::Failed(_))), n(|r| matches!(r, RepoSync::Skipped)))
}

fn sync_lines(rows: &[(String, RepoSync)]) -> Vec<String> {
    let mut lines = vec![sync_summary(rows), String::new()];
    for (name, r) in rows {
        lines.push(match r {
            RepoSync::NewVersion(v) => {
                format!(
                    "  {name}: synced, new version {}",
                    v.rsplit_once("/versions/").map_or(v.as_str(), |(_, n)| n).trim_end_matches('/')
                )
            }
            RepoSync::NoChange => format!("  {name}: synced, no change"),
            RepoSync::Failed(e) => format!("  {name}: failed: {e}"),
            RepoSync::Skipped => format!("  {name}: skipped: no default remote (set one with e on the repository)"),
            RepoSync::Submitted(_) => format!("  {name}: submitted, see Tasks"),
        });
    }
    lines.push(String::new());
    lines.push("Sync does not publish the view: publish a new version (P) to use the new content.".into());
    lines
}

/// The audit detail of a run: the submit result, so the rows are Submitted, Failed or Skipped.
fn sync_audit(cv: &str, policy: &str, rows: &[(String, RepoSync)]) -> String {
    let names = |p: fn(&RepoSync) -> bool| rows.iter().filter(|(_, r)| p(r)).map(|(n, _)| n.as_str()).collect::<Vec<_>>();
    let (sent, failed, skipped) = (
        names(|r| matches!(r, RepoSync::Submitted(_))),
        names(|r| matches!(r, RepoSync::Failed(_))),
        names(|r| matches!(r, RepoSync::Skipped)),
    );
    let list = |label: &str, v: &[&str]| if v.is_empty() { String::new() } else { format!("; {label}: {}", v.join(", ")) };
    let counts = format!("{} submitted, {} failed, {} skipped", sent.len(), failed.len(), skipped.len());
    format!("{cv} ({policy}): {counts}{}{}", list("failed", &failed), list("skipped", &skipped))
}

/// One audit entry per run, written when the syncs are submitted, so that other operators see it while the tasks run
/// and a quit during the wait does not lose it. An unavailable or failing store skips it.
async fn audit_sync(ctx: &Ctx, cv: &str, policy: &str, rows: &[(String, RepoSync)]) {
    if let Ok(svc) = ctx.cv() {
        svc.audit("cv.sync", &sync_audit(cv, policy, rows)).await
    }
}

/// Track the submitted tasks first (they survive Esc and a restart), then wait for them and show the result.
fn wait_syncs(app: &mut App, cv: String, policy: String, rows: Vec<(String, RepoSync)>) {
    let mut sent = 0;
    for (name, r) in &rows {
        if let RepoSync::Submitted(h) = r {
            app.track_task(h, &format!("sync {name} ({policy})"), None);
            sent += 1;
        }
    }
    if sent == 0 {
        return app.text(format!("Sync of {cv}"), sync_lines(&rows));
    }
    app.notify(Level::Info, format!("submitted {sent} syncs of {cv} (see Tasks)"));
    app.job(format!("wait for syncs of {cv}"), move |ctx| async move {
        let rows = finish_syncs(&ctx.client, rows, &ctx.on_task()).await;
        ap(move |app| app.text(format!("Sync of {cv}"), sync_lines(&rows)))
    });
}

async fn env_names(st: &Store, scope: &str) -> anyhow::Result<Vec<String>> {
    st.ensure_library(scope).await?;
    Ok(st.environments(scope).await?.into_iter().map(|e| e.name).collect())
}

fn cv_action(app: &mut App, scope: &str, act: Act, item: Value) {
    let id = item["id"].as_i64().unwrap_or_default();
    let name = s(&item, "name");
    let repos: Vec<String> = item["repos"].as_array().into_iter().flatten().map(|r| s(r, "name")).collect();
    let p = scope.to_string();
    match act {
        Act::Create => {
            let form = Form::new(
                "New Content View",
                vec![
                    Field::text("name", "Name").required().help("[A-Za-z0-9-_.], becomes part of base paths"),
                    Field::text("description", "Description"),
                    Field::new("composite", "Composite (fixed after create)", FieldKind::Bool),
                ],
            );
            app.form(form, move |app, f| {
                let (n, d, p, composite) = (f.get("name").to_string(), f.get("description").to_string(), p.clone(), f.bool("composite"));
                crate::store::validate_name(&n).map_err(|e| e.to_string())?;
                local(app, "create content view", async move |st| {
                    if composite {
                        st.create_composite(&p, &n, &d).await?;
                        Ok(format!("composite content view {n} created; add components with a"))
                    } else {
                        st.create_cv(&p, &n, &d).await?;
                        Ok(format!("content view {n} created; add repositories with a"))
                    }
                });
                Ok(())
            });
        }
        Act::Edit if item["composite"] == true => {
            let Some(st) = cv_store(app) else { return };
            app.spawn("read components".into(), true, async move {
                let cv = st.content_view(id).await?;
                let parts = with_versions(&st, st.components(id).await?).await?;
                ap(move |app| {
                    let mut fields = vec![
                        Field::text("description", "Description").value(cv.description),
                        Field::new("auto", "Auto publish when a latest component publishes", FieldKind::Bool)
                            .value(cv.auto_publish.to_string()),
                    ];
                    fields.extend(pin_fields(&parts));
                    app.form(Form::new(format!("Edit {name}"), fields), move |app, f| {
                        let (d, auto, pins) = (f.get("description").to_string(), f.bool("auto"), chosen_pins(f, &parts, true));
                        local(app, "update content view", async move |st| {
                            st.update_cv(id, &d).await?;
                            st.set_auto_publish(id, auto).await?;
                            for (component, pin) in pins {
                                st.set_component(id, component, pin).await?;
                            }
                            Ok("saved".into())
                        });
                        Ok(())
                    });
                })
            });
        }
        Act::Edit => {
            let form = Form::new(format!("Edit {name}"), vec![Field::text("description", "Description").value(s(&item, "description"))]);
            app.form(form, move |app, f| {
                let d = f.get("description").to_string();
                local(app, "update content view", async move |st| {
                    st.update_cv(id, &d).await?;
                    Ok("saved".into())
                });
                Ok(())
            });
        }
        Act::CvAddRepo => {
            // Repositories already in the view are not listed: adding them again would overwrite their filters.
            let present = item["repos"].as_array().into_iter().flatten().map(|r| s(r, "href")).collect();
            let form = Form::new(
                format!("Add repositories to {name}"),
                vec![
                    Field::pick("repos", "Repositories", Source::Repos(None)).multi().required().exclude(present),
                    Field::text("filters", "Filter rules")
                        .help("rpm: include name=x* arch=.. evr>=1.0; exclude name=*-debuginfo | file: exclude path=isos/* | deb: exclude name=*-dbg version<1.0"),
                ],
            )
            .note("Each repository keeps its own type and distribution; formats are never merged.")
            .note("Filter rules apply to every selected repository, so they need repositories of one type.");
            app.form(form, move |app, f| {
                let repos = new_cv_repos(&f.picked("repos"), f.get("filters"))?;
                let name = name.clone();
                local(app, "add repositories", async move |st| {
                    let mut all = st.cv_repos(id).await?;
                    all.extend(repos.iter().cloned());
                    check_repo_clash(&[(name, all)])?;
                    for r in &repos {
                        st.set_cv_repo(id, r).await?;
                    }
                    let n = repos.len();
                    Ok(format!("added {n} {} (takes effect on next publish)", if n == 1 { "repository" } else { "repositories" }))
                });
                Ok(())
            });
        }
        Act::CvFilters | Act::CvRemoveRepo => {
            if repos.is_empty() {
                app.error("content view has no repositories");
                return;
            }
            let current: Vec<String> =
                item["repos"].as_array().into_iter().flatten().map(|r| format!("{}: {}", s(r, "name"), s(r, "filters"))).collect();
            let mut fields = vec![Field::new("repo", "Repository", FieldKind::Choice(repos))];
            if act == Act::CvFilters {
                fields.push(Field::text("filters", "New filter rules").help("empty = no filtering (all content)"));
            }
            let mut form = Form::new(
                if act == Act::CvFilters { format!("Filters of {name}") } else { format!("Remove repository from {name}") },
                fields,
            );
            for c in current {
                form = form.note(c);
            }
            let Some(st) = cv_store(app) else { return };
            app.spawn("read repositories".into(), true, async move {
                let defs = st.cv_repos(id).await?;
                ap(move |app| {
                    app.form(form, move |app, f| {
                        let r = defs.iter().find(|d| d.repo_name == f.get("repo")).cloned().ok_or("unknown repository")?;
                        if act == Act::CvRemoveRepo {
                            local(app, "remove repository", async move |st| {
                                st.remove_cv_repo(id, &r.repo_href).await?;
                                Ok(format!("{} removed from the definition; published versions are unchanged", r.repo_name))
                            });
                        } else {
                            let filters = parse_rules(f.get("filters"), r.repo_type)?;
                            local(app, "update filters", async move |st| {
                                let msg = format!("filters of {}: {}", r.repo_name, format_rules(&filters));
                                st.set_cv_repo(id, &CvRepo { filters, ..r }).await?;
                                Ok(msg)
                            });
                        }
                        Ok(())
                    })
                })
            });
        }
        Act::CvAddComponent => {
            // Views already in the composite are not listed: adding one again would only change its pin (Edit).
            let present = item["components"].as_array().into_iter().flatten().map(|c| s(c, "id")).collect();
            let form = Form::new(
                format!("Add components to {name}"),
                vec![Field::pick("components", "Content Views", Source::ContentViews).multi().required().exclude(present)],
            );
            app.form(form, move |app, f| {
                let picked: Vec<Component> = f
                    .picked("components")
                    .into_iter()
                    .filter_map(|i| Some(Component { cv_id: i.value.parse().ok()?, name: i.label, pinned: None }))
                    .collect();
                if picked.is_empty() {
                    return Err("pick Content Views from the list".into());
                }
                let Some(st) = cv_store(app) else { return Ok(()) };
                let name = name.clone();
                app.spawn("read versions".into(), true, async move {
                    let parts = with_versions(&st, picked).await?;
                    ap(move |app| pin_form(app, id, name, parts))
                });
                Ok(())
            });
        }
        Act::CvRemoveComponent => {
            let components: Vec<(i64, String)> =
                item["components"].as_array().into_iter().flatten().filter_map(|c| Some((c["id"].as_i64()?, s(c, "name")))).collect();
            if components.is_empty() {
                app.error("composite has no components");
                return;
            }
            let names = components.iter().map(|(_, n)| n.clone()).collect();
            let form =
                Form::new(format!("Remove component from {name}"), vec![Field::new("component", "Component", FieldKind::Choice(names))]);
            app.form(form, move |app, f| {
                let (component, c) = components.iter().find(|(_, n)| n == f.get("component")).cloned().ok_or("unknown component")?;
                let name = name.clone();
                local(app, "remove component", async move |st| {
                    st.remove_component(id, component).await?;
                    Ok(format!("{c} removed from {name}; published versions are unchanged"))
                });
                Ok(())
            });
        }
        Act::CvPreview => app.job(format!("preview {name}"), move |ctx| async move {
            let svc = ctx.cv()?;
            let latest = svc.store.versions(id).await?.into_iter().find(|v| v.state == "published").map(|v| v.id);
            let lines = svc.preview(id, latest).await?;
            ap(move |app| app.text(format!("Preview of {name} (draft vs latest published)"), lines))
        }),
        Act::CvPublish => {
            let Some(st) = cv_store(app) else { return };
            // The list row can be stale in a shared database: read the repositories now, as Resume does.
            app.spawn(format!("read repositories of {name}"), true, async move {
                let deb = st.cv_repos(id).await?.iter().any(|r| r.repo_type == RepoType::Deb);
                ap(move |app| cv_publish(app, id, name, deb))
            });
        }
        Act::CvVersions => app.push(Kind::CvVersions { cv: id, name }),
        Act::CvSyncRepos => {
            let items: Vec<PickItem> = item["repos"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|r| PickItem { label: s(r, "name"), value: s(r, "href"), note: s(r, "type") })
                .collect();
            if items.is_empty() {
                app.error("content view has no repositories");
                return;
            }
            let has = |t: &str| items.iter().any(|i| i.note == t);
            let Some(t) = RepoType::ALL.into_iter().find(|t| has(t.component())) else {
                app.error("unknown repository type");
                return;
            };
            let all = items.iter().map(|i| i.value.as_str()).collect::<Vec<_>>().join("\n");
            let mut form = Form::new(
                format!("Sync repositories of {name}"),
                vec![
                    Field::pick("repos", "Repositories", Source::Repos(None)).multi().required().value(all),
                    Field::new("policy", "Sync policy", FieldKind::Choice(sync_policies(caps(app).as_deref(), t))),
                ],
            )
            .note("Each repository syncs from its own default remote; one without a default remote is skipped.")
            .note("This only syncs. Publish a new version (P) afterwards to use the new content.");
            if has("rpm") && (has("file") || has("deb")) {
                form = form.note("File and Deb repositories: additive stays additive; any mirror policy means mirror.");
            }
            // All marked, so the picker needs no server request.
            form.set_options(0, Ok(items));
            app.form(form, move |app, f| {
                let repos = new_cv_repos(&f.picked("repos"), "")?;
                let (policy, name) = (f.get("policy").to_string(), name.clone());
                let hrefs = repos.iter().map(|r| r.repo_href.clone()).collect();
                let go = {
                    let policy = policy.clone();
                    move |app: &mut App| {
                        app.job(format!("sync {} repositories of {name}", repos.len()), move |ctx| async move {
                            let rows = start_syncs(&ctx.client, &repos, &policy).await;
                            audit_sync(&ctx, &name, &policy, &rows).await;
                            ap(move |app| wait_syncs(app, name, policy, rows))
                        })
                    }
                };
                confirm_mirror(app, &policy, hrefs, None, go);
                Ok(())
            });
        }
        Act::Promote | Act::Rollback => {
            let published: Vec<String> =
                item["versions"].as_array().into_iter().flatten().filter(|v| v["state"] == "published").map(|v| s(v, "number")).collect();
            if published.is_empty() {
                app.error("no published versions");
                return;
            }
            let mode = if act == Act::Promote { Mode::Promote } else { Mode::Rollback };
            let Some(st) = cv_store(app) else { return };
            app.spawn("read environments".into(), true, async move {
                let envs = env_names(&st, &p).await?;
                let versions = st.versions(id).await?;
                ap(move |app| {
                    let form = Form::new(
                        format!("{} {name}", if mode == Mode::Promote { "Promote" } else { "Roll back" }),
                        vec![
                            Field::new("version", "Version", FieldKind::Choice(published)),
                            Field::new("env", "Environment", FieldKind::Choice(envs)),
                        ],
                    );
                    app.form(form, move |app, f| {
                        let number: i64 = f.get("version").parse().map_err(|_| "pick a version")?;
                        let vid = versions.iter().find(|v| v.number == number).map(|v| v.id).ok_or("unknown version")?;
                        promote_preview(app, name.clone(), vid, f.get("env").to_string(), mode);
                        Ok(())
                    });
                })
            });
        }
        Act::Unpromote => {
            let Some(st) = cv_store(app) else { return };
            app.spawn("read environments".into(), true, async move {
                let has = st.cv_env_ids(id).await?;
                let envs: Vec<String> = st.environments(&p).await?.into_iter().filter(|e| has.contains(&e.id)).map(|e| e.name).collect();
                ap(move |app| {
                    if envs.is_empty() {
                        return app.error(format!("{name} is not in any environment"));
                    }
                    let form = Form::new(format!("Unpromote {name}"), vec![Field::new("env", "Environment", FieldKind::Choice(envs))]);
                    app.form(form, move |app, f| {
                        unpromote_preview(app, id, name.clone(), f.get("env").to_string());
                        Ok(())
                    });
                })
            });
        }
        Act::CvDrift => app.job(format!("verify {name}"), move |ctx| async move {
            let (svc, mut lines, mut hints) = (ctx.cv()?, vec![], vec![]);
            for e in svc.store.environments(&svc.scope).await? {
                if let Some(b) = svc.store.binding(e.id, id).await? {
                    let drift = svc.drift(id, e.id).await?;
                    // What `verify` reads (`ops::verify_served`): metadata, and one package of RPM and Deb.
                    let status = match (drift.is_empty(), drift.iter().all(|d| d.fix == Fix::Unchecked)) {
                        (true, _) => "OK - served metadata match; RPM/Deb: also the smallest package",
                        (false, true) => {
                            "OK - served metadata match; RPM/Deb: also the smallest package; File: no manifest compared for the \
                             repositories below"
                        }
                        (false, false) => "DRIFT",
                    };
                    lines.push(format!("{}: {} {status}", e.name, shown_binding(&svc.store, &b).await?));
                    hints.extend(reconcile_hints(&svc, &e, &b, &drift).await?);
                    lines.extend(drift.into_iter().map(|d| format!("    {}", d.text)));
                }
            }
            if lines.is_empty() {
                lines.push("not bound to any environment".into());
            } else if !hints.is_empty() {
                lines.push(String::new());
                lines.extend(hints);
            }
            ap(move |app| app.text(format!("Verification of {name}"), lines))
        }),
        Act::Delete => app.load_job(format!("read what deleting {name} removes"), move |ctx| async move {
            // A composite owns distributions only: its versions borrow the components' objects.
            let mut lines = if item["composite"] == true {
                vec![
                    format!("Delete composite content view {name}: its distributions in every environment (consumers lose these URLs)."),
                    "Component views and their snapshots are NOT touched.".into(),
                ]
            } else {
                vec![
                    format!("Delete content view {name} and everything pulp-tui created for it:"),
                    "  - its distributions in every environment (consumers lose these URLs)".into(),
                    "  - its snapshot repositories (with their versions and publications)".into(),
                    "Source repositories are NOT touched.".into(),
                ]
            };
            let svc = ctx.cv()?;
            svc.delete_view_impact(id).await?; // a refusal shows before the typed confirmation
            let envs = svc.store.environments(&svc.scope).await?;
            let mut protected: Vec<String> = vec![];
            lines.push(String::new());
            let served: Vec<_> = svc.store.all_binding_repos(id).await?.into_iter().filter(|b| b.publication.is_some()).collect();
            lines.push(if served.is_empty() { "Nothing is served now.".into() } else { "Served now:".into() });
            for b in served {
                let Some(e) = envs.iter().find(|e| e.id == b.env_id) else { continue };
                lines.push(format!("  {:<12} /{}{}", e.name, b.base_path, if e.protected { "  (protected)" } else { "" }));
                if e.protected && !protected.contains(&e.name) {
                    protected.push(e.name.clone());
                }
            }
            // Like a promotion to a protected environment, the environment is typed too.
            let typed = std::iter::once(name.clone()).chain(protected).collect::<Vec<_>>().join(" ");
            ap(move |app| {
                app.confirm_danger(format!("Delete content view {name}"), lines, &typed, move |app| {
                    app.job(format!("delete content view {name}"), move |ctx| async move {
                        ctx.cv()?.locked(id, "delete").await?.delete_view(id, &ctx.log(), &ctx.on_task()).await?;
                        ap(move |app| {
                            app.ok(format!("content view {name} deleted"));
                            app.reload();
                        })
                    });
                });
            })
        }),
        Act::BreakLock => app.job(format!("read lock of {name}"), move |ctx| async move {
            let Some(h) = ctx.cv()?.store.lock_holder(id).await? else {
                return ap(move |app| app.ok(format!("{name} is not locked")));
            };
            ap(move |app| {
                let lines = vec![
                    format!("Locked by {} ({}) since {}.", h.owner, h.operation, h.acquired_at),
                    "Break it only if that session is dead. Its workflow stops within 30 s.".into(),
                ];
                app.confirm_danger(format!("Break lock on {name}"), lines, &name.clone(), move |app| {
                    app.job(format!("break lock on {name}"), move |ctx| async move {
                        let svc = ctx.cv()?;
                        // Only the lock shown: another operator may hold it by now.
                        let res = if svc.store.break_lock(id, &h).await? {
                            svc.audit("cv.lock.break", &format!("{name}: {} ({}) since {}", h.owner, h.operation, h.acquired_at)).await;
                            Ok(format!("lock on {name} broken"))
                        } else {
                            Err(match svc.store.lock_holder(id).await? {
                                Some(n) => format!(
                                    "lock on {name} changed, not broken: now locked by {} ({}) since {}",
                                    n.owner, n.operation, n.acquired_at
                                ),
                                None => format!("{name} is not locked any more"),
                            })
                        };
                        ap(move |app| {
                            match res {
                                Ok(m) => app.ok(m),
                                Err(m) => app.error(m),
                            }
                            app.reload();
                        })
                    });
                });
            })
        }),
        _ => {}
    }
}

/// One line per kind of fix that the `drift` of the view in `e` (binding `b`) needs, each naming an action that fixes
/// that kind. While a workflow holds a live lock on the view (a promotion that runs), only that one line: the lock
/// refuses the actions, and the workflow can change what drifts.
async fn reconcile_hints(svc: &CvService, e: &Environment, b: &Binding, drift: &[Drift]) -> anyhow::Result<Vec<String>> {
    if drift.iter().any(|d| d.fix != Fix::Unchecked)
        && svc.store.lock_live(b.cv_id, crate::cv::unix_now()).await?
        && let Some(h) = svc.store.lock_holder(b.cv_id).await?
    {
        return Ok(vec![format!(
            "{}: {} by {} is running: verify again when it ends (its lock refuses Publish, Promote and Rollback until then).",
            e.name, h.operation, h.owner
        )]);
    }
    let mut out = vec![];
    for fix in [Fix::Bind, Fix::Publish, Fix::Repair, Fix::Origin, Fix::Retention] {
        if !drift.iter().any(|d| d.fix == fix) {
            continue;
        }
        let line = match fix_hint(fix) {
            Some(hint) => hint.to_string(),
            None => bind_hint(svc, e, b).await?,
        };
        out.push(format!("{}: {line}", e.name));
    }
    Ok(out)
}

/// The action that fixes drift of kind `fix`; `None` for `Fix::Bind`, whose action names a version (`bind_hint`), and
/// for `Fix::Unchecked` (nothing to fix; `reconcile_hints` does not ask).
fn fix_hint(fix: Fix) -> Option<&'static str> {
    Some(match fix {
        Fix::Bind | Fix::Unchecked => return None,
        Fix::Publish => {
            "publish a new version (P) and promote it: a snapshot or publication is gone or another one, \
             so a promotion of this version cannot fix it."
        }
        Fix::Repair => {
            "check the content app, then repair the snapshot version (Repositories, v, F): the served files are not those of \
             the snapshot."
        }
        Fix::Origin => "set CONTENT_ORIGIN on the Pulp server: a distribution without base_url serves nothing.",
        Fix::Retention => {
            "clear retain_repo_versions of the snapshot repository with the Pulp API or CLI (pulp-tui does not edit snapshot \
             repositories): Pulp would prune its versions."
        }
    })
}

/// Promote the version that `drift` checks again (the served one, else the attempted one), or roll back to it when
/// Promote refuses it: the rule of `CvService::promote`, where Rollback to the bound version always runs.
async fn bind_hint(svc: &CvService, e: &Environment, b: &Binding) -> anyhow::Result<String> {
    let v = svc.store.version(b.versions().next().unwrap_or_default()).await?;
    Ok(match svc.promote_refusal(&v, e).await? {
        None => format!("promote v{} again (M) to reconcile (idempotent).", v.number),
        Some(why) => format!("roll back to v{} (b) to reconcile; Promote refuses: {why}", v.number),
    })
}

/// `cv::binding_text` of `b`, read from the store.
async fn shown_binding(st: &Store, b: &Binding) -> anyhow::Result<String> {
    let running = st.lock_live(b.cv_id, crate::cv::unix_now()).await?;
    let numbers: std::collections::HashMap<i64, i64> = st.versions(b.cv_id).await?.iter().map(|v| (v.id, v.number)).collect();
    Ok(crate::cv::binding_text(b, &numbers, running, "nothing"))
}

/// Show the exact distribution changes of a promotion/rollback, then run it.
fn promote_preview(app: &mut App, name: String, vid: i64, env: String, mode: Mode) {
    app.job(format!("preview {name} -> {env}"), move |ctx| async move {
        let svc = ctx.cv()?;
        let envs = svc.store.environments(&svc.scope).await?;
        let e = envs.into_iter().find(|e| e.name == env).ok_or_else(|| anyhow!("unknown environment {env}"))?;
        let v = svc.store.version(vid).await?;
        if let Some(why) = promotion_refusal(&svc, &e, &v, mode).await? {
            return ap(move |app| app.error(why));
        }
        let lines = promote_lines(&svc, &name, &e, &v, mode).await?;
        let protected = e.protected;
        let (title, danger_title, typed) = (format!("{name} -> {env}"), format!("{env} is protected"), env.clone());
        ap(move |app| {
            let go = move |app: &mut App| run_promotion(app, name, v, e, mode);
            if protected {
                app.confirm_danger(danger_title, lines, &typed, go);
            } else {
                app.confirm_prod(title, lines, &typed, go);
            }
        })
    });
}

/// The rules of `CvService::promote` that refuse the `mode` of version `v` to `e`, before a preview that the operator
/// would confirm in vain.
async fn promotion_refusal(svc: &CvService, e: &Environment, v: &CvVersion, mode: Mode) -> anyhow::Result<Option<String>> {
    Ok(match mode {
        Mode::Promote => svc.promote_refusal(v, e).await?,
        Mode::Rollback if e.prior_id.is_some() => crate::cv::rollback_refusal(&e.name, v.number, svc.bound_number(e.id, v.cv_id).await?),
        Mode::Rollback => None,
    })
}

/// The preview of the `mode` of version `v` of view `name` to `e`: what `e` serves now, a line per repository
/// (`repo_lines`), and what a rollback keeps.
async fn promote_lines(svc: &CvService, name: &str, e: &Environment, v: &CvVersion, mode: Mode) -> anyhow::Result<Vec<String>> {
    let now = match svc.store.binding(e.id, v.cv_id).await? {
        Some(b) => shown_binding(&svc.store, &b).await?,
        None => "nothing".into(),
    };
    let mut lines = vec![format!("{name}: {} now serves {now}; will serve v{}", e.name, v.number), String::new()];
    lines.extend(repo_lines(svc, name, e, v).await?);
    if mode == Mode::Rollback {
        lines.push(String::new());
        lines.push("Rollback only changes bindings; newer versions stay available.".into());
        lines.push("Files of the replaced publications may stay downloadable for Pulp's".into());
        lines.push("DISTRIBUTED_PUBLICATION_RETENTION_PERIOD; repository metadata switches immediately.".into());
    }
    Ok(lines)
}

/// The distribution in `e` of each repository of version `v` (`CvService::promote_target`), then `left_out_lines`.
async fn repo_lines(svc: &CvService, name: &str, e: &Environment, v: &CvVersion) -> anyhow::Result<Vec<String>> {
    let (vrs, rows) = (svc.store.version_repos(v.id).await?, svc.store.binding_repos(e.id, v.cv_id).await?);
    let mut lines = vec![];
    for vr in &vrs {
        let row = rows.iter().find(|b| b.repo_href == vr.repo().repo_href);
        lines.push(svc.promote_target(name, e, vr.repo(), row).await?);
    }
    lines.extend(left_out_lines(svc, name, e, &vrs, rows, &format!("v{}", v.number)).await?);
    Ok(lines)
}

/// A line for each row (`rows`, of view `name` in `e`) of a repository that version `version` (repositories `vrs`)
/// lacks, when its promotion changes it: the distribution stops serving, or the row is forgotten (`CvService::left_out`).
async fn left_out_lines(
    svc: &CvService,
    name: &str,
    e: &Environment,
    vrs: &[CvVersionRepo],
    rows: Vec<BindingRepo>,
    version: &str,
) -> anyhow::Result<Vec<String>> {
    let mut lines = vec![];
    for b in rows {
        let what = match svc.left_out(name, e, vrs, &b).await? {
            None => continue,
            Some(LeftOut::Forget(why)) => why,
            Some(LeftOut::Refuse(why)) => format!("the step fails for it: {why}"),
            Some(LeftOut::Unbind(_)) if b.publication.is_none() => {
                "its distribution stops serving (kept for rollback); Pulp shows that it serves content set outside pulp-tui".into()
            }
            Some(LeftOut::Unbind(_)) => "its distribution stops serving (kept for rollback)".into(),
        };
        lines.push(format!("  {}: not part of {version} -> {what}", b.repo_name));
    }
    Ok(lines)
}

/// What the Library step of view `id` (`name`) does when it promotes a version with the repositories `vrs` (named
/// `version`), under a heading: the repositories that it fails for (`refused_lines`) and the rows that it changes
/// (`left_out_lines`). Publish passes the draft (`CvService::draft_rows`), Resume the version.
async fn library_lines(svc: &CvService, id: i64, name: &str, vrs: &[CvVersionRepo], version: &str) -> anyhow::Result<Vec<String>> {
    let lib = svc.store.ensure_library(&svc.scope).await?; // as the Library step does
    let Some(lib) = svc.store.environments(&svc.scope).await?.into_iter().find(|e| e.id == lib) else { return Ok(vec![]) };
    let rows = svc.store.binding_repos(lib.id, id).await?;
    let mut lines = refused_lines(svc, name, &lib, vrs).await?;
    lines.extend(left_out_lines(svc, name, &lib, vrs, rows, version).await?);
    Ok(if lines.is_empty() { lines } else { std::iter::once("In Library:".to_string()).chain(lines).collect() })
}

/// `library_lines` of a Resume of version `v` of view `name`, after a blank line: none when the Library step does not
/// run (only the auto publish is left, `CvService::resume`) or a newer version is published (`CvService::bind_library`).
async fn resume_library_lines(svc: &CvService, name: &str, v: &CvVersion) -> anyhow::Result<Vec<String>> {
    if !resume_library_step(svc, v).await? {
        return Ok(vec![]);
    }
    let lines = library_lines(svc, v.cv_id, name, &svc.store.version_repos(v.id).await?, &format!("v{}", v.number)).await?;
    Ok(if lines.is_empty() { lines } else { std::iter::once(String::new()).chain(lines).collect() })
}

/// Whether a Resume of version `v` runs the Library step: `v` is not published or its Library step was interrupted,
/// and no newer version is published.
async fn resume_library_step(svc: &CvService, v: &CvVersion) -> anyhow::Result<bool> {
    let newer = svc.store.versions(v.cv_id).await?.iter().any(|n| n.state == "published" && n.number > v.number);
    Ok(!newer && (v.state != "published" || svc.library_interrupted(v).await?))
}

/// A line for each repository of `vrs` (of view `name`) that the Library step `e` fails for (`CvService::bind_target`).
async fn refused_lines(svc: &CvService, name: &str, e: &Environment, vrs: &[CvVersionRepo]) -> anyhow::Result<Vec<String>> {
    let mut lines = vec![];
    for r in vrs.iter().map(CvVersionRepo::repo) {
        if let (_, Some(why)) = svc.bind_target(name, e, r).await? {
            lines.push(format!("  {} ({}): the Library step fails for it: {why}", r.repo_name, r.repo_type.label()));
        }
    }
    Ok(lines)
}

/// The confirmed `mode` of version `v` of view `name` to `e`, under the view's lock, then what Verify finds.
fn run_promotion(app: &mut App, name: String, v: CvVersion, e: Environment, mode: Mode) {
    let (verb, env) = (if mode == Mode::Promote { "promote" } else { "rollback" }, e.name.clone());
    app.job(format!("{verb} {name} v{} -> {env}", v.number), move |ctx| async move {
        let svc = ctx.cv()?.locked(v.cv_id, &format!("{verb} to {env}")).await?;
        let res = svc.promote(v.cv_id, v.id, e.id, mode, &ctx.log(), &ctx.on_task()).await;
        let drift = svc.drift(v.cv_id, e.id).await.map_err(|e| e.to_string());
        ap(move |app| {
            promotion_result(app, &name, &env, v.number, res, drift);
            app.reload();
        })
    });
}

/// The notice of the promotion of version `number` of view `name` to `env` that ended `res`, after which Verify found
/// `drift`.
fn promotion_result(app: &mut App, name: &str, env: &str, number: i64, res: anyhow::Result<()>, drift: Result<Vec<Drift>, String>) {
    match (res, drift) {
        (Err(e), _) => app.error(format!("{e:#} (partial states are recorded; promote again to retry or roll back)")),
        // `Fix::Unchecked` is not drift, as in the drift check.
        (Ok(()), Ok(d)) if d.iter().all(|d| d.fix == Fix::Unchecked) => {
            let unchecked: Vec<String> = d.into_iter().map(|d| d.text).collect();
            let note =
                if unchecked.is_empty() { String::new() } else { format!("; File: no manifest compared ({})", unchecked.join("; ")) };
            app.ok(format!("{name} v{number} active in {env}; served metadata verified{note}"))
        }
        (Ok(()), d) => {
            let issues = d.map_or_else(|e| e, |d| d.into_iter().map(|d| d.text).collect::<Vec<_>>().join("; "));
            app.notify(Level::Warn, format!("{name} in {env}: bound, verification found: {issues}"))
        }
    }
}

/// Show the distributions that unpromoting deletes, then run it.
fn unpromote_preview(app: &mut App, cv: i64, name: String, env: String) {
    app.load_job(format!("read what unpromoting {name} from {env} deletes"), move |ctx| async move {
        let svc = ctx.cv()?;
        let envs = svc.store.environments(&svc.scope).await?;
        let e = envs.into_iter().find(|e| e.name == env).ok_or_else(|| anyhow!("unknown environment {env}"))?;
        let (e, rows) = svc.unpromote_impact(cv, e.id).await?;
        let mut lines = vec![format!("Unpromote {name} from {env}: delete its distributions there (consumers lose these URLs):")];
        lines.extend(rows.iter().map(|b| format!("  {}: /{}", b.repo_name, b.base_path)));
        lines.push(String::new());
        lines.push("Versions and snapshots stay; promote again to serve the view here.".into());
        // Like a promotion to a protected environment, the environment is typed too.
        let typed = if e.protected { format!("{name} {env}") } else { name.clone() };
        ap(move |app| {
            app.confirm_danger(format!("Unpromote {name} from {env}"), lines, &typed, move |app| {
                app.job(format!("unpromote {name} from {env}"), move |ctx| async move {
                    let svc = ctx.cv()?.locked(cv, &format!("unpromote from {env}")).await?;
                    svc.unpromote(cv, e.id, &ctx.log(), &ctx.on_task()).await?;
                    ap(move |app| {
                        app.ok(format!("{name} is not in {env} any more"));
                        app.reload();
                    })
                });
            });
        })
    });
}

/// The Publish form of view `id`; `deb`: the view has a Deb repository (read from the store).
fn cv_publish(app: &mut App, id: i64, name: String, deb: bool) {
    // Every Deb copy step uses `deb/copy/`.
    if deb && unsupported(app, crate::pulp::Feature::DebCopy) {
        return;
    }
    let mut fields = vec![Field::text("notes", "Change notes")];
    if deb {
        let choices = vec!["same as source".into(), "none".into(), "service".into()];
        fields.push(Field::new("signing", "Deb signing", FieldKind::Choice(choices)));
        fields.push(Field::pick("signing_service", "Signing service", Source::SigningServices).help("used when Deb signing = service"));
    }
    let form = Form::new(format!("Publish {name}"), fields).note("A preview of the exact content is shown before anything is created.");
    app.form(form, move |app, f| {
        let (notes, signing) = (f.get("notes").to_string(), publish_signing(f)?);
        let name = name.clone();
        app.job(format!("preview {name}"), move |ctx| async move {
            let (lines, protected) = publish_preview(&ctx.cv()?, id, &name).await?;
            ap(move |app| {
                guard_library(app, protected, lines, Some(format!("Publish {name}")), move |app| run_publish(app, id, name, notes, signing))
            })
        });
        Ok(())
    });
}

/// The Deb signing of the Publish form, stored with the version (`CvVersionRepo::signing`): "" = same as source.
fn publish_signing(f: &Form) -> Result<String, String> {
    Ok(match f.get("signing") {
        "none" => "none".to_string(),
        "service" => f.opt("signing_service").ok_or("pick a signing service")?.to_string(),
        _ => String::new(),
    })
}

/// The Publish preview of view `id` (`name`): the content against the newest published version and what the Library
/// step changes (`library_lines`); and whether Library is protected.
async fn publish_preview(svc: &CvService, id: i64, name: &str) -> anyhow::Result<(Vec<String>, bool)> {
    let latest = svc.store.versions(id).await?.into_iter().find(|v| v.state == "published").map(|v| v.id);
    let mut lines = svc.preview(id, latest).await?;
    lines.insert(0, "Publishing creates a new immutable version and makes it active in Library.".into());
    lines.insert(1, String::new());
    lines.extend(library_lines(svc, id, name, &svc.draft_rows(id).await?, "the new version").await?);
    Ok((lines, svc.library_protected().await?))
}

/// The confirmed Publish of view `id` (`name`) under its lock, then the auto publish of the composites that use it.
fn run_publish(app: &mut App, id: i64, name: String, notes: String, signing: String) {
    app.job(format!("publish {name}"), move |ctx| async move {
        let (v, note) = ctx.cv()?.locked(id, "publish").await?.publish(id, &notes, &signing, &ctx.log(), &ctx.on_task()).await?;
        // After the lock of this view is released; composites take their own locks.
        let auto = ctx.cv()?.auto_publish(id, &ctx.log(), &ctx.on_task()).await;
        ap(move |app| {
            let active = if note.is_none() { " and active in Library" } else { "" };
            let (level, text) = auto_notice(format!("{name} version {} published{active}", v.number), note, auto);
            app.notify(level, text);
            app.reload();
        })
    });
}

/// Resume the publish of version `vid` (number `number`) of view `cv`, then auto publish its composites.
fn resume(app: &mut App, cv: i64, name: String, vid: i64, number: String) {
    app.job(format!("resume {name} v{number}"), move |ctx| async move {
        let svc = ctx.cv()?.locked(cv, &format!("resume v{number}")).await?;
        let (v, note) = svc.resume(vid, &ctx.log(), &ctx.on_task()).await?;
        // Composites follow the newest version: an older one gives them nothing new. Only composites that lack it
        // publish (`lagging_composites`), also when a stopped client never ran their auto publish.
        let newest = svc.store.versions(cv).await?.into_iter().find(|n| n.state == "published").is_some_and(|n| n.id == v.id);
        drop(svc); // release the lock of this view; composites take their own
        let auto = if newest { ctx.cv()?.auto_publish(cv, &ctx.log(), &ctx.on_task()).await } else { vec![] };
        ap(move |app| {
            let (level, text) = auto_notice(format!("{name} v{} published", v.number), note, auto);
            app.notify(level, text);
            app.reload();
        })
    });
}

fn version_action(app: &mut App, scope: &str, cv: i64, name: String, act: Act, item: Value) {
    let vid = item["id"].as_i64().unwrap_or_default();
    let number = s(&item, "number");
    match act {
        Act::Promote | Act::Rollback => {
            if item["state"] != "published" {
                app.error("only published versions can be bound to environments");
                return;
            }
            let mode = if act == Act::Promote { Mode::Promote } else { Mode::Rollback };
            let Some(st) = cv_store(app) else { return };
            let p = scope.to_string();
            app.spawn("read environments".into(), true, async move {
                let envs = env_names(&st, &p).await?;
                ap(move |app| {
                    app.form(
                        Form::new(format!("v{number} -> environment"), vec![Field::new("env", "Environment", FieldKind::Choice(envs))]),
                        move |app, f| {
                            promote_preview(app, name.clone(), vid, f.get("env").to_string(), mode);
                            Ok(())
                        },
                    )
                })
            });
        }
        Act::Compare => {
            let marked: Vec<i64> = app.list.marked.iter().filter_map(|m| m.parse().ok()).filter(|m| *m != vid).collect();
            let [other] = marked.as_slice() else {
                app.error("mark exactly one other version with Space, then compare");
                return;
            };
            let (a, b) = if *other < vid { (*other, vid) } else { (vid, *other) };
            app.job("compare content view versions", move |ctx| async move {
                let lines = ctx.cv()?.compare_versions(a, b).await?;
                ap(move |app| app.text(format!("{name}: version comparison"), lines))
            });
        }
        Act::Resume => {
            let lines = vec![format!("Resume {name} v{number}: it becomes active in Library unless a newer version is published.")];
            app.load_job(format!("check resume of {name} v{number}"), move |ctx| async move {
                let svc = ctx.cv()?;
                let v = svc.store.version(vid).await?;
                if !svc.resume_left(&v).await? {
                    return ap(|app| app.notify(Level::Info, "version is already published"));
                }
                let protected = svc.library_protected().await?;
                // Only the auto publish is left (`resume_left`), and it does nothing while Library is protected.
                if protected && v.state == "published" && !svc.library_interrupted(&v).await? {
                    let names: Vec<String> = svc.lagging_composites(cv).await?.into_iter().map(|c| c.name).collect();
                    let names = names.join(", ");
                    bail!(
                        "v{number} is published; only the auto publish of {names} is left, and it does not run while Library \
                         is protected: publish {names} yourself"
                    );
                }
                // As for Publish: every Deb copy step uses `deb/copy/`.
                if svc.store.version_repos(vid).await?.iter().any(|r| r.repo().repo_type == RepoType::Deb) {
                    ctx.caps.require(crate::pulp::Feature::DebCopy)?;
                }
                // A confirmation names what the Library step changes; with no change an open Library resumes at once.
                let library = resume_library_lines(&svc, &name, &v).await?;
                let title = (!library.is_empty()).then(|| format!("Resume {name} v{number}"));
                let lines: Vec<String> = lines.into_iter().chain(library).collect();
                ap(move |app| guard_library(app, protected, lines, title, move |app| resume(app, cv, name, vid, number)))
            });
        }
        Act::Delete => app.load_job(format!("read what deleting {name} v{number} removes"), move |ctx| async move {
            let objects = ctx.cv()?.delete_version_impact(vid).await?;
            // Only a composite version has components; it borrows their snapshots and publications.
            let mut lines = if item["components"].as_array().is_some_and(|c| !c.is_empty()) {
                vec![
                    format!("Delete {name} version {number}: only this composite version is removed."),
                    "The snapshots and publications of its component versions are NOT touched.".into(),
                ]
            } else if objects.is_empty() {
                vec![format!("Delete {name} version {number}. Nothing in Pulp is deleted.")]
            } else {
                vec![format!("Delete {name} version {number}. In Pulp:")]
            };
            lines.extend(objects.into_iter().map(|(repo, href, keep)| match keep {
                None => format!("  {repo}: delete {href}"),
                Some(why) => format!("  {repo}: keep {href} ({why})"),
            }));
            lines.push("The version number is never reused.".into());
            ap(move |app| {
                app.confirm_danger(format!("Delete {name} v{number}"), lines, &format!("v{number}"), move |app| {
                    app.job(format!("delete {name} v{number}"), move |ctx| async move {
                        ctx.cv()?.locked(cv, &format!("delete v{number}")).await?.delete_version(vid, &ctx.log(), &ctx.on_task()).await?;
                        ap(move |app| {
                            app.ok("version deleted");
                            app.reload();
                        })
                    });
                });
            })
        }),
        Act::WatchTask => {
            let Some(st) = cv_store(app) else { return };
            // Read again: the list is stale while another operator publishes.
            app.spawn("read pending task".into(), true, async move {
                // Repositories publish one at a time, so at most one row has a pending task.
                // With a derived version, the task is the publication.
                let pending = st.version_repos(vid).await?.into_iter().find_map(|r| {
                    let step = if r.derived_version.is_some() { "publish" } else { "copy" };
                    Some((step, r.repo().repo_name.clone(), r.pending_task?))
                });
                ap(move |app| match pending {
                    Some((step, repo, h)) => app.watch_task(&h, &format!("{name} v{number}: {step} {repo}"), None),
                    None => app.notify(
                        Level::Info,
                        format!(
                            "v{number} has no copy or publication task now. W watches only these two steps; \
                             the session that runs the publish shows the others (settings, cleanup, Library) in Tasks"
                        ),
                    ),
                })
            });
        }
        _ => {}
    }
}

fn env_action(app: &mut App, scope: &str, act: Act, item: Value) {
    let id = item["id"].as_i64().unwrap_or_default();
    let name = s(&item, "name");
    let p = scope.to_string();
    match act {
        Act::Create => {
            let Some(st) = cv_store(app) else { return };
            app.spawn("read environments".into(), true, async move {
                let envs = env_names(&st, &p).await?;
                ap(move |app| {
                    let form = Form::new(
                        "New lifecycle environment",
                        vec![
                            Field::text("name", "Name").required(),
                            Field::new("prior", "Comes after", FieldKind::Choice(envs)),
                            Field::new("protected", "Protected (typed confirmation)", FieldKind::Bool),
                        ],
                    )
                    .note("Paths branch from Library, e.g. Library -> Dev -> Test -> Prod. Promotion follows the path.");
                    app.form(form, move |app, f| {
                        let (n, prior, prot, p) = (f.get("name").to_string(), f.get("prior").to_string(), f.bool("protected"), p.clone());
                        local(app, "create environment", async move |st| {
                            let pid = st
                                .environments(&p)
                                .await?
                                .into_iter()
                                .find(|e| e.name == prior)
                                .map(|e| e.id)
                                .ok_or_else(|| anyhow!("unknown prior {prior}"))?;
                            st.create_env(&p, &n, pid, prot).await?;
                            Ok(format!("environment {n} created after {prior}"))
                        });
                        Ok(())
                    });
                })
            });
        }
        Act::Edit => {
            let old = s(&item, "label");
            let form = Form::new(
                format!("Edit environment {name}"),
                vec![
                    Field::text("label", "Label")
                        .value(old.clone())
                        .required()
                        .help("lowercase [a-z0-9-_.], the <lc> part of served paths"),
                ],
            )
            .note("Changing the label moves every served path of this environment.");
            app.form(form, move |app, f| {
                let label = f.get("label").to_string();
                crate::store::validate_label(&label).map_err(|e| e.to_string())?;
                let (name, old) = (name.clone(), old.clone());
                app.job(format!("read served paths of {name}"), move |ctx| async move {
                    let rows = ctx.cv()?.relabel_impact(id, &label).await?;
                    ap(move |app| {
                        if rows.is_empty() {
                            return relabel(app, id, name, label);
                        }
                        let mut lines = vec![format!("Label {old} -> {label}. Served paths move:")];
                        lines.extend(rows.iter().map(|(b, new)| format!("  /{} -> /{new}", b.base_path)));
                        lines.push("Clients using the old paths stop getting content.".into());
                        app.confirm_danger(format!("Change label of {name}"), lines, &name.clone(), move |app| {
                            relabel(app, id, name, label)
                        });
                    })
                });
                Ok(())
            });
        }
        Act::ToggleProtected => {
            let prot = item["protected"] != true;
            let (title, typed) = (format!("Remove protection from {name}"), name.clone());
            let lines = vec![
                format!("Promote and roll back to {name} (for Library also publish and Resume) stop asking to type its name."),
                "With a shared database, this applies to every operator.".into(),
            ];
            let toggle = move |app: &mut App| {
                local(app, "toggle protected", async move |st| {
                    st.set_env_protected(id, prot).await?;
                    Ok(format!("{name} protected: {prot}"))
                })
            };
            // Adding protection only adds a question to later changes: no confirmation.
            if prot { toggle(app) } else { app.confirm_danger(title, lines, &typed, toggle) }
        }
        Act::Delete => {
            if name == LIBRARY && item["prior"] == "" {
                app.error("Library cannot be deleted");
                return;
            }
            let Some(st) = cv_store(app) else { return };
            app.spawn(format!("read what deleting {name} removes"), true, async move {
                let envs = st.environments(&p).await?;
                let env = envs.iter().find(|e| e.id == id).ok_or_else(|| anyhow!("unknown environment {name}"))?;
                let after: Vec<&str> = envs.iter().filter(|e| e.prior_id == Some(id)).map(|e| e.name.as_str()).collect();
                let (mut bound, mut served) = (vec![], vec![]);
                for cv in st.env_cv_ids(id).await? {
                    let view = st.content_view(cv).await?.name;
                    if st.binding(id, cv).await?.is_some() {
                        bound.push(view.clone());
                    }
                    for b in st.binding_repos(id, cv).await? {
                        served.push(format!("  {view}/{}  /{}  {}", b.repo_name, b.base_path, b.distribution));
                    }
                }
                // The refusal of `Store::delete_env`, with the names, before the typed confirmation.
                let mut why = vec![];
                if !bound.is_empty() {
                    why.push(format!("views bound to it: {} (unpromote them first)", bound.join(", ")));
                }
                if !after.is_empty() {
                    why.push(format!("environments after it: {} (delete them first)", after.join(", ")));
                }
                if !why.is_empty() {
                    bail!("{name} is in use: {}", why.join("; "));
                }
                let mut lines = vec![format!("Delete lifecycle environment {name} (label {}). No view is bound to it.", env.label)];
                if !served.is_empty() {
                    lines.push("pulp-tui forgets these served rows; their distributions stay in Pulp:".into());
                    lines.extend(served);
                }
                ap(move |app| {
                    app.confirm_danger(format!("Delete environment {name}"), lines, &name.clone(), move |app| {
                        local(app, "delete environment", async move |st| {
                            st.delete_env(id).await?;
                            Ok(format!("environment {name} deleted"))
                        })
                    })
                })
            });
        }
        _ => {}
    }
}

/// Change the label of environment `id` (its served paths move) and show what moved.
fn relabel(app: &mut App, id: i64, name: String, label: String) {
    app.job(format!("relabel {name}"), move |ctx| async move {
        let (lines, failed) = ctx.cv()?.relabel_env(id, &label, &ctx.log(), &ctx.on_task()).await?;
        ap(move |app| {
            app.text(format!("Label of {name}"), lines);
            if failed == 0 {
                app.ok(format!("label of {name} is {label}"));
            } else {
                app.notify(Level::Warn, format!("{failed} path(s) not moved; run Edit again with the same label to retry"));
            }
            app.reload();
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthMethod, Config, Profile, Settings};
    use crate::tui::app::Modal;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn item(name: &str, t: &str) -> PickItem {
        let collection = match t {
            "deb" => "deb/apt".to_string(),
            _ => format!("{t}/{t}"),
        };
        PickItem { label: name.into(), value: format!("/pulp/api/v3/repositories/{collection}/{name}/"), note: t.into() }
    }

    #[test]
    fn mixed_types_with_rules_rejected() {
        let msg = "filter rules need repositories of one type; add repositories of different types separately";
        for other in ["file", "deb"] {
            let e = new_cv_repos(&[item("a", "rpm"), item("b", other)], "exclude name=*-debuginfo").unwrap_err();
            assert_eq!(e, msg);
        }
    }

    #[test]
    fn deb_repositories_take_deb_rules() {
        let r = new_cv_repos(&[item("a", "deb"), item("b", "deb")], "exclude name=*-dbg version<1.0").unwrap();
        assert_eq!((r[0].repo_type, r[1].repo_type), (RepoType::Deb, RepoType::Deb));
        assert_eq!(r[1].filters[0].to_string(), "exclude name=*-dbg version<1.0");
        assert!(new_cv_repos(&[item("a", "deb")], "exclude evr<1.0").is_err());
    }

    #[test]
    fn mixed_types_without_rules_ok() {
        let r = new_cv_repos(&[item("a", "rpm"), item("b", "file")], "").unwrap();
        assert_eq!((r[0].repo_type, r[1].repo_type), (RepoType::Rpm, RepoType::File));
        assert!(r.iter().all(|x| x.filters.is_empty()));
    }

    #[test]
    fn rules_apply_to_each_selected_repo() {
        let r = new_cv_repos(&[item("a", "rpm"), item("b", "rpm")], "exclude name=*-debuginfo").unwrap();
        assert_eq!(r.len(), 2);
        assert!(r.iter().all(|x| x.filters.len() == 1));
        assert_eq!(r[1].repo_name, "b");
    }

    #[test]
    fn nothing_selected_rejected() {
        assert!(new_cv_repos(&[], "").is_err());
    }

    #[test]
    fn unknown_type_is_an_error() {
        let odd = PickItem { label: "x".into(), value: "/pulp/api/v3/repositories/container/container/x/".into(), note: String::new() };
        assert_eq!(new_cv_repos(&[odd], "").unwrap_err(), "unknown repository type for x");
    }

    #[test]
    fn blank_rules_do_not_count_for_mixed_types() {
        assert!(new_cv_repos(&[item("a", "rpm"), item("b", "file")], " ; ").is_ok());
    }

    const REPOS: &str = "/pulp/api/v3/repositories/";
    const TASKS: &str = "/pulp/api/v3/tasks/";

    fn client(server: &MockServer) -> PulpClient {
        let p = Profile { url: server.uri(), auth: AuthMethod::None, ..Default::default() };
        PulpClient::new("t", &p, &Settings::default()).unwrap()
    }

    async fn serve(server: &MockServer, verb: &str, p: String, status: u16, body: Value) {
        Mock::given(method(verb)).and(path(p)).respond_with(ResponseTemplate::new(status).set_body_json(body)).mount(server).await;
    }

    fn cv_repo(name: &str, t: &str, id: &str) -> CvRepo {
        let repo_href = format!("{REPOS}{t}/{t}/{id}/");
        CvRepo { repo_href, repo_name: name.into(), repo_type: RepoType::from_href(&format!("/{t}/{t}/")).unwrap(), filters: vec![] }
    }

    async fn test_app() -> App {
        let st = Arc::new(Store::open_in_memory().await.unwrap());
        App::new(Config::default(), "/dev/null".into(), st.clone(), Ok(st))
    }

    fn rows(v: Vec<(&str, RepoSync)>) -> Vec<(String, RepoSync)> {
        v.into_iter().map(|(n, r)| (n.to_string(), r)).collect()
    }

    fn mixed_rows() -> Vec<(String, RepoSync)> {
        rows(vec![
            ("os", RepoSync::NewVersion(format!("{REPOS}rpm/rpm/b/versions/4/"))),
            ("appstream", RepoSync::NoChange),
            ("docs", RepoSync::Failed("rejected [400]".into())),
            ("iso", RepoSync::Skipped),
        ])
    }

    #[tokio::test]
    async fn start_syncs_skip_and_fail_per_repository() {
        let server = MockServer::start().await;
        let (a, b, c_, d) = ("file/file/a/", "rpm/rpm/b/", "file/file/c/", "rpm/rpm/d/");
        let remote = json!({ "remote": "/pulp/api/v3/remotes/x/1/" });
        serve(&server, "GET", format!("{REPOS}{a}"), 200, remote.clone()).await;
        serve(&server, "POST", format!("{REPOS}{a}sync/"), 400, json!({ "remote": ["Invalid hyperlink"] })).await;
        serve(&server, "GET", format!("{REPOS}{b}"), 200, remote).await;
        serve(&server, "POST", format!("{REPOS}{b}sync/"), 202, json!({ "task": format!("{TASKS}1/") })).await;
        serve(&server, "GET", format!("{REPOS}{c_}"), 200, json!({ "remote": null })).await;
        serve(&server, "GET", format!("{REPOS}{d}"), 404, json!({ "detail": "Not found." })).await;
        let repos =
            [cv_repo("a-docs", "file", "a"), cv_repo("b-os", "rpm", "b"), cv_repo("c-iso", "file", "c"), cv_repo("d-gone", "rpm", "d")];
        let rows = start_syncs(&client(&server), &repos, "additive").await;
        assert!(matches!(&rows[0], (n, RepoSync::Failed(e)) if n == "a-docs" && e.starts_with("rejected [400]")), "{:?}", rows[0]);
        assert_eq!(rows[1], ("b-os".into(), RepoSync::Submitted(format!("{TASKS}1/"))));
        assert_eq!(rows[2], ("c-iso".into(), RepoSync::Skipped));
        assert!(matches!(&rows[3], (n, RepoSync::Failed(e)) if n == "d-gone" && e.starts_with("not found [404]")), "{:?}", rows[3]);
        // One POST each for a and b; nothing for c (no remote) or d (gone); the 400 is not retried.
        let mut posts: Vec<_> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "POST")
            .map(|r| r.url.path().to_string())
            .collect();
        posts.sort();
        assert_eq!(posts, [format!("{REPOS}file/file/a/sync/"), format!("{REPOS}rpm/rpm/b/sync/")]);
    }

    #[tokio::test]
    async fn finish_syncs_read_each_task() {
        let server = MockServer::start().await;
        let t = |n: u8, state: &str, created: Vec<&str>, error: Value| json!({ "pulp_href": format!("{TASKS}{n}/"), "state": state, "created_resources": created, "error": error });
        let v4 = format!("{REPOS}rpm/rpm/b/versions/4/");
        for (n, body) in [
            (1, t(1, "completed", vec![&v4], Value::Null)),
            (2, t(2, "completed", vec![], Value::Null)),
            (3, t(3, "failed", vec![], json!({ "description": "Cannot connect to host" }))),
            (4, t(4, "canceled", vec![], Value::Null)),
        ] {
            serve(&server, "GET", format!("{TASKS}{n}/"), 200, body).await;
        }
        let sub = |n: u8| RepoSync::Submitted(format!("{TASKS}{n}/"));
        let input = vec![
            ("os".into(), sub(1)),
            ("appstream".into(), sub(2)),
            ("epel".into(), sub(3)),
            ("extras".into(), sub(4)),
            ("iso".into(), RepoSync::Skipped),
        ];
        let out = finish_syncs(&client(&server), input, &ops::no_progress).await;
        assert_eq!(out.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), ["os", "appstream", "epel", "extras", "iso"]);
        assert_eq!(
            out.iter().map(|r| &r.1).collect::<Vec<_>>(),
            [
                &RepoSync::NewVersion(v4),
                &RepoSync::NoChange,
                &RepoSync::Failed("task failed: Cannot connect to host".into()),
                &RepoSync::Failed("task canceled: ".into()),
                &RepoSync::Skipped
            ]
        );
    }

    #[tokio::test]
    async fn a_skipped_task_is_not_a_sync() {
        let server = MockServer::start().await;
        serve(&server, "GET", format!("{TASKS}1/"), 200, json!({ "pulp_href": format!("{TASKS}1/"), "state": "skipped" })).await;
        let out = finish_syncs(&client(&server), vec![("os".into(), RepoSync::Submitted(format!("{TASKS}1/")))], &ops::no_progress).await;
        assert_eq!(out[0].1, RepoSync::Failed("task skipped: ".into()));
    }

    #[tokio::test]
    async fn a_lost_task_read_is_not_reported_as_a_failed_task() {
        let server = MockServer::start().await;
        serve(&server, "GET", format!("{TASKS}1/"), 500, json!({ "detail": "boom" })).await;
        let out = finish_syncs(&client(&server), vec![("os".into(), RepoSync::Submitted(format!("{TASKS}1/")))], &ops::no_progress).await;
        let RepoSync::Failed(e) = &out[0].1 else { panic!("{:?}", out[0]) };
        assert!(e.starts_with(&format!("could not read task {TASKS}1/, it may still be running (see Tasks): ")), "{e}");
        assert!(e.contains("server error [500]"), "{e}");
    }

    #[test]
    fn sync_lines_list_every_repository() {
        assert_eq!(
            sync_lines(&mixed_rows()),
            [
                "2 synced, 1 failed, 1 skipped",
                "",
                "  os: synced, new version 4",
                "  appstream: synced, no change",
                "  docs: failed: rejected [400]",
                "  iso: skipped: no default remote (set one with e on the repository)",
                "",
                "Sync does not publish the view: publish a new version (P) to use the new content."
            ]
        );
        assert_eq!(sync_lines(&rows(vec![("x", RepoSync::Submitted("/t/".into()))]))[2], "  x: submitted, see Tasks");
    }

    #[test]
    fn sync_audit_names_failed_and_skipped() {
        // The entry is written at submit time: rows are Submitted, Failed or Skipped.
        let submitted = rows(vec![
            ("os", RepoSync::Submitted("/t/1/".into())),
            ("appstream", RepoSync::Submitted("/t/2/".into())),
            ("docs", RepoSync::Failed("rejected [400]".into())),
            ("epel", RepoSync::Failed("not found [404]".into())),
            ("iso", RepoSync::Skipped),
        ]);
        assert_eq!(
            sync_audit("web", "additive", &submitted),
            "web (additive): 2 submitted, 2 failed, 1 skipped; failed: docs, epel; skipped: iso"
        );
        assert_eq!(
            sync_audit("web", "mirror", &rows(vec![("os", RepoSync::Submitted("/t/1/".into()))])),
            "web (mirror): 1 submitted, 0 failed, 0 skipped"
        );
    }

    fn sync_form(app: &mut App, repos: Value) -> Option<crate::tui::form::Form> {
        cv_action(app, "lab", Act::CvSyncRepos, json!({ "id": 1, "name": "web", "repos": repos }));
        match app.modal.take() {
            Some(Modal::Form { form, .. }) => Some(form),
            _ => None,
        }
    }

    #[tokio::test]
    async fn cv_publish_asks_for_deb_signing_only_with_a_deb_repository() {
        let (mut app, mut rx, st) = composite_app().await;
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let repo =
            |name: &str, t: RepoType, href: &str| CvRepo { repo_href: href.into(), repo_name: name.into(), repo_type: t, filters: vec![] };
        st.set_cv_repo(web, &repo("os", RepoType::Rpm, &format!("{REPOS}rpm/rpm/1/"))).await.unwrap();
        let keys = |f: &crate::tui::form::Form| f.fields.iter().map(|x| x.key.clone()).collect::<Vec<_>>();
        // The repositories come from the store: the list row can be stale in a shared database.
        let row = json!({ "id": web, "name": "web", "repos": [] });
        cv_action(&mut app, "lab", Act::CvPublish, row.clone());
        drain(&mut app, &mut rx).await;
        let Some(Modal::Form { form, .. }) = app.modal.take() else { panic!("no publish form") };
        assert_eq!(keys(&form), ["notes"]);
        st.set_cv_repo(web, &repo("apt", RepoType::Deb, &format!("{REPOS}deb/apt/2/"))).await.unwrap();
        cv_action(&mut app, "lab", Act::CvPublish, row);
        drain(&mut app, &mut rx).await;
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no publish form") };
        assert_eq!(keys(&form), ["notes", "signing", "signing_service"]);
        assert_eq!(form.get("signing"), "same as source");
        form.fields[1].value = "service".into();
        assert_eq!(submit(&mut app, &form), Err("pick a signing service".to_string()));
    }

    /// Connect `app` to `server` as pulp_deb without `deb/copy/` in its OpenAPI schema.
    fn connect_without_deb_copy(app: &mut App, server: &MockServer) {
        let mut caps = crate::pulp::Capabilities::default();
        caps.versions.insert("deb".into(), "2.20.0".into());
        caps.load_openapi(&json!({ "paths": { "/pulp/api/v3/repositories/deb/apt/": {} } }));
        app.conn = Some(crate::tui::app::Conn { client: client(server), caps: Arc::new(caps), scope: "lab".into() });
    }

    #[tokio::test]
    async fn resume_of_a_deb_version_needs_the_deb_copy_api() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        connect_without_deb_copy(&mut app, &server);
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let apt = CvRepo { repo_href: format!("{REPOS}deb/apt/2/"), repo_name: "apt".into(), repo_type: RepoType::Deb, filters: vec![] };
        let v = st.begin_version(web, "", &[apt]).await.unwrap();
        st.finish_version(v.id, "failed", Some("copy failed")).await.unwrap();
        version_action(&mut app, "lab", web, "web".into(), Act::Resume, json!({ "id": v.id, "number": v.number, "state": "failed" }));
        drain(&mut app, &mut rx).await;
        assert!(app.modal.is_none(), "stopped before the confirmation");
        let (level, text) = last_notice(&app);
        assert!(level == Level::Error && text.ends_with(": not supported by this server: Deb copy API (deb/copy/)"), "{text}");
        assert!(server.received_requests().await.unwrap().is_empty(), "nothing sent");
        assert_eq!(st.version(v.id).await.unwrap().state, "failed");
    }

    #[tokio::test]
    async fn cv_publish_of_a_deb_view_needs_the_deb_copy_api() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        connect_without_deb_copy(&mut app, &server);
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let apt = CvRepo { repo_href: format!("{REPOS}deb/apt/2/"), repo_name: "apt".into(), repo_type: RepoType::Deb, filters: vec![] };
        st.set_cv_repo(web, &apt).await.unwrap();
        // Another operator added the Deb repository after this list was read.
        cv_action(&mut app, "lab", Act::CvPublish, json!({ "id": web, "name": "web", "repos": [] }));
        drain(&mut app, &mut rx).await;
        assert!(app.modal.is_none(), "stopped before the publish form");
        assert_eq!(last_notice(&app), (Level::Error, "not supported by this server: Deb copy API (deb/copy/)"));
        assert!(server.received_requests().await.unwrap().is_empty(), "nothing sent");
    }

    #[tokio::test]
    async fn cv_sync_form_marks_every_repository() {
        let mut app = test_app().await;
        let both = json!([
            { "name": "os", "type": "rpm", "href": format!("{REPOS}rpm/rpm/1/") },
            { "name": "iso", "type": "file", "href": format!("{REPOS}file/file/2/") }
        ]);
        let form = sync_form(&mut app, both).expect("no form");
        assert_eq!((form.picked("repos").len(), form.fields[0].shown.as_str()), (2, "2 selected"));
        assert_eq!(form.fields[1].kind, FieldKind::Choice(super::super::actions::sync_policies(None, RepoType::Rpm)));
        let note = "File and Deb repositories: additive stays additive; any mirror policy means mirror.";
        assert!(form.notes.iter().any(|n| n == note), "RPM + File still shows the mixed-sync note: {:?}", form.notes);
        // A file-only view gets the File values.
        let files = json!([{ "name": "iso", "type": "file", "href": format!("{REPOS}file/file/2/") }]);
        let form = sync_form(&mut app, files).expect("no form");
        assert_eq!(form.fields[1].kind, FieldKind::Choice(vec!["additive".into(), "mirror".into()]));
        // RPM mixed with File or Deb: the note says how the other types read the RPM values.
        assert!(form.notes.iter().all(|n| n != note));
        let deb = json!({ "name": "apt", "type": "deb", "href": format!("{REPOS}deb/apt/3/") });
        let rpm = json!({ "name": "os", "type": "rpm", "href": format!("{REPOS}rpm/rpm/1/") });
        let form = sync_form(&mut app, json!([rpm, deb])).expect("no form");
        assert!(form.notes.iter().any(|n| n == note), "{:?}", form.notes);
        assert_eq!(form.fields[1].kind, FieldKind::Choice(super::super::actions::sync_policies(None, RepoType::Rpm)));
        let form = sync_form(&mut app, json!([deb])).expect("no form");
        assert!(form.notes.iter().all(|n| n != note));
        assert_eq!(form.fields[1].kind, FieldKind::Choice(vec!["additive".into(), "mirror".into()]));
        // No repositories: no form, an error notice.
        let mut empty = test_app().await;
        assert!(sync_form(&mut empty, json!([])).is_none());
        assert!(empty.notices.iter().any(|n| n.text == "content view has no repositories"));
    }

    /// Submit the sync form of a view with the RPM repository `os` and the File repository `iso` (default policy),
    /// and run the jobs: to the end, or (`stop_at_wait`) until the wait job has started. `cv_store` false = no store for
    /// Content Views. Returns the audit log of the connection, the result view and the number of tracked tasks.
    async fn run_sync(server: &MockServer, cv_store: bool, stop_at_wait: bool) -> (Vec<(String, String)>, Option<Modal>, usize) {
        let st = Arc::new(Store::open_in_memory().await.unwrap());
        let cv = if cv_store { Ok(st.clone()) } else { Err("no store".into()) };
        let mut app = App::new(Config::default(), "/dev/null".into(), st, cv);
        let store = app.store.clone();
        app.conn = Some(crate::tui::app::Conn { client: client(server), caps: Default::default(), scope: "mock".into() });
        app.profile = Some("lab".into());
        let mut rx = app.rx.take().unwrap();
        let repos = json!([
            { "name": "os", "type": "rpm", "href": format!("{REPOS}rpm/rpm/1/") },
            { "name": "iso", "type": "file", "href": format!("{REPOS}file/file/2/") }
        ]);
        cv_action(&mut app, "mock", Act::CvSyncRepos, json!({ "id": 1, "name": "web", "repos": repos }));
        let Some(Modal::Form { form, mut submit }) = app.modal.take() else { panic!("no form") };
        submit(&mut app, &form).unwrap();
        while !app.jobs.is_empty() && !(stop_at_wait && app.jobs.iter().any(|j| j.label.starts_with("wait for syncs"))) {
            let m = rx.recv().await.unwrap();
            app.handle_msg(m);
        }
        let log = store.audit_log("mock", 10).await.unwrap();
        (log.into_iter().map(|a| (a.action, a.detail)).collect(), app.modal.take(), app.tasks.items.len())
    }

    #[tokio::test]
    async fn one_audit_entry_when_the_syncs_are_submitted() {
        let server = MockServer::start().await;
        serve(&server, "GET", format!("{REPOS}rpm/rpm/1/"), 200, json!({ "remote": "/pulp/api/v3/remotes/rpm/rpm/1/" })).await;
        serve(&server, "POST", format!("{REPOS}rpm/rpm/1/sync/"), 202, json!({ "task": format!("{TASKS}1/") })).await;
        let v2 = format!("{REPOS}rpm/rpm/1/versions/2/");
        serve(
            &server,
            "GET",
            format!("{TASKS}1/"),
            200,
            json!({ "pulp_href": format!("{TASKS}1/"), "state": "completed", "created_resources": [v2] }),
        )
        .await;
        serve(&server, "GET", format!("{REPOS}file/file/2/"), 200, json!({ "remote": null })).await;
        // Exactly one entry for the whole run: the wait job writes none. The entry has the submit result.
        let (log, modal, _) = run_sync(&server, true, false).await;
        assert_eq!(log, [("cv.sync".to_string(), "web (additive): 1 submitted, 0 failed, 1 skipped; skipped: iso".to_string())]);
        let Some(Modal::Text { title, lines, .. }) = modal else { panic!("no result") };
        assert_eq!((title.as_str(), lines[2].as_str()), ("Sync of web", "  os: synced, new version 2"));
    }

    #[tokio::test]
    async fn the_audit_entry_exists_while_the_tasks_run() {
        let server = MockServer::start().await;
        serve(&server, "GET", format!("{REPOS}rpm/rpm/1/"), 200, json!({ "remote": "/pulp/api/v3/remotes/rpm/rpm/1/" })).await;
        serve(&server, "POST", format!("{REPOS}rpm/rpm/1/sync/"), 202, json!({ "task": format!("{TASKS}1/") })).await;
        serve(&server, "GET", format!("{REPOS}file/file/2/"), 200, json!({ "remote": null })).await;
        // The task is never read: the run stops when the wait job starts.
        let (log, modal, tracked) = run_sync(&server, true, true).await;
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(log[0].1, "web (additive): 1 submitted, 0 failed, 1 skipped; skipped: iso");
        assert!(modal.is_none(), "the result is not known yet");
        assert_eq!(tracked, 1);
    }

    #[tokio::test]
    async fn an_unavailable_cv_store_does_not_fail_the_sync() {
        let server = MockServer::start().await;
        serve(&server, "GET", format!("{REPOS}rpm/rpm/1/"), 200, json!({ "remote": "/pulp/api/v3/remotes/rpm/rpm/1/" })).await;
        serve(&server, "POST", format!("{REPOS}rpm/rpm/1/sync/"), 202, json!({ "task": format!("{TASKS}1/") })).await;
        serve(&server, "GET", format!("{REPOS}file/file/2/"), 200, json!({ "remote": null })).await;
        let done = json!({ "pulp_href": format!("{TASKS}1/"), "state": "completed", "created_resources": [] });
        serve(&server, "GET", format!("{TASKS}1/"), 200, done).await;
        let (log, modal, tracked) = run_sync(&server, false, false).await;
        assert!(log.is_empty(), "{log:?}");
        let Some(Modal::Text { lines, .. }) = modal else { panic!("no result") };
        assert_eq!((lines[0].as_str(), tracked), ("1 synced, 0 failed, 1 skipped", 1));
    }

    #[tokio::test]
    async fn one_audit_entry_at_once_when_nothing_is_submitted() {
        let server = MockServer::start().await;
        serve(&server, "GET", format!("{REPOS}rpm/rpm/1/"), 200, json!({ "remote": null })).await;
        serve(&server, "GET", format!("{REPOS}file/file/2/"), 404, json!({ "detail": "Not found." })).await;
        let (log, modal, _) = run_sync(&server, true, false).await;
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(log[0].1, "web (additive): 0 submitted, 1 failed, 1 skipped; failed: iso; skipped: os");
        assert!(matches!(modal, Some(Modal::Text { .. })));
        assert!(server.received_requests().await.unwrap().iter().all(|r| r.method.as_str() == "GET"), "nothing was sent");
    }

    #[tokio::test]
    async fn submitted_syncs_are_tracked_before_waiting() {
        let mut app = test_app().await;
        let input = vec![("os".into(), RepoSync::Submitted(format!("{TASKS}1/"))), ("iso".into(), RepoSync::Skipped)];
        wait_syncs(&mut app, "web".into(), "additive".into(), input);
        assert!(app.tasks.items.iter().any(|t| t.href == format!("{TASKS}1/") && t.label == "sync os (additive)"));
    }

    #[tokio::test]
    async fn nothing_submitted_shows_the_result_at_once() {
        let mut app = test_app().await;
        let input = rows(vec![("iso", RepoSync::Skipped), ("docs", RepoSync::Failed("rejected [400]".into()))]);
        wait_syncs(&mut app, "web".into(), "additive".into(), input);
        let Some(Modal::Text { title, lines, .. }) = &app.modal else { panic!("no result") };
        assert_eq!((title.as_str(), lines[0].as_str()), ("Sync of web", "0 synced, 1 failed, 1 skipped"));
        assert!(app.jobs.is_empty() && app.tasks.items.is_empty());
    }

    // ---- composite views ----

    /// An app on profile `lab` (scope `lab`, the local store) and the receiver of its job results.
    async fn composite_app() -> (App, tokio::sync::mpsc::UnboundedReceiver<crate::tui::app::Msg>, Arc<Store>) {
        let mut app = test_app().await;
        app.profile = Some("lab".into());
        let rx = app.rx.take().unwrap();
        let st = app.cv_store.clone().unwrap();
        (app, rx, st)
    }

    #[tokio::test]
    async fn watch_opens_the_pending_copy_or_publication_task_of_a_version() {
        let (mut app, mut rx, st) = composite_app().await;
        let cv = st.create_cv("lab", "web", "").await.unwrap();
        let v = st.begin_version(cv, "", &[cv_repo("os", "rpm", "1")]).await.unwrap();
        let row = json!({ "id": v.id, "number": v.number });
        version_action(&mut app, "lab", cv, "web".into(), Act::WatchTask, row.clone());
        drain(&mut app, &mut rx).await;
        assert!(app.modal.is_none());
        let none = "v1 has no copy or publication task now. W watches only these two steps; \
                    the session that runs the publish shows the others (settings, cleanup, Library) in Tasks";
        assert_eq!(last_notice(&app), (Level::Info, none));

        let mut vr = st.version_repos(v.id).await.unwrap().remove(0);
        vr.pending_task = Some(format!("{TASKS}7/"));
        st.save_version_repo(&vr).await.unwrap();
        version_action(&mut app, "lab", cv, "web".into(), Act::WatchTask, row.clone());
        drain(&mut app, &mut rx).await;
        let Some(Modal::Text { title, watch, .. }) = &app.modal else { panic!("no watch view") };
        assert_eq!((title.as_str(), watch.clone()), ("Watch task web v1: copy os", Some(format!("{TASKS}7/"))));
        assert_eq!(app.tasks.items[0].href, format!("{TASKS}7/"), "tracked, so the Task Center polls it");

        // After the copy, a pending task is the publication.
        vr.derived_version = Some("/dv/1/".into());
        st.save_version_repo(&vr).await.unwrap();
        app.modal = None; // the operator closes the first view (an open one would keep the second waiting)
        version_action(&mut app, "lab", cv, "web".into(), Act::WatchTask, row);
        drain(&mut app, &mut rx).await;
        let Some(Modal::Text { title, .. }) = &app.modal else { panic!("no watch view") };
        assert_eq!(title, "Watch task web v1: publish os");
    }

    /// Apply job results until no job runs.
    async fn drain(app: &mut App, rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::tui::app::Msg>) {
        while !app.jobs.is_empty() {
            let m = rx.recv().await.unwrap();
            app.handle_msg(m);
        }
    }

    async fn publish_empty(st: &Store, cv: i64) -> crate::store::CvVersion {
        let v = st.begin_version(cv, "", &[]).await.unwrap();
        st.finish_version(v.id, "published", None).await.unwrap();
        st.version(v.id).await.unwrap()
    }

    fn open_form(app: &mut App) -> (Form, crate::tui::app::Submit) {
        match app.modal.take() {
            Some(Modal::Form { form, submit }) => (form, submit),
            _ => panic!("no form"),
        }
    }

    fn set(form: &mut Form, key: &str, value: &str) {
        form.fields.iter_mut().find(|f| f.key == key).unwrap().value = value.into();
    }

    fn last_notice(app: &App) -> (Level, &str) {
        let n = app.notices.back().expect("no notice");
        (n.level, n.text.as_str())
    }

    #[test]
    fn pin_choices_list_latest_then_published_versions_newest_first() {
        let v = |number: i64, state: &str| crate::store::CvVersion {
            id: number * 10,
            cv_id: 1,
            number,
            notes: String::new(),
            state: state.into(),
            created_at: String::new(),
            finished_at: None,
            error: None,
        };
        let versions = [v(4, "failed"), v(3, "published"), v(2, "deleted"), v(1, "published")];
        assert_eq!(pin_choices(&versions), ["latest", "v3", "v1"]);
        assert_eq!(pin_choices(&[]), ["latest"]);
        assert_eq!(pin_of("latest", &versions), None);
        assert_eq!(pin_of("v1", &versions), Some(10));
        assert_eq!(pin_of("v3", &versions), Some(30));
        assert_eq!(pin_of("v4", &versions), None, "a version that is not published is not a pin");
        assert_eq!(pin_choice(Some(30), &versions), "v3");
        assert_eq!(pin_choice(None, &versions), "latest");
    }

    #[test]
    fn auto_publish_results_extend_the_publish_notice() {
        let ok = |s: &str| Ok(s.to_string());
        assert_eq!(auto_notice("x published".into(), None, vec![]), (Level::Ok, "x published".into()));
        assert_eq!(
            auto_notice("x published".into(), None, vec![ok("a v2 published"), ok("b v1 published")]),
            (Level::Ok, "x published; auto publish: a v2 published; b v1 published".into())
        );
        assert_eq!(
            auto_notice("x published".into(), None, vec![ok("a v2 published"), Err("b: locked".into())]),
            (Level::Warn, "x published; auto publish: a v2 published; b: locked".into())
        );
        assert_eq!(
            auto_notice("x published".into(), Some("Library not changed".into()), vec![ok("a v2 published")]),
            (Level::Warn, "x published; Library not changed; auto publish: a v2 published".into())
        );
    }

    #[tokio::test]
    async fn resume_of_an_older_version_says_library_keeps_the_newer_one() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let v1 = st.begin_version(web, "", &[]).await.unwrap();
        st.finish_version(v1.id, "failed", Some("copy failed")).await.unwrap();
        let v2 = publish_empty(&st, web).await;
        let lib = st.ensure_library("lab").await.unwrap();
        st.promoted(lib, web, v2.id, "active").await.unwrap();
        let c = st.create_composite("lab", "rocky9", "").await.unwrap();
        st.set_component(c, web, None).await.unwrap();
        st.set_auto_publish(c, true).await.unwrap();
        app.push(Kind::CvVersions { cv: web, name: "web".into() });
        let item = json!({ "id": v1.id, "number": 1, "state": "failed" });
        version_action(&mut app, "lab", web, "web".into(), Act::Resume, item);
        drain(&mut app, &mut rx).await;
        assert_eq!(last_notice(&app), (Level::Warn, "web v1 published; Library not changed: v2 is newer (use Rollback to serve v1)"));
        assert_eq!(st.binding(lib, web).await.unwrap().unwrap().active(), Some(v2.id));
        assert!(st.versions(c).await.unwrap().is_empty(), "composites follow v2: nothing new to auto publish");
    }

    #[tokio::test]
    async fn a_failed_library_step_keeps_the_version_published_and_auto_publishes() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let extras = st.create_cv("lab", "extras", "").await.unwrap();
        let v1 = st.begin_version(extras, "", &[]).await.unwrap();
        let c = st.create_composite("lab", "rocky9", "").await.unwrap();
        st.set_component(c, extras, None).await.unwrap();
        st.set_auto_publish(c, true).await.unwrap();
        // Library still serves "old" of extras, and Pulp refuses the unbind of its distribution.
        let lib = st.ensure_library("lab").await.unwrap();
        let dist = "/pulp/api/v3/distributions/rpm/rpm/1/";
        let row = crate::store::BindingRepo {
            env_id: lib,
            cv_id: extras,
            repo_href: format!("{REPOS}rpm/rpm/9/"),
            repo_name: "old".into(),
            repo_type: RepoType::Rpm,
            distribution: dist.into(),
            base_path: "library/extras/old".into(),
            publication: Some("/p/1/".into()),
        };
        st.save_binding_repo(&row).await.unwrap();
        let labels = json!({ crate::cv::LABEL_CV: "extras", crate::cv::LABEL_ENV: LIBRARY, crate::cv::LABEL_DB: st.db_id() });
        serve(&server, "GET", dist.into(), 200, json!({ "pulp_labels": labels })).await;
        serve(&server, "PATCH", dist.into(), 403, json!({ "detail": "no" })).await;
        app.push(Kind::CvVersions { cv: extras, name: "extras".into() });
        version_action(&mut app, "lab", extras, "extras".into(), Act::Resume, json!({ "id": v1.id, "number": 1, "state": "failed" }));
        drain(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm: {:?}", last_notice(&app)) };
        assert_eq!(lines[2..], ["In Library:", "  old: not part of v1 -> its distribution stops serving (kept for rollback)"]);
        yes(&mut app);
        drain(&mut app, &mut rx).await;
        let (level, text) = last_notice(&app);
        assert_eq!(level, Level::Warn);
        assert!(text.starts_with("extras v1 published; Library step failed: extras in Library: failed: unbind old: "), "{text}");
        assert!(text.ends_with("; promote v1 to Library; auto publish: rocky9 v1 published"), "{text}");
        assert_eq!(st.version(v1.id).await.unwrap().state, "published");
        assert_eq!(st.versions(c).await.unwrap().len(), 1, "the composite auto published");
    }

    #[tokio::test]
    async fn publish_and_resume_ask_to_type_library_when_it_is_protected() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        // A composite of a version without repositories: preview and publish send nothing to Pulp.
        let os = st.create_cv("lab", "os", "").await.unwrap();
        publish_empty(&st, os).await;
        let c = st.create_composite("lab", "rocky9", "").await.unwrap();
        st.set_component(c, os, None).await.unwrap();
        app.push(Kind::ContentViews);
        let publish = |app: &mut App| cv_action(app, "lab", Act::CvPublish, json!({ "id": c, "name": "rocky9", "composite": true }));
        let submit_form = |app: &mut App| {
            let (form, mut submit) = open_form(app);
            submit(app, &form).unwrap();
        };
        let confirm = |app: &mut App| match app.modal.take() {
            Some(Modal::Confirm { title, typed, on_yes, .. }) => (title, typed, on_yes.unwrap()),
            _ => panic!("no confirmation"),
        };
        publish(&mut app);
        drain(&mut app, &mut rx).await; // the form opens after the store read
        submit_form(&mut app);
        drain(&mut app, &mut rx).await;
        assert_eq!(confirm(&mut app).1, None, "an open Library takes a plain confirmation");

        let lib = st.ensure_library("lab").await.unwrap();
        st.set_env_protected(lib, true).await.unwrap();
        publish(&mut app);
        drain(&mut app, &mut rx).await;
        submit_form(&mut app);
        drain(&mut app, &mut rx).await;
        let (title, typed, on_yes) = confirm(&mut app);
        assert_eq!((title.as_str(), typed.as_deref()), ("Library is protected", Some(LIBRARY)));
        assert!(st.versions(c).await.unwrap().is_empty(), "nothing before the confirmation");
        on_yes(&mut app);
        drain(&mut app, &mut rx).await;
        assert_eq!(last_notice(&app), (Level::Ok, "rocky9 version 1 published and active in Library"));

        let v2 = st.begin_version(os, "", &[]).await.unwrap();
        st.finish_version(v2.id, "failed", None).await.unwrap();
        version_action(&mut app, "lab", os, "os".into(), Act::Resume, json!({ "id": v2.id, "number": 2, "state": "failed" }));
        drain(&mut app, &mut rx).await;
        let (title, typed, on_yes) = confirm(&mut app);
        assert_eq!((title.as_str(), typed.as_deref()), ("Library is protected", Some(LIBRARY)));
        assert_eq!(st.version(v2.id).await.unwrap().state, "failed", "nothing before the confirmation");
        on_yes(&mut app);
        drain(&mut app, &mut rx).await;
        assert_eq!(st.version(v2.id).await.unwrap().state, "published");
    }

    /// The Publish preview says first what Publish does, then compares the draft with the newest published version (of a
    /// composite: its components).
    #[tokio::test]
    async fn the_publish_preview_compares_the_draft_with_the_newest_published_version() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        // A composite of a version without repositories: preview and publish send nothing to Pulp.
        let os = st.create_cv("lab", "os", "").await.unwrap();
        publish_empty(&st, os).await;
        let c = st.create_composite("lab", "rocky9", "").await.unwrap();
        st.set_component(c, os, None).await.unwrap();
        app.push(Kind::ContentViews);
        let mut heads = vec![];
        for _ in 0..2 {
            cv_action(&mut app, "lab", Act::CvPublish, json!({ "id": c, "name": "rocky9", "composite": true }));
            drain(&mut app, &mut rx).await;
            let (form, mut submit) = open_form(&mut app);
            submit(&mut app, &form).unwrap();
            drain(&mut app, &mut rx).await;
            let Some(Modal::Confirm { lines, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirmation") };
            heads.push(lines[..4].to_vec());
            yes(&mut app);
            drain(&mut app, &mut rx).await;
        }
        let first = "Publishing creates a new immutable version and makes it active in Library.";
        let head = |os: &str| vec![first.to_string(), String::new(), "== components".into(), os.into()];
        assert_eq!(heads, [head("  os: added (v1)"), head("  os: v1 (unchanged)")]);
    }

    /// F46: the Library step of Publish changes the Library rows of repositories that the new version lacks, as a
    /// promotion does: the Publish preview names each change before the confirmation. A kept distribution that serves
    /// a publication again stops serving; one that lost the labels of the view is left as it is and forgotten.
    #[tokio::test]
    async fn the_publish_preview_names_what_the_library_step_changes() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        // A composite of a version without repositories: the preview reads only the left-out rows from Pulp.
        let os = st.create_cv("lab", "os", "").await.unwrap();
        publish_empty(&st, os).await;
        let c = st.create_composite("lab", "rocky9", "").await.unwrap();
        st.set_component(c, os, None).await.unwrap();
        let lib = st.ensure_library("lab").await.unwrap();
        let ours = json!({ crate::cv::LABEL_CV: "rocky9", crate::cv::LABEL_ENV: LIBRARY, crate::cv::LABEL_DB: st.db_id() });
        for (n, labels) in [("1", ours), ("2", json!({}))] {
            let old = left_out(&st, lib, c, n, None).await;
            let d = json!({ "pulp_href": old.distribution, "name": format!("d{n}"), "base_path": old.base_path, "publication": "/p/3/",
                "pulp_labels": labels });
            serve(&server, "GET", old.distribution, 200, d).await;
        }
        cv_action(&mut app, "lab", Act::CvPublish, json!({ "id": c, "name": "rocky9", "composite": true }));
        drain(&mut app, &mut rx).await; // the form opens after the store read
        let (form, mut submit) = open_form(&mut app);
        submit(&mut app, &form).unwrap();
        drain(&mut app, &mut rx).await;
        let lines = confirm_lines(&mut app);
        let r1 = "  r1: not part of the new version -> its distribution stops serving (kept for rollback); Pulp shows that it \
                  serves content set outside pulp-tui";
        let r2 = "  r2: not part of the new version -> its distribution d2 is not labeled pulp_tui_cv=rocky9 (changed outside \
                  pulp-tui): pulp-tui leaves it as it is and forgets it";
        assert_eq!(lines[lines.len() - 3..], ["In Library:", r1, r2], "{lines:?}");
        assert!(st.versions(c).await.unwrap().is_empty(), "nothing before the confirmation");
    }

    /// A view that is not a composite: the Library step compares its rows with the repositories of the definition. The
    /// row of a repository that the view still has stays as it is; the row of one that it dropped stops serving.
    #[tokio::test]
    async fn the_publish_preview_of_a_view_names_the_library_row_of_a_dropped_repository() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let lib = st.ensure_library("lab").await.unwrap();
        // The definition keeps r1 (no versions yet: the preview reads only the repository) and dropped r2.
        let kept = left_out(&st, lib, web, "1", Some("/p/1/")).await;
        let r1 = CvRepo { repo_href: kept.repo_href.clone(), repo_name: "r1".into(), repo_type: RepoType::Rpm, filters: vec![] };
        st.set_cv_repo(web, &r1).await.unwrap();
        serve(&server, "GET", kept.repo_href.clone(), 200, json!({ "pulp_href": kept.repo_href, "name": "r1" })).await;
        let dropped = left_out(&st, lib, web, "2", Some("/p/2/")).await;
        let ours = json!({ crate::cv::LABEL_CV: "web", crate::cv::LABEL_ENV: LIBRARY, crate::cv::LABEL_DB: st.db_id() });
        serve(&server, "GET", dropped.distribution, 200, json!({ "name": "d2", "publication": "/p/2/", "pulp_labels": ours })).await;
        serve_distributions(&server, &[], None).await; // the Library target of r1: nothing refuses it
        cv_action(&mut app, "lab", Act::CvPublish, json!({ "id": web, "name": "web" }));
        drain(&mut app, &mut rx).await;
        let (form, mut submit) = open_form(&mut app);
        submit(&mut app, &form).unwrap();
        drain(&mut app, &mut rx).await;
        let lines = confirm_lines(&mut app);
        let r2 = "  r2: not part of the new version -> its distribution stops serving (kept for rollback)";
        assert_eq!(lines[lines.len() - 2..], ["In Library:", r2], "{lines:?}");
    }

    /// Mock answers for distribution `name` (the `name` filter of the RPM distributions) and for the overlap lists of
    /// `distributions/`: `found` for each, else nothing.
    async fn serve_distributions(server: &MockServer, named: &[(String, Value)], overlap: Option<(&str, Value)>) {
        let none = json!({ "count": 0, "next": null, "results": [] });
        let page = |d: Value| json!({ "count": 1, "next": null, "results": [d] });
        for (name, d) in named {
            Mock::given(method("GET"))
                .and(path("/pulp/api/v3/distributions/rpm/rpm/"))
                .and(wiremock::matchers::query_param("name", name.as_str()))
                .respond_with(ResponseTemplate::new(200).set_body_json(page(d.clone())))
                .with_priority(1)
                .mount(server)
                .await;
        }
        if let Some((at_or_above, d)) = overlap {
            Mock::given(method("GET"))
                .and(path("/pulp/api/v3/distributions/"))
                .and(wiremock::matchers::query_param("base_path__in", at_or_above))
                .respond_with(ResponseTemplate::new(200).set_body_json(page(d)))
                .with_priority(1)
                .mount(server)
                .await;
        }
        for p in ["/pulp/api/v3/distributions/rpm/rpm/", "/pulp/api/v3/distributions/"] {
            serve(server, "GET", p.into(), 200, none.clone()).await;
        }
    }

    /// The lines of the Publish preview of view `web` (`id`).
    async fn publish_preview_lines(
        app: &mut App,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::tui::app::Msg>,
        id: i64,
    ) -> Vec<String> {
        cv_action(app, "lab", Act::CvPublish, json!({ "id": id, "name": "web" }));
        drain(app, rx).await;
        let (form, mut submit) = open_form(app);
        submit(app, &form).unwrap();
        drain(app, rx).await;
        confirm_lines(app)
    }

    /// F55: the distribution of the derived name of r1 lost the labels of the view (an operator took it over), so the run
    /// refuses it (`ops::ensure_distribution`). The Publish preview (its Library step) and the Promote preview name that
    /// refusal and the way out, before anything is sent.
    #[tokio::test]
    async fn the_previews_name_a_taken_over_derived_distribution_that_the_promotion_refuses() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let r1 = cv_repo("r1", "rpm", "1");
        st.set_cv_repo(web, &r1).await.unwrap();
        serve(&server, "GET", r1.repo_href.clone(), 200, json!({ "pulp_href": r1.repo_href, "name": "r1" })).await;
        let lib = st.ensure_library("lab").await.unwrap();
        st.create_env("lab", "Dev", lib, false).await.unwrap();
        let theirs = |env: &str| {
            let name = crate::cv::distribution_name("web", env, "r1");
            let d = json!({ "pulp_href": "/pulp/api/v3/distributions/rpm/rpm/5/", "name": name,
                "base_path": format!("{}/web/r1", env.to_lowercase()), "pulp_labels": { "team": "x" } });
            (name, d)
        };
        serve_distributions(&server, &[theirs(LIBRARY), theirs("Dev")], None).await;
        let refused = |env: &str| {
            format!(
                "distribution pulp-tui-cv-web-{env}-r1 exists but lacks ownership labels {{\"pulp_tui_cv\":\"web\",\"pulp_tui_env\":\"{env}\"}}: \
                 restore them or delete the distribution, then try again"
            )
        };

        let lines = publish_preview_lines(&mut app, &mut rx, web).await;
        let library = format!("  r1 (RPM): the Library step fails for it: {}", refused(LIBRARY));
        assert_eq!(lines[lines.len() - 2..], ["In Library:".to_string(), library], "{lines:?}");
        assert!(st.versions(web).await.unwrap().is_empty(), "nothing before the confirmation");

        let v = st.begin_version(web, "", std::slice::from_ref(&r1)).await.unwrap();
        st.finish_version(v.id, "published", None).await.unwrap();
        st.promoted(lib, web, v.id, "active").await.unwrap();
        promote_preview(&mut app, "web".into(), v.id, "Dev".into(), Mode::Promote);
        drain(&mut app, &mut rx).await;
        let dev = format!("  r1 (RPM): distribution pulp-tui-cv-web-Dev-r1 at /dev/web/r1; the promotion fails for it: {}", refused("Dev"));
        assert_eq!(confirm_lines(&mut app)[2], dev);
        assert!(server.received_requests().await.unwrap().iter().all(|r| r.method.as_str() == "GET"), "reads only");
    }

    /// F57: a distribution outside pulp-tui at `library/web` is above the new Library path of r1, so Pulp refuses the new
    /// distribution (`ops::overlapping_distribution`). The Publish preview names it for the Library step.
    #[tokio::test]
    async fn the_publish_preview_names_a_library_base_path_that_overlaps_another() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let r1 = cv_repo("r1", "rpm", "1");
        st.set_cv_repo(web, &r1).await.unwrap();
        serve(&server, "GET", r1.repo_href.clone(), 200, json!({ "pulp_href": r1.repo_href, "name": "r1" })).await;
        let foreign = json!({ "pulp_href": "/d/9/", "name": "foreign", "base_path": "library/web" });
        serve_distributions(&server, &[], Some(("library,library/web,library/web/r1", foreign))).await;
        let lines = publish_preview_lines(&mut app, &mut rx, web).await;
        let r1 = "  r1 (RPM): the Library step fails for it: base path library/web/r1 overlaps base path library/web of distribution \
                  foreign";
        assert_eq!(lines[lines.len() - 2..], ["In Library:", r1], "{lines:?}");
    }

    /// A view without repositories: Publish refuses it, so its preview says that refusal, not the Library rows that a
    /// Library step would change.
    #[tokio::test]
    async fn the_publish_preview_of_a_view_without_repositories_says_that_publish_refuses_it() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let lib = st.ensure_library("lab").await.unwrap();
        let old = left_out(&st, lib, web, "2", Some("/p/2/")).await;
        serve(&server, "GET", old.distribution, 200, json!({ "name": "d2", "publication": "/p/2/" })).await;
        cv_action(&mut app, "lab", Act::CvPublish, json!({ "id": web, "name": "web" }));
        drain(&mut app, &mut rx).await;
        let (form, mut submit) = open_form(&mut app);
        submit(&mut app, &form).unwrap();
        drain(&mut app, &mut rx).await;
        assert!(app.modal.is_none(), "no confirmation");
        let (level, text) = last_notice(&app);
        assert_eq!((level, text), (Level::Error, "preview web: content view web has no repositories"));
    }

    /// F58: the Library step of a Resume changes the Library rows of the repositories that the version lacks, as the one
    /// of Publish does. The Resume confirmation names each change under "In Library:" (an open Library too), and nothing
    /// runs before it: for a failed version, and for a published one whose Library step was interrupted. An older
    /// published version does not change that; a newer one keeps Library: then there is no such line.
    #[tokio::test]
    async fn the_resume_confirmation_names_what_the_library_step_changes() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let lib = st.ensure_library("lab").await.unwrap();
        let old = left_out(&st, lib, web, "2", Some("/p/2/")).await;
        let ours = json!({ crate::cv::LABEL_CV: "web", crate::cv::LABEL_ENV: LIBRARY, crate::cv::LABEL_DB: st.db_id() });
        serve(&server, "GET", old.distribution, 200, json!({ "name": "d2", "publication": "/p/2/", "pulp_labels": ours })).await;
        publish_empty(&st, web).await; // v1
        let v = st.begin_version(web, "", &[]).await.unwrap();
        st.finish_version(v.id, "failed", None).await.unwrap();
        let resume = |app: &mut App, state: &str| {
            version_action(app, "lab", web, "web".into(), Act::Resume, json!({ "id": v.id, "number": v.number, "state": state }))
        };
        let first = "Resume web v2: it becomes active in Library unless a newer version is published.";
        let r2 = "  r2: not part of v2 -> its distribution stops serving (kept for rollback)";
        let want = vec![first.to_string(), String::new(), "In Library:".into(), r2.into()];
        for state in ["failed", "published"] {
            if state == "published" {
                st.finish_version(v.id, state, None).await.unwrap();
                st.promoted(lib, web, v.id, "promoting").await.unwrap(); // the client stopped in the Library step
            }
            resume(&mut app, state);
            drain(&mut app, &mut rx).await;
            let Some(Modal::Confirm { title, lines, typed: None, .. }) = app.modal.take() else {
                panic!("{state}: no confirm: {:?}", app.notices.back().map(|n| &n.text))
            };
            assert_eq!((title.as_str(), &lines), ("Resume web v2", &want), "{state}");
            assert_eq!(st.version(v.id).await.unwrap().state, state, "nothing before the confirmation");
        }

        let newer = publish_empty(&st, web).await;
        assert!(newer.number > v.number);
        resume(&mut app, "published");
        drain(&mut app, &mut rx).await;
        assert!(app.modal.is_none(), "Library keeps v{}: no Library line, no confirmation", newer.number);
    }

    #[tokio::test]
    async fn a_failed_resume_shows_the_failed_version_without_a_reload() {
        let server = MockServer::start().await; // 404 for the rest: the derived repository cannot be found
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        serve_distributions(&server, &[], None).await; // the Library target of os (Resume preview): nothing refuses it
        let web = st.create_cv("lab", "web", "").await.unwrap();
        st.begin_version(web, "", &[cv_repo("os", "rpm", "1")]).await.unwrap();
        app.push(Kind::CvVersions { cv: web, name: "web".into() });
        drain(&mut app, &mut rx).await;
        let item = app.list.items[0].clone();
        assert_eq!(item["state"], "interrupted");
        version_action(&mut app, "lab", web, "web".into(), Act::Resume, item);
        drain(&mut app, &mut rx).await;
        assert_eq!(last_notice(&app).0, Level::Error);
        assert_eq!(app.list.items[0]["state"], "failed");
    }

    #[tokio::test]
    async fn create_makes_a_composite_view_on_request() {
        let (mut app, mut rx, st) = composite_app().await;
        for (composite, msg) in [
            (true, "composite content view rocky9 created; add components with a"),
            (false, "content view plain created; add repositories with a"),
        ] {
            cv_action(&mut app, "lab", Act::Create, json!({}));
            let (mut form, mut submit) = open_form(&mut app);
            let last = form.fields.last().unwrap();
            assert_eq!((last.key.as_str(), last.label.as_str()), ("composite", "Composite (fixed after create)"));
            assert_eq!(last.kind, FieldKind::Bool);
            set(&mut form, "name", if composite { "rocky9" } else { "plain" });
            set(&mut form, "composite", &composite.to_string());
            submit(&mut app, &form).unwrap();
            drain(&mut app, &mut rx).await;
            assert_eq!(last_notice(&app), (Level::Ok, msg));
        }
        let views: Vec<_> = st.content_views("lab").await.unwrap().into_iter().map(|v| (v.name, v.composite)).collect();
        assert_eq!(views, [("plain".to_string(), false), ("rocky9".to_string(), true)]);
    }

    /// A composite `rocky9` (auto publish off) with `os` (v1, v2) pinned to v1 and `extras` (v1) on latest.
    async fn rocky(st: &Store) -> (i64, i64, i64, Value) {
        let (os, extras) = (st.create_cv("lab", "os", "").await.unwrap(), st.create_cv("lab", "extras", "").await.unwrap());
        let v1 = publish_empty(st, os).await;
        publish_empty(st, os).await;
        publish_empty(st, extras).await;
        let c = st.create_composite("lab", "rocky9", "old").await.unwrap();
        st.set_component(c, os, Some(v1.id)).await.unwrap();
        st.set_component(c, extras, None).await.unwrap();
        let item = json!({
            "id": c, "name": "rocky9", "description": "old", "composite": true, "auto_publish": false,
            "components": [{ "id": extras, "name": "extras" }, { "id": os, "name": "os" }]
        });
        (c, os, extras, item)
    }

    #[tokio::test]
    async fn edit_of_a_composite_sets_description_auto_publish_and_changed_pins() {
        let (mut app, mut rx, st) = composite_app().await;
        let (c, os, extras, item) = rocky(&st).await;
        cv_action(&mut app, "lab", Act::Edit, item);
        assert!(app.modal.is_none(), "the components are read first");
        drain(&mut app, &mut rx).await;
        let (mut form, mut submit) = open_form(&mut app);
        let keys: Vec<_> = form.fields.iter().map(|f| f.key.clone()).collect();
        assert_eq!(keys, ["description", "auto", &extras.to_string(), &os.to_string()]);
        assert_eq!(form.fields[1].label, "Auto publish when a latest component publishes");
        assert_eq!((form.fields[1].kind.clone(), form.fields[1].value.as_str()), (FieldKind::Bool, "false"));
        let (e, o) = (&form.fields[2], &form.fields[3]);
        assert_eq!(
            (e.label.as_str(), e.value.as_str(), &e.kind),
            ("extras", "latest", &FieldKind::Choice(vec!["latest".into(), "v1".into()]))
        );
        let want = FieldKind::Choice(vec!["latest".into(), "v2".into(), "v1".into()]);
        assert_eq!((o.label.as_str(), o.value.as_str(), &o.kind), ("os", "v1", &want), "the current pin is preselected");

        set(&mut form, "description", "new");
        set(&mut form, "auto", "true");
        set(&mut form, &os.to_string(), "latest");
        set(&mut form, &extras.to_string(), "v1");
        submit(&mut app, &form).unwrap();
        drain(&mut app, &mut rx).await;
        assert_eq!(last_notice(&app), (Level::Ok, "saved"));
        let cv = st.content_view(c).await.unwrap();
        assert_eq!((cv.description.as_str(), cv.auto_publish), ("new", true));
        let pins: Vec<_> = st.components(c).await.unwrap().into_iter().map(|k| (k.name, k.pinned.is_some())).collect();
        assert_eq!(pins, [("extras".to_string(), true), ("os".to_string(), false)]);
    }

    #[tokio::test]
    async fn edit_of_a_normal_view_keeps_the_description_form() {
        let (mut app, _rx, _st) = composite_app().await;
        cv_action(&mut app, "lab", Act::Edit, json!({ "id": 1, "name": "web", "description": "d", "composite": false }));
        let (form, _) = open_form(&mut app);
        assert_eq!(form.fields.iter().map(|f| f.key.as_str()).collect::<Vec<_>>(), ["description"]);
    }

    #[tokio::test]
    async fn add_components_pins_each_and_checks_repository_clashes() {
        let (mut app, mut rx, st) = composite_app().await;
        let (c, os, extras, item) = rocky(&st).await;
        let base = cv_repo("baseos", "rpm", "1");
        st.set_cv_repo(os, &base).await.unwrap();
        let (tools, dup) = (st.create_cv("lab", "tools", "").await.unwrap(), st.create_cv("lab", "dup", "").await.unwrap());
        publish_empty(&st, tools).await;
        publish_empty(&st, tools).await;
        publish_empty(&st, dup).await;
        st.set_cv_repo(tools, &cv_repo("tools", "rpm", "2")).await.unwrap();
        st.set_cv_repo(dup, &base).await.unwrap(); // the same repository as os

        cv_action(&mut app, "lab", Act::CvAddComponent, item.clone());
        let (mut form, mut submit) = open_form(&mut app);
        assert_eq!(form.title, "Add components to rocky9");
        let f = &form.fields[0];
        assert_eq!((f.key.as_str(), f.label.as_str(), f.required), ("components", "Content Views", true));
        assert_eq!(f.kind, FieldKind::Pick { source: Source::ContentViews, multi: true });
        assert_eq!(f.exclude, [extras.to_string(), os.to_string()], "the current components are not listed");
        let pick = |cv: i64, name: &str| PickItem { label: name.into(), value: cv.to_string(), note: String::new() };

        // A clash: dup holds the repository of os. Nothing is added.
        form.fields[0].options = Some(vec![pick(dup, "dup"), pick(tools, "tools")]);
        set(&mut form, "components", &dup.to_string());
        submit(&mut app, &form).unwrap();
        drain(&mut app, &mut rx).await;
        let (mut pins, mut pin_submit) = open_form(&mut app);
        assert_eq!(pins.title, "Pin components of rocky9");
        assert_eq!(pins.fields.iter().map(|f| (f.label.as_str(), f.value.as_str())).collect::<Vec<_>>(), [("dup", "latest")]);
        pin_submit(&mut app, &pins).unwrap();
        drain(&mut app, &mut rx).await;
        let (level, text) = last_notice(&app);
        assert_eq!(level, Level::Error);
        assert!(text.contains("components os and dup both contain repository baseos"), "{text}");
        assert_eq!(st.components(c).await.unwrap().len(), 2);

        // A clean add, pinned to the older version.
        set(&mut form, "components", &tools.to_string());
        submit(&mut app, &form).unwrap();
        drain(&mut app, &mut rx).await;
        (pins, pin_submit) = open_form(&mut app);
        assert_eq!(pins.fields[0].kind, FieldKind::Choice(vec!["latest".into(), "v2".into(), "v1".into()]));
        set(&mut pins, &tools.to_string(), "v1");
        pin_submit(&mut app, &pins).unwrap();
        drain(&mut app, &mut rx).await;
        assert_eq!(last_notice(&app), (Level::Ok, "added 1 component(s) (takes effect on next publish)"));
        let added = st.components(c).await.unwrap().into_iter().find(|k| k.cv_id == tools).unwrap();
        let v1 = st.versions(tools).await.unwrap().into_iter().find(|v| v.number == 1).unwrap();
        assert_eq!(added.pinned, Some(v1.id));
    }

    #[tokio::test]
    async fn add_components_refuses_a_typed_name() {
        let (mut app, _rx, st) = composite_app().await;
        let (_, _, _, item) = rocky(&st).await;
        cv_action(&mut app, "lab", Act::CvAddComponent, item);
        let (mut form, mut submit) = open_form(&mut app);
        set(&mut form, "components", "tools");
        assert_eq!(submit(&mut app, &form), Err("pick Content Views from the list".into()));
        assert!(app.jobs.is_empty());
    }

    #[tokio::test]
    async fn remove_component_keeps_the_others_and_says_so() {
        let (mut app, mut rx, st) = composite_app().await;
        let (c, os, extras, item) = rocky(&st).await;
        cv_action(&mut app, "lab", Act::CvRemoveComponent, item);
        let (mut form, mut submit) = open_form(&mut app);
        assert_eq!(form.title, "Remove component from rocky9");
        assert_eq!(form.fields[0].kind, FieldKind::Choice(vec!["extras".into(), "os".into()]));
        set(&mut form, "component", "os");
        submit(&mut app, &form).unwrap();
        drain(&mut app, &mut rx).await;
        assert_eq!(last_notice(&app), (Level::Ok, "os removed from rocky9; published versions are unchanged"));
        assert_eq!(st.components(c).await.unwrap().iter().map(|k| k.cv_id).collect::<Vec<_>>(), [extras]);
        assert_eq!(st.versions(os).await.unwrap().len(), 2, "the component's versions are unchanged");
        // No components: an error notice, no form.
        cv_action(&mut app, "lab", Act::CvRemoveComponent, json!({ "id": c, "name": "rocky9", "composite": true, "components": [] }));
        assert!(app.modal.is_none());
        assert_eq!(last_notice(&app), (Level::Error, "composite has no components"));
    }

    #[tokio::test]
    async fn add_repositories_refuses_a_path_clash() {
        let (mut app, mut rx, st) = composite_app().await;
        let web = st.create_cv("lab", "web", "").await.unwrap();
        st.set_cv_repo(web, &cv_repo("a b", "rpm", "1")).await.unwrap();
        cv_action(&mut app, "lab", Act::CvAddRepo, json!({ "id": web, "name": "web", "repos": [] }));
        let (mut form, mut submit) = open_form(&mut app);
        form.fields[0].options = Some(vec![item("a-b", "rpm")]);
        set(&mut form, "repos", &item("a-b", "rpm").value);
        submit(&mut app, &form).unwrap();
        drain(&mut app, &mut rx).await;
        let (level, text) = last_notice(&app);
        assert_eq!(level, Level::Error);
        assert!(text.contains("same path segment a-b"), "{text}");
        assert_eq!(st.cv_repos(web).await.unwrap().len(), 1);
    }

    /// The publish job of extras v2 ended (Connect, quit, crash) after the publish and before the auto publish of rocky9.
    #[tokio::test]
    async fn resume_runs_the_auto_publish_that_an_aborted_job_left_out() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let (c, _, extras, _) = rocky(&st).await;
        st.set_auto_publish(c, true).await.unwrap();
        let svc = crate::cv::CvService { client: client(&server), store: st.clone(), scope: "lab".into(), actor: "t".into(), lock: None };
        svc.publish(c, "", "", &|_| {}, &ops::no_progress).await.unwrap(); // rocky9 v1 uses extras v1
        let v2 = publish_empty(&st, extras).await; // the aborted job: published, no auto publish
        app.push(Kind::CvVersions { cv: extras, name: "extras".into() });
        let (v1, resume) = (st.versions(extras).await.unwrap()[1].clone(), |app: &mut App, v: &CvVersion| {
            // The list is stale: it shows `failed`, the database says `published`.
            let item = json!({ "id": v.id, "number": v.number, "state": "failed" });
            version_action(app, "lab", extras, "extras".into(), Act::Resume, item);
        });
        resume(&mut app, &v1);
        drain(&mut app, &mut rx).await;
        assert_eq!(last_notice(&app), (Level::Info, "version is already published"), "composites follow the newest version");
        resume(&mut app, &v2);
        drain(&mut app, &mut rx).await;
        assert_eq!(last_notice(&app), (Level::Ok, "extras v2 published; auto publish: rocky9 v2 published"));
        let used = st.version_components(st.versions(c).await.unwrap()[0].id).await.unwrap();
        assert!(used.iter().any(|u| u.version_id == v2.id), "{used:?}");
        resume(&mut app, &v2);
        drain(&mut app, &mut rx).await;
        assert_eq!(last_notice(&app), (Level::Info, "version is already published"));
        assert_eq!(st.versions(c).await.unwrap().len(), 2, "no third composite version");
    }

    /// Only the auto publish of a lagging composite is left, and Library is protected (auto publish does not run then):
    /// one refusal that says why, with no typed Library.
    #[tokio::test]
    async fn resume_refuses_an_auto_publish_while_library_is_protected() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let (c, _, extras, _) = rocky(&st).await;
        st.set_auto_publish(c, true).await.unwrap();
        let svc = crate::cv::CvService { client: client(&server), store: st.clone(), scope: "lab".into(), actor: "t".into(), lock: None };
        svc.publish(c, "", "", &|_| {}, &ops::no_progress).await.unwrap(); // rocky9 v1 uses extras v1
        let v2 = publish_empty(&st, extras).await; // the aborted job: published, no auto publish
        st.set_env_protected(st.ensure_library("lab").await.unwrap(), true).await.unwrap();
        let item = json!({ "id": v2.id, "number": v2.number, "state": "published" });
        version_action(&mut app, "lab", extras, "extras".into(), Act::Resume, item);
        drain(&mut app, &mut rx).await;
        assert!(app.modal.is_none(), "no typed Library");
        let why = "check resume of extras v2: v2 is published; only the auto publish of rocky9 is left, and it does not run \
                   while Library is protected: publish rocky9 yourself";
        assert_eq!(last_notice(&app), (Level::Error, why));
        assert_eq!(st.versions(c).await.unwrap().len(), 1, "no composite version");
    }

    #[tokio::test]
    async fn resume_finishes_an_interrupted_library_step_and_auto_publishes() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let v = publish_empty(&st, web).await;
        let lib = st.ensure_library("lab").await.unwrap();
        st.promoted(lib, web, v.id, "promoting").await.unwrap(); // the client stopped in the Library step
        let c = st.create_composite("lab", "rocky9", "").await.unwrap();
        st.set_component(c, web, None).await.unwrap();
        st.set_auto_publish(c, true).await.unwrap();
        app.push(Kind::CvVersions { cv: web, name: "web".into() });
        let item = json!({ "id": v.id, "number": 1, "state": "published" });
        version_action(&mut app, "lab", web, "web".into(), Act::Resume, item.clone());
        drain(&mut app, &mut rx).await;
        assert_eq!(last_notice(&app), (Level::Ok, "web v1 published; auto publish: rocky9 v1 published"));
        assert_eq!(st.binding(lib, web).await.unwrap().unwrap().active(), Some(v.id));
        assert_eq!(st.versions(c).await.unwrap().len(), 1);
        // Library is active now: nothing is left to do.
        version_action(&mut app, "lab", web, "web".into(), Act::Resume, item);
        drain(&mut app, &mut rx).await;
        assert_eq!(last_notice(&app), (Level::Info, "version is already published"));
        assert_eq!(st.versions(c).await.unwrap().len(), 1, "no second composite version");
    }

    /// The "environments" cell of view `cv` on the Content Views screen, and the one of version `v` on its Versions screen.
    async fn shown_envs(st: &Store, cv: i64, v: i64) -> (String, String) {
        let cfg = &Config::default();
        let load = |k: Kind| async move { super::super::kinds::load(&k, None, Ok(st), "lab", "lab", cfg, 0, "").await.unwrap().0 };
        let views = load(Kind::ContentViews).await;
        let versions = load(Kind::CvVersions { cv, name: String::new() }).await;
        let version = versions.iter().find(|x| x["id"] == v).unwrap();
        (s(&views[0], "bound"), s(version, "environments"))
    }

    #[tokio::test]
    async fn a_failed_promotion_keeps_the_served_version_bound() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let svc = crate::cv::CvService { client: client(&server), store: st.clone(), scope: "lab".into(), actor: "t".into(), lock: None };
        let lib = st.ensure_library("lab").await.unwrap();
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let v1 = publish_empty(&st, web).await;
        svc.promote(web, v1.id, lib, Mode::Rollback, &|_| {}, &ops::no_progress).await.unwrap();
        // v2 cannot be served: its only repository has no publication, so every repository fails.
        let v2 = st.begin_version(web, "", &[cv_repo("os", "rpm", "1")]).await.unwrap();
        st.finish_version(v2.id, "published", None).await.unwrap();
        let e = svc.promote(web, v2.id, lib, Mode::Rollback, &|_| {}, &ops::no_progress).await.unwrap_err();
        assert!(e.to_string().starts_with("web in Library: failed"), "{e}");
        assert_eq!(shown_envs(&st, web, v1.id).await, ("Library:v1 (v2 failed)".into(), "Library".into()));
        assert_eq!(shown_envs(&st, web, v2.id).await.1, "Library (failed)");
        // The served v1 is the rollback target: its delete is refused before the typed confirmation.
        let row = json!({ "id": v1.id, "number": 1, "state": "published" });
        version_action(&mut app, "lab", web, "web".into(), Act::Delete, row);
        drain(&mut app, &mut rx).await;
        assert!(app.modal.is_none(), "no delete confirmation");
        assert_eq!(last_notice(&app), (Level::Error, "read what deleting web v1 removes: version 1 is bound to Library"));
        svc.promote(web, v1.id, lib, Mode::Rollback, &|_| {}, &ops::no_progress).await.unwrap();
        assert_eq!(shown_envs(&st, web, v1.id).await, ("Library:v1".into(), "Library".into()));
        assert_eq!(shown_envs(&st, web, v2.id).await.1, "", "the failed attempt is gone after the rollback");
    }

    /// The distribution of `os` in Dev was renamed in Pulp: the preview names it as Pulp has it, not as derived. At the
    /// derived path the promotion fails; at another path it makes a new distribution. A foreign distribution has the
    /// path of the new docs distribution: the promotion fails for docs.
    #[tokio::test]
    async fn the_promote_preview_shows_a_distribution_renamed_outside_pulp_tui() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let lib = st.ensure_library("lab").await.unwrap();
        let dev = st.create_env("lab", "Dev", lib, false).await.unwrap();
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let v = st.begin_version(web, "", &[cv_repo("os", "rpm", "1"), cv_repo("docs", "file", "2")]).await.unwrap();
        st.finish_version(v.id, "published", None).await.unwrap();
        st.promoted(lib, web, v.id, "active").await.unwrap();
        let os = "/pulp/api/v3/distributions/rpm/rpm/7/";
        let row = crate::store::BindingRepo {
            env_id: dev,
            cv_id: web,
            repo_href: format!("{REPOS}rpm/rpm/1/"),
            repo_name: "os".into(),
            repo_type: RepoType::Rpm,
            distribution: os.into(),
            base_path: "dev/web/os".into(),
            publication: Some("/p/1/".into()),
        };
        st.save_binding_repo(&row).await.unwrap();
        let none = json!({ "count": 0, "next": null, "results": [] });
        let docs = "  docs (File): new distribution pulp-tui-cv-web-Dev-docs at /dev/web/docs; the promotion fails for it: base path \
                    dev/web/docs is used by distribution foreign";
        let at_derived = "  os (RPM): distribution os-moved at /dev/web/os was renamed outside pulp-tui; the promotion looks for \
                          pulp-tui-cv-web-Dev-os and fails: rename it back";
        let elsewhere = "  os (RPM): new distribution pulp-tui-cv-web-Dev-os at /dev/web/os; os-moved at /custom/os (renamed outside \
                         pulp-tui) keeps its publication, and pulp-tui no longer records it: rename it back first to re-point it";
        for (at, want) in [("dev/web/os", at_derived), ("custom/os", elsewhere)] {
            server.reset().await;
            for t in ["rpm/rpm", "file/file"] {
                serve(&server, "GET", format!("/pulp/api/v3/distributions/{t}/"), 200, none.clone()).await;
            }
            let foreign =
                json!({ "count": 1, "next": null, "results": [{ "pulp_href": "/d/9/", "name": "foreign", "base_path": "dev/web/docs" }] });
            Mock::given(method("GET"))
                .and(path("/pulp/api/v3/distributions/"))
                .and(wiremock::matchers::query_param("base_path__in", "dev,dev/web,dev/web/docs"))
                .respond_with(ResponseTemplate::new(200).set_body_json(foreign))
                .with_priority(1)
                .mount(&server)
                .await;
            serve(&server, "GET", "/pulp/api/v3/distributions/".into(), 200, none.clone()).await;
            let labels = json!({ crate::cv::LABEL_CV: "web", crate::cv::LABEL_ENV: "Dev" });
            let renamed = json!({ "pulp_href": os, "name": "os-moved", "base_path": at, "pulp_labels": labels });
            serve(&server, "GET", os.into(), 200, renamed).await;
            promote_preview(&mut app, "web".into(), v.id, "Dev".into(), Mode::Promote);
            drain(&mut app, &mut rx).await;
            assert_eq!(confirm_lines(&mut app)[2..4], [docs, want], "renamed one at {at}");
        }
    }

    /// Rows of repositories that the version lacks and that record no publication: the promotion unserves the one whose
    /// distribution serves a publication (set outside pulp-tui), and forgets the one whose distribution is gone or lost
    /// the labels of the view; the preview names these, and no other one.
    #[tokio::test]
    async fn the_promote_preview_names_a_left_out_distribution_that_serves_again() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let lib = st.ensure_library("lab").await.unwrap();
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let v = publish_empty(&st, web).await;
        for (n, id) in [("old", "1"), ("idle", "2"), ("gone", "3"), ("mine", "4")] {
            let row = crate::store::BindingRepo {
                env_id: lib,
                cv_id: web,
                repo_href: format!("{REPOS}rpm/rpm/{id}/"),
                repo_name: n.into(),
                repo_type: RepoType::Rpm,
                distribution: format!("/pulp/api/v3/distributions/rpm/rpm/{id}/"),
                base_path: format!("library/web/{n}"),
                publication: None,
            };
            st.save_binding_repo(&row).await.unwrap();
        }
        let ours = json!({ crate::cv::LABEL_CV: "web", crate::cv::LABEL_ENV: LIBRARY });
        let serves = |labels: Value| json!({ "name": "d", "publication": "/p/3/", "pulp_labels": labels });
        serve(&server, "GET", "/pulp/api/v3/distributions/rpm/rpm/1/".into(), 200, serves(ours)).await;
        serve(&server, "GET", "/pulp/api/v3/distributions/rpm/rpm/2/".into(), 200, json!({ "publication": null })).await;
        serve(&server, "GET", "/pulp/api/v3/distributions/rpm/rpm/4/".into(), 200, serves(json!({ "team": "x" }))).await;
        promote_preview(&mut app, "web".into(), v.id, LIBRARY.into(), Mode::Rollback);
        drain(&mut app, &mut rx).await;
        let gone = "  gone: not part of v1 -> its distribution is gone (deleted outside pulp-tui): pulp-tui forgets it";
        let mine = "  mine: not part of v1 -> its distribution d is not labeled pulp_tui_cv=web (changed outside pulp-tui): \
                    pulp-tui leaves it as it is and forgets it";
        let old = "  old: not part of v1 -> its distribution stops serving (kept for rollback); Pulp shows that it serves \
                   content set outside pulp-tui";
        assert_eq!(confirm_lines(&mut app)[2..6], [gone, mine, old, ""]);
    }

    /// F59: the distribution of a left-out row that records a publication answers 404, and the user is not a Pulp
    /// administrator (403 on `access_policies/`): it can be hidden by RBAC. The preview says that the step fails for the
    /// row, why, and what to do.
    #[tokio::test]
    async fn the_promote_preview_says_that_the_step_fails_for_a_distribution_the_user_may_not_view() {
        let server = MockServer::start().await; // 404 for the rest: the distribution of r5
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let lib = st.ensure_library("lab").await.unwrap();
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let v = publish_empty(&st, web).await;
        left_out(&st, lib, web, "5", Some("/p/5/")).await;
        serve(&server, "GET", "/pulp/api/v3/access_policies/".into(), 403, json!({ "detail": "no" })).await;
        promote_preview(&mut app, "web".into(), v.id, LIBRARY.into(), Mode::Rollback);
        drain(&mut app, &mut rx).await;
        let refused = "  r5: not part of v1 -> the step fails for it: Pulp answers 404 for its distribution \
                       /pulp/api/v3/distributions/rpm/rpm/5/: it was deleted, or this Pulp user may not view it. pulp-tui \
                       keeps the row, so the version that it serves stays protected: run the step again as a Pulp \
                       administrator, or give this user the view permission on the distribution";
        assert_eq!(confirm_lines(&mut app)[2..4], [refused, ""]);
    }

    /// A confirmed promotion says what it verified: the served metadata, not every file.
    #[tokio::test]
    async fn a_confirmed_promotion_says_it_verified_the_served_metadata() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        app.push(Kind::ContentViews);
        drain(&mut app, &mut rx).await;
        let lib = st.ensure_library("lab").await.unwrap();
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let v = publish_empty(&st, web).await; // no repositories: nothing to send to Pulp
        promote_preview(&mut app, "web".into(), v.id, LIBRARY.into(), Mode::Rollback);
        drain(&mut app, &mut rx).await;
        let Some(Modal::Confirm { on_yes, .. }) = app.modal.take() else { panic!("no confirmation: {:?}", last_notice(&app)) };
        on_yes.unwrap()(&mut app);
        drain(&mut app, &mut rx).await;
        let notices: Vec<&str> = app.notices.iter().map(|n| n.text.as_str()).collect();
        assert!(notices.contains(&"web v1 active in Library; served metadata verified"), "{notices:?}");
        assert_eq!(st.binding(lib, web).await.unwrap().unwrap().active(), Some(v.id));
    }

    /// A left-out row of `web` in Library: repository `n`, at distribution `rpm/rpm/n`.
    async fn left_out(st: &Store, lib: i64, web: i64, n: &str, publication: Option<&str>) -> crate::store::BindingRepo {
        let row = crate::store::BindingRepo {
            env_id: lib,
            cv_id: web,
            repo_href: format!("{REPOS}rpm/rpm/{n}/"),
            repo_name: format!("r{n}"),
            repo_type: RepoType::Rpm,
            distribution: format!("/pulp/api/v3/distributions/rpm/rpm/{n}/"),
            base_path: format!("library/web/r{n}"),
            publication: publication.map(str::to_string),
        };
        st.save_binding_repo(&row).await.unwrap();
        row
    }

    /// A promotion to a protected environment asks for the typed environment; what Verify finds then is a warning.
    #[tokio::test]
    async fn a_promotion_to_a_protected_environment_is_typed_and_warns_of_what_verify_finds() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let lib = st.ensure_library("lab").await.unwrap();
        st.set_env_protected(lib, true).await.unwrap();
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let v = publish_empty(&st, web).await;
        // The unbind of a left-out row completes, but Pulp still shows the publication: Verify names it.
        let old = left_out(&st, lib, web, "8", Some("/p/3/")).await;
        let labels = json!({ crate::cv::LABEL_CV: "web", crate::cv::LABEL_ENV: LIBRARY, crate::cv::LABEL_DB: st.db_id() });
        let d = json!({ "pulp_href": old.distribution, "name": "d", "base_path": old.base_path, "publication": "/p/3/", "pulp_labels": labels });
        serve(&server, "GET", old.distribution.clone(), 200, d).await;
        serve(&server, "PATCH", old.distribution.clone(), 202, json!({ "task": format!("{TASKS}1/") })).await;
        serve(&server, "GET", format!("{TASKS}1/"), 200, json!({ "pulp_href": format!("{TASKS}1/"), "state": "completed" })).await;
        promote_preview(&mut app, "web".into(), v.id, LIBRARY.into(), Mode::Rollback);
        drain(&mut app, &mut rx).await;
        let Some(Modal::Confirm { title, typed, on_yes: Some(yes), .. }) = app.modal.take() else {
            panic!("no confirm: {:?}", last_notice(&app))
        };
        assert_eq!((title.as_str(), typed.as_deref()), ("Library is protected", Some(LIBRARY)));
        yes(&mut app);
        drain(&mut app, &mut rx).await;
        let warned = app.notices.iter().any(|n| {
            n.level == Level::Warn
                && n.text.starts_with("web in Library: bound, verification found: r8: distribution serves /p/3/, expected nothing")
        });
        assert!(warned, "{:?}", app.notices.iter().map(|n| &n.text).collect::<Vec<_>>());
        assert_eq!(st.binding(lib, web).await.unwrap().unwrap().active(), Some(v.id));
    }

    /// Promote and Rollback differ in the preview (a rollback says what it keeps) and in the job that runs.
    #[tokio::test]
    async fn promote_and_rollback_show_their_own_notes_and_job() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let lib = st.ensure_library("lab").await.unwrap();
        st.create_env("lab", "Dev", lib, false).await.unwrap();
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let v = publish_empty(&st, web).await;
        st.promoted(lib, web, v.id, "active").await.unwrap();
        let notes = [
            "",
            "Rollback only changes bindings; newer versions stay available.",
            "Files of the replaced publications may stay downloadable for Pulp's",
            "DISTRIBUTED_PUBLICATION_RETENTION_PERIOD; repository metadata switches immediately.",
        ];
        for (mode, env, job, tail) in
            [(Mode::Promote, "Dev", "promote web v1 -> Dev", &[][..]), (Mode::Rollback, LIBRARY, "rollback web v1 -> Library", &notes[..])]
        {
            promote_preview(&mut app, "web".into(), v.id, env.into(), mode);
            drain(&mut app, &mut rx).await;
            let Some(Modal::Confirm { lines, on_yes: Some(yes), .. }) = app.modal.take() else {
                panic!("no confirm: {:?}", last_notice(&app))
            };
            assert_eq!(lines[2..].iter().map(String::as_str).collect::<Vec<_>>(), tail, "{env}");
            yes(&mut app);
            assert_eq!(app.jobs.last().map(|j| j.label.as_str()), Some(job));
            drain(&mut app, &mut rx).await;
            let ok = format!("web v1 active in {env}; served metadata verified");
            assert!(app.notices.iter().any(|n| n.level == Level::Ok && n.text == ok), "{env}");
        }
    }

    /// F56: Library serves the r9 publication that its row records, from a distribution that an operator took over (the
    /// labels of the view removed). The preview says that pulp-tui leaves it as it is and forgets the row, and the run
    /// does that: no PATCH, the row is gone, the promotion completes. (A version delete still keeps that publication:
    /// Pulp shows that the distribution serves it, `CvService::served_in_pulp`.)
    #[tokio::test]
    async fn a_taken_over_left_out_distribution_with_a_recorded_publication_is_forgotten_as_the_preview_says() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let lib = st.ensure_library("lab").await.unwrap();
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let v = publish_empty(&st, web).await;
        let p = "/pulp/api/v3/publications/rpm/rpm/3/";
        let old = left_out(&st, lib, web, "9", Some(p)).await;
        let theirs =
            json!({ "pulp_href": old.distribution, "name": "theirs", "base_path": old.base_path, "publication": p, "pulp_labels": {} });
        serve(&server, "GET", old.distribution.clone(), 200, theirs).await;
        promote_preview(&mut app, "web".into(), v.id, LIBRARY.into(), Mode::Rollback);
        drain(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm: {:?}", last_notice(&app)) };
        let forget = "  r9: not part of v1 -> its distribution theirs is not labeled pulp_tui_cv=web (changed outside pulp-tui): \
                      pulp-tui leaves it as it is and forgets it";
        assert_eq!(lines[2], forget);
        yes(&mut app);
        drain(&mut app, &mut rx).await;
        let notices: Vec<&str> = app.notices.iter().map(|n| n.text.as_str()).collect();
        assert!(notices.contains(&"web v1 active in Library; served metadata verified"), "{notices:?}");
        assert!(st.binding_repos(lib, web).await.unwrap().is_empty(), "the row is forgotten");
        assert!(server.received_requests().await.unwrap().iter().all(|r| r.method.as_str() == "GET"), "nothing is changed");
    }

    /// A promotion that fails (Pulp refuses the unbind of a left-out distribution) says that its partial states are
    /// recorded.
    #[tokio::test]
    async fn a_failed_promotion_says_that_its_partial_states_are_recorded() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let lib = st.ensure_library("lab").await.unwrap();
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let v = publish_empty(&st, web).await;
        let old = left_out(&st, lib, web, "9", Some("/pulp/api/v3/publications/rpm/rpm/3/")).await;
        let labels = json!({ crate::cv::LABEL_CV: "web", crate::cv::LABEL_ENV: LIBRARY, crate::cv::LABEL_DB: st.db_id() });
        let ours = json!({ "pulp_href": old.distribution, "name": "d", "base_path": old.base_path, "pulp_labels": labels });
        serve(&server, "GET", old.distribution.clone(), 200, ours).await;
        serve(&server, "PATCH", old.distribution.clone(), 403, json!({ "detail": "no" })).await;
        promote_preview(&mut app, "web".into(), v.id, LIBRARY.into(), Mode::Rollback);
        drain(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm: {:?}", last_notice(&app)) };
        assert_eq!(lines[2], "  r9: not part of v1 -> its distribution stops serving (kept for rollback)");
        yes(&mut app);
        drain(&mut app, &mut rx).await;
        let failed = app.notices.iter().any(|n| {
            n.level == Level::Error
                && n.text.starts_with("web in Library: failed: unbind r9: ")
                && n.text.ends_with(" (partial states are recorded; promote again to retry or roll back)")
        });
        assert!(failed, "{:?}", app.notices.iter().map(|n| &n.text).collect::<Vec<_>>());
    }

    /// A promotion of a File repository whose publication has no manifest completes OK, as Verify says, and the notice
    /// says that no manifest was compared.
    #[tokio::test]
    async fn a_promotion_without_a_file_manifest_is_ok_and_says_no_manifest_compared() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        app.push(Kind::ContentViews);
        drain(&mut app, &mut rx).await;
        let lib = st.ensure_library("lab").await.unwrap();
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let v = st.begin_version(web, "", &[cv_repo("docs", "file", "1")]).await.unwrap();
        let (dv, publication, dist) = (
            format!("{REPOS}file/file/2/versions/1/"),
            "/pulp/api/v3/publications/file/file/1/",
            "/pulp/api/v3/distributions/file/file/3/",
        );
        let row = crate::store::CvVersionRepo {
            derived_version: Some(dv.clone()),
            publication: Some(publication.into()),
            ..st.version_repos(v.id).await.unwrap().remove(0)
        };
        st.save_version_repo(&row).await.unwrap();
        st.finish_version(v.id, "published", None).await.unwrap();
        // The distribution of the derived name already serves the publication: the promotion sends nothing.
        let labels = json!({ crate::cv::LABEL_CV: "web", crate::cv::LABEL_ENV: LIBRARY, crate::cv::LABEL_DB: st.db_id() });
        let base = format!("{}/pulp/content/library/web/docs/", server.uri());
        let d = json!({ "pulp_href": dist, "name": crate::cv::distribution_name("web", LIBRARY, "docs"), "base_path": "library/web/docs",
            "base_url": base, "publication": publication, "pulp_labels": labels });
        serve(&server, "GET", "/pulp/api/v3/distributions/file/file/".into(), 200, json!({ "count": 1, "next": null, "results": [d] }))
            .await;
        serve(&server, "GET", dist.into(), 200, d).await;
        let p = json!({ "pulp_href": publication, "repository_version": dv, "manifest": null });
        serve(&server, "GET", publication.into(), 200, p).await;
        serve(&server, "GET", dv.clone(), 200, json!({ "pulp_href": dv })).await;
        promote_preview(&mut app, "web".into(), v.id, LIBRARY.into(), Mode::Rollback);
        drain(&mut app, &mut rx).await;
        let Some(Modal::Confirm { on_yes, .. }) = app.modal.take() else { panic!("no confirmation: {:?}", last_notice(&app)) };
        on_yes.unwrap()(&mut app);
        drain(&mut app, &mut rx).await;
        let ok = "web v1 active in Library; served metadata verified; File: no manifest compared (docs: the publication has no \
                  manifest: served files not compared)";
        let notices: Vec<(Level, &str)> = app.notices.iter().map(|n| (n.level, n.text.as_str())).collect();
        assert!(notices.contains(&(Level::Ok, ok)), "{notices:?}");
        assert_eq!(st.binding(lib, web).await.unwrap().unwrap().active(), Some(v.id));
    }

    /// Drift that a promotion cannot fix gets a hint that names what fixes it, and no "promote again".
    #[tokio::test]
    async fn drift_hints_name_only_actions_that_fix_that_drift() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let lib = st.ensure_library("lab").await.unwrap();
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let v = st.begin_version(web, "", &[cv_repo("os", "rpm", "1")]).await.unwrap();
        let (derived, dv, publication, dist) = (
            format!("{REPOS}rpm/rpm/2/"),
            format!("{REPOS}rpm/rpm/2/versions/1/"),
            "/pulp/api/v3/publications/rpm/rpm/1/",
            "/pulp/api/v3/distributions/rpm/rpm/3/",
        );
        let row = crate::store::CvVersionRepo {
            derived_repo: Some(derived.clone()),
            derived_version: Some(dv.clone()),
            publication: Some(publication.into()),
            ..st.version_repos(v.id).await.unwrap().remove(0)
        };
        st.save_version_repo(&row).await.unwrap();
        st.finish_version(v.id, "published", None).await.unwrap();
        st.promoted(lib, web, v.id, "active").await.unwrap();
        let b = crate::store::BindingRepo {
            env_id: lib,
            cv_id: web,
            repo_href: format!("{REPOS}rpm/rpm/1/"),
            repo_name: "os".into(),
            repo_type: RepoType::Rpm,
            distribution: dist.into(),
            base_path: "library/web/os".into(),
            publication: Some(publication.into()),
        };
        st.save_binding_repo(&b).await.unwrap();
        // It serves the right publication, but the server has no content origin and the snapshot has retention.
        serve(
            &server,
            "GET",
            dist.into(),
            200,
            json!({ "pulp_href": dist, "name": "d", "base_path": "library/web/os", "base_url": null, "publication": publication }),
        )
        .await;
        serve(&server, "GET", publication.into(), 200, json!({ "pulp_href": publication, "repository_version": dv })).await;
        serve(&server, "GET", dv.clone(), 200, json!({ "pulp_href": dv })).await;
        serve(&server, "GET", derived.clone(), 200, json!({ "pulp_href": derived, "name": "web-os", "retain_repo_versions": 1 })).await;
        cv_action(&mut app, "lab", Act::CvDrift, json!({ "id": web, "name": "web" }));
        drain(&mut app, &mut rx).await;
        let Some(Modal::Text { lines, .. }) = app.modal.take() else { panic!("no verification") };
        assert_eq!(lines[0], "Library: v1 DRIFT");
        let hints: Vec<&str> = lines.iter().skip_while(|l| !l.is_empty()).skip(1).map(String::as_str).collect();
        assert_eq!(
            hints,
            [
                "Library: set CONTENT_ORIGIN on the Pulp server: a distribution without base_url serves nothing.",
                "Library: clear retain_repo_versions of the snapshot repository with the Pulp API or CLI (pulp-tui does not edit \
                 snapshot repositories): Pulp would prune its versions."
            ]
        );
        // While a workflow holds a live lock on the view, only that workflow is named.
        st.try_lock(web, "bob@ws2", "tok", "publish", crate::cv::unix_now()).await.unwrap().unwrap();
        cv_action(&mut app, "lab", Act::CvDrift, json!({ "id": web, "name": "web" }));
        drain(&mut app, &mut rx).await;
        let Some(Modal::Text { lines, .. }) = app.modal.take() else { panic!("no verification") };
        let hints: Vec<&str> = lines.iter().skip_while(|l| !l.is_empty()).skip(1).map(String::as_str).collect();
        let running = "Library: publish by bob@ws2 is running: verify again when it ends (its lock refuses Publish, Promote and \
                       Rollback until then).";
        assert_eq!(hints, [running]);
    }

    #[tokio::test]
    async fn a_blocked_promotion_shows_why_and_offers_no_confirmation() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let lib = st.ensure_library("lab").await.unwrap();
        st.create_env("lab", "Dev", lib, false).await.unwrap();
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let v = publish_empty(&st, web).await; // not active in Library
        promote_preview(&mut app, "web".into(), v.id, "Dev".into(), Mode::Promote);
        drain(&mut app, &mut rx).await;
        assert!(app.modal.is_none(), "no preview to confirm");
        assert_eq!(last_notice(&app), (Level::Error, "version 1 is not active in Library; promote it there first (or use rollback)"));
        assert!(server.received_requests().await.unwrap().is_empty(), "nothing read from Pulp for a refused promotion");
    }

    fn confirm_lines(app: &mut App) -> Vec<String> {
        match app.modal.take() {
            Some(Modal::Confirm { lines, .. }) => lines,
            _ => panic!("no confirmation"),
        }
    }

    #[tokio::test]
    async fn preview_and_drift_show_an_interrupted_promotion() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let dev = st.create_env("lab", "Dev", st.ensure_library("lab").await.unwrap(), false).await.unwrap();
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let v = publish_empty(&st, web).await;
        st.promoted(dev, web, v.id, "promoting").await.unwrap(); // no live lock: the client that ran it ended
        promote_preview(&mut app, "web".into(), v.id, "Dev".into(), Mode::Rollback);
        drain(&mut app, &mut rx).await;
        assert_eq!(confirm_lines(&mut app)[0], "web: Dev now serves nothing (v1 interrupted); will serve v1");
        cv_action(&mut app, "lab", Act::CvDrift, json!({ "id": web, "name": "web" }));
        drain(&mut app, &mut rx).await;
        let Some(Modal::Text { lines, .. }) = app.modal.take() else { panic!("no verification") };
        assert_eq!(lines[..2], ["Dev: nothing (v1 interrupted) DRIFT", "    promotion of v1 is interrupted"]);
    }

    /// While a promotion runs (a live lock), the drift check says so instead of naming Promote or Rollback: the lock
    /// refuses them.
    #[tokio::test]
    async fn drift_names_a_running_promotion_instead_of_promote_or_rollback() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let dev = st.create_env("lab", "Dev", st.ensure_library("lab").await.unwrap(), false).await.unwrap();
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let v = publish_empty(&st, web).await;
        st.promoted(dev, web, v.id, "promoting").await.unwrap();
        st.try_lock(web, "bob@ws2", "tok", "promote to Dev", crate::cv::unix_now()).await.unwrap().unwrap();
        cv_action(&mut app, "lab", Act::CvDrift, json!({ "id": web, "name": "web" }));
        drain(&mut app, &mut rx).await;
        let Some(Modal::Text { lines, .. }) = app.modal.take() else { panic!("no verification") };
        let hint = "Dev: promote to Dev by bob@ws2 is running: verify again when it ends (its lock refuses Publish, Promote and \
                    Rollback until then).";
        assert_eq!(lines, ["Dev: nothing (v1 promoting) DRIFT", "    promotion of v1 is promoting", "", hint]);
    }

    /// A File publication without a manifest: the drift check compares no served file and says so, not a plain OK.
    #[tokio::test]
    async fn drift_says_when_a_file_publication_has_no_manifest() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let lib = st.ensure_library("lab").await.unwrap();
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let v = st.begin_version(web, "", &[cv_repo("docs", "file", "1")]).await.unwrap();
        let (dv, publication, dist) = (
            format!("{REPOS}file/file/2/versions/1/"),
            "/pulp/api/v3/publications/file/file/1/",
            "/pulp/api/v3/distributions/file/file/3/",
        );
        let row = crate::store::CvVersionRepo {
            derived_version: Some(dv.clone()),
            publication: Some(publication.into()),
            ..st.version_repos(v.id).await.unwrap().remove(0)
        };
        st.save_version_repo(&row).await.unwrap();
        st.finish_version(v.id, "published", None).await.unwrap();
        st.promoted(lib, web, v.id, "active").await.unwrap();
        let b = crate::store::BindingRepo {
            env_id: lib,
            cv_id: web,
            repo_href: format!("{REPOS}file/file/1/"),
            repo_name: "docs".into(),
            repo_type: RepoType::File,
            distribution: dist.into(),
            base_path: "library/web/docs".into(),
            publication: Some(publication.into()),
        };
        st.save_binding_repo(&b).await.unwrap();
        let base = format!("{}/pulp/content/library/web/docs/", server.uri());
        let d = json!({ "pulp_href": dist, "name": "d", "base_path": "library/web/docs", "base_url": base, "publication": publication });
        serve(&server, "GET", dist.into(), 200, d).await;
        let p = json!({ "pulp_href": publication, "repository_version": dv, "manifest": null });
        serve(&server, "GET", publication.into(), 200, p).await;
        serve(&server, "GET", dv.clone(), 200, json!({ "pulp_href": dv })).await;
        cv_action(&mut app, "lab", Act::CvDrift, json!({ "id": web, "name": "web" }));
        drain(&mut app, &mut rx).await;
        let Some(Modal::Text { lines, .. }) = app.modal.take() else { panic!("no verification") };
        assert_eq!(
            lines,
            [
                "Library: v1 OK - served metadata match; RPM/Deb: also the smallest package; File: no manifest compared for the \
                 repositories below",
                "    docs: the publication has no manifest: served files not compared"
            ]
        );
    }

    #[tokio::test]
    async fn drift_names_rollback_when_the_prior_environment_moved_on() {
        let (mut app, mut rx, st) = composite_app().await;
        app.conn =
            Some(crate::tui::app::Conn { client: client(&MockServer::start().await), caps: Default::default(), scope: "lab".into() });
        let lib = st.ensure_library("lab").await.unwrap();
        let dev = st.create_env("lab", "Dev", lib, false).await.unwrap();
        let test = st.create_env("lab", "Test", dev, false).await.unwrap();
        let prod = st.create_env("lab", "Prod", test, false).await.unwrap();
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let (v1, v2) = (publish_empty(&st, web).await, publish_empty(&st, web).await);
        st.promoted(lib, web, v2.id, "active").await.unwrap();
        st.promoted(dev, web, v2.id, "partial").await.unwrap(); // Library has v2 active: Promote works
        st.promoted(test, web, v2.id, "partial").await.unwrap(); // Dev has v2, but not active: refused
        st.promoted(prod, web, v1.id, "partial").await.unwrap(); // Test moved on to v2: Promote of v1 is refused
        cv_action(&mut app, "lab", Act::CvDrift, json!({ "id": web, "name": "web" }));
        drain(&mut app, &mut rx).await;
        let Some(Modal::Text { lines, .. }) = app.modal.take() else { panic!("no verification") };
        // Verify reads the served metadata and one sample package, not every file: the line says so.
        assert_eq!(lines[0], "Library: v2 OK - served metadata match; RPM/Deb: also the smallest package");
        assert_eq!(
            lines[lines.len() - 4..],
            [
                "",
                "Dev: promote v2 again (M) to reconcile (idempotent).",
                "Test: roll back to v2 (b) to reconcile; Promote refuses: version 2 is not active in Dev; promote it there first (or use rollback)",
                "Prod: roll back to v1 (b) to reconcile; Promote refuses: version 1 is not active in Test; promote it there first (or use rollback)"
            ]
        );
    }

    #[tokio::test]
    async fn rollback_to_a_newer_version_says_use_promote_instead_of_a_preview() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let dev = st.create_env("lab", "Dev", st.ensure_library("lab").await.unwrap(), false).await.unwrap();
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let (v1, v2) = (publish_empty(&st, web).await, publish_empty(&st, web).await);
        promote_preview(&mut app, "web".into(), v2.id, "Dev".into(), Mode::Rollback);
        drain(&mut app, &mut rx).await;
        assert!(app.modal.is_none(), "no preview to confirm");
        assert_eq!(last_notice(&app), (Level::Error, "no version is active in Dev; use Promote"));
        st.promoted(dev, web, v1.id, "active").await.unwrap();
        promote_preview(&mut app, "web".into(), v2.id, "Dev".into(), Mode::Rollback);
        drain(&mut app, &mut rx).await;
        assert!(app.modal.is_none(), "no preview to confirm");
        assert_eq!(last_notice(&app), (Level::Error, "v2 is newer than the active v1 in Dev; use Promote"));
        promote_preview(&mut app, "web".into(), v1.id, "Dev".into(), Mode::Rollback);
        drain(&mut app, &mut rx).await;
        assert_eq!(confirm_lines(&mut app)[0], "web: Dev now serves v1; will serve v1", "the active version again: a retry");
    }

    #[tokio::test]
    async fn unpromote_shows_the_paths_it_deletes_then_deletes_them() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let lib = st.ensure_library("lab").await.unwrap();
        let prod = st.create_env("lab", "Prod", lib, true).await.unwrap();
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let v = publish_empty(&st, web).await;
        st.promoted(prod, web, v.id, "active").await.unwrap();
        let row = crate::store::BindingRepo {
            env_id: prod,
            cv_id: web,
            repo_href: format!("{REPOS}rpm/rpm/1/"),
            repo_name: "os".into(),
            repo_type: RepoType::Rpm,
            distribution: "/pulp/api/v3/distributions/rpm/rpm/1/".into(),
            base_path: "prod/web/os".into(),
            publication: Some("/p/1/".into()),
        };
        st.save_binding_repo(&row).await.unwrap();
        cv_action(&mut app, "lab", Act::Unpromote, json!({ "id": web, "name": "web" }));
        drain(&mut app, &mut rx).await;
        let Some(Modal::Form { form, mut submit }) = app.modal.take() else { panic!("no environment form") };
        assert!(matches!(&form.fields[0].kind, FieldKind::Choice(c) if c == &["Prod"]), "only where the view is");
        submit(&mut app, &form).unwrap();
        drain(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, typed, on_yes, .. }) = app.modal.take() else { panic!("no confirmation") };
        assert!(lines.contains(&"  os: /prod/web/os".to_string()), "{lines:?}");
        assert_eq!(typed.as_deref(), Some("web Prod"));
        Mock::given(method("DELETE"))
            .and(path("/pulp/api/v3/distributions/rpm/rpm/1/"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        on_yes.unwrap()(&mut app);
        drain(&mut app, &mut rx).await;
        let notices: Vec<&str> = app.notices.iter().map(|n| n.text.as_str()).collect();
        assert!(notices.contains(&"web is not in Prod any more"), "{notices:?}");
        assert!(st.binding(prod, web).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn deleting_a_composite_does_not_promise_snapshot_removal() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let c = st.create_composite("lab", "rocky9", "").await.unwrap();
        cv_action(&mut app, "lab", Act::Delete, json!({ "id": c, "name": "rocky9", "composite": true }));
        drain(&mut app, &mut rx).await;
        assert_eq!(
            confirm_lines(&mut app),
            [
                "Delete composite content view rocky9: its distributions in every environment (consumers lose these URLs).",
                "Component views and their snapshots are NOT touched.",
                "",
                "Nothing is served now."
            ]
        );
        // The impact lists what is served; a served protected environment must be typed too.
        let lib = st.ensure_library("lab").await.unwrap();
        let prod = st.create_env("lab", "Prod", lib, true).await.unwrap();
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let row = crate::store::BindingRepo {
            env_id: prod,
            cv_id: web,
            repo_href: format!("{REPOS}rpm/rpm/1/"),
            repo_name: "os".into(),
            repo_type: RepoType::Rpm,
            distribution: "/d/1/".into(),
            base_path: "prod/web/os".into(),
            publication: Some("/p/1/".into()),
        };
        st.save_binding_repo(&row).await.unwrap();
        cv_action(&mut app, "lab", Act::Delete, json!({ "id": web, "name": "web", "composite": false }));
        drain(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, typed, .. }) = app.modal.take() else { panic!("no confirmation") };
        assert!(lines.iter().any(|l| l.contains("its snapshot repositories (with their versions and publications)")));
        assert_eq!(lines.last().unwrap(), &format!("  {:<12} /prod/web/os  (protected)", "Prod"));
        assert_eq!(typed.as_deref(), Some("web Prod"));
    }

    #[tokio::test]
    async fn delete_view_is_refused_before_the_confirmation_while_a_distribution_serves_its_snapshot() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let derived = format!("{REPOS}rpm/rpm/2/");
        let v = st.begin_version(web, "", &[cv_repo("os", "rpm", "1")]).await.unwrap();
        let mut vr = st.version_repos(v.id).await.unwrap().remove(0);
        vr.derived_repo = Some(derived.clone());
        st.save_version_repo(&vr).await.unwrap();
        serve(&server, "GET", "/pulp/api/v3/publications/rpm/rpm/".into(), 200, json!({ "count": 0, "next": null, "results": [] })).await;
        let d = json!({ "pulp_href": "/d/7/", "name": "partial", "base_path": "partial", "repository": derived });
        serve(&server, "GET", "/pulp/api/v3/distributions/rpm/rpm/".into(), 200, json!({ "count": 1, "next": null, "results": [d] })).await;
        cv_action(&mut app, "lab", Act::Delete, json!({ "id": web, "name": "web", "composite": false }));
        drain(&mut app, &mut rx).await;
        assert!(app.modal.is_none(), "refused: no confirmation");
        let (level, text) = last_notice(&app);
        assert_eq!(level, Level::Error);
        assert!(text.starts_with("read what deleting web removes: content view web: distribution partial serves "), "{text}");
    }

    #[tokio::test]
    async fn delete_version_shows_what_it_deletes_or_why_it_is_refused() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let web = st.create_cv("lab", "web", "").await.unwrap();
        let derived = format!("{REPOS}rpm/rpm/2/");
        let pubs = "/pulp/api/v3/publications/rpm/rpm/";
        // v1 and v2 of web: one snapshot version (the selection did not change), a publication each.
        let mut versions = vec![];
        for n in 1..=2 {
            let v = st.begin_version(web, "", &[cv_repo("os", "rpm", "1")]).await.unwrap();
            let mut vr = st.version_repos(v.id).await.unwrap().remove(0);
            (vr.derived_repo, vr.derived_version) = (Some(derived.clone()), Some(format!("{derived}versions/1/")));
            vr.publication = Some(format!("{pubs}{n}/"));
            st.save_version_repo(&vr).await.unwrap();
            st.finish_version(v.id, "published", None).await.unwrap();
            versions.push((v, vr));
        }
        let (v1, row) = versions.remove(0);
        let labels = json!({ "pulp_labels": { crate::cv::LABEL_CV: "web", crate::cv::LABEL_DB: st.db_id() } });
        serve(&server, "GET", derived.clone(), 200, labels).await;
        // An operator's distribution serves the publication of v2 only: the publication of v1 can go.
        let dv = format!("{derived}versions/1/");
        let pubs_of = |n: u64| json!({ "pulp_href": format!("{pubs}{n}/"), "repository_version": dv });
        serve(&server, "GET", pubs.into(), 200, json!({ "count": 2, "next": null, "results": [pubs_of(1), pubs_of(2)] })).await;
        let d = json!({ "pulp_href": "/d/7/", "name": "partial", "base_path": "partial", "publication": format!("{pubs}2/") });
        serve(&server, "GET", "/pulp/api/v3/distributions/rpm/rpm/".into(), 200, json!({ "count": 1, "next": null, "results": [d] })).await;
        let item = json!({ "id": v1.id, "number": v1.number, "state": "published", "components": [] });
        let lib = st.ensure_library("lab").await.unwrap();
        st.promoted(lib, web, v1.id, "active").await.unwrap();
        version_action(&mut app, "lab", web, "web".into(), Act::Delete, item.clone());
        drain(&mut app, &mut rx).await;
        assert!(app.modal.is_none(), "refused: no confirmation");
        assert_eq!(last_notice(&app), (Level::Error, "read what deleting web v1 removes: version 1 is bound to Library"));

        st.delete_binding(lib, web).await.unwrap();
        version_action(&mut app, "lab", web, "web".into(), Act::Delete, item);
        drain(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, typed, on_yes, .. }) = app.modal.take() else { panic!("no confirmation") };
        assert_eq!(
            lines,
            [
                "Delete web version 1. In Pulp:".to_string(),
                format!("  os: delete {pubs}1/"),
                format!("  os: keep {derived}versions/1/ (shared with another version)"),
                "The version number is never reused.".into(),
            ]
        );
        assert_eq!(typed.as_deref(), Some("v1"));
        let sent = server.received_requests().await.unwrap();
        assert!(sent.iter().all(|r| r.method.as_str() == "GET"), "nothing changed before the confirmation: {sent:?}");
        Mock::given(method("DELETE"))
            .and(path(row.publication.unwrap()))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        on_yes.unwrap()(&mut app);
        drain(&mut app, &mut rx).await;
        assert!(app.notices.iter().any(|n| n.text == "version deleted"));
        assert_eq!(st.version(v1.id).await.unwrap().state, "deleted");

        // A composite version borrows the snapshots of its components: nothing in Pulp is deleted.
        let (v2, row2) = versions.remove(0);
        let c = st.create_composite("lab", "rocky9", "").await.unwrap();
        let cv1 = st.begin_composite_version(c, "", &[v2.id], &[row2]).await.unwrap();
        let item = json!({ "id": cv1.id, "number": cv1.number, "state": "publishing", "components": ["web v2"] });
        version_action(&mut app, "lab", c, "rocky9".into(), Act::Delete, item);
        drain(&mut app, &mut rx).await;
        assert_eq!(
            confirm_lines(&mut app),
            [
                "Delete rocky9 version 1: only this composite version is removed.",
                "The snapshots and publications of its component versions are NOT touched.",
                "The version number is never reused.",
            ]
        );
    }

    #[tokio::test]
    async fn delete_version_says_when_nothing_in_pulp_is_deleted() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let web = st.create_cv("lab", "web", "").await.unwrap();
        // The publish failed before it made the derived repository: the row has no Pulp objects.
        let v = st.begin_version(web, "", &[cv_repo("os", "rpm", "1")]).await.unwrap();
        st.finish_version(v.id, "failed", Some("stopped")).await.unwrap();
        let item = json!({ "id": v.id, "number": v.number, "state": "failed", "components": [] });
        version_action(&mut app, "lab", web, "web".into(), Act::Delete, item);
        drain(&mut app, &mut rx).await;
        assert_eq!(confirm_lines(&mut app), ["Delete web version 1. Nothing in Pulp is deleted.", "The version number is never reused."]);
        assert!(server.received_requests().await.unwrap().is_empty(), "no derived repository to read");
    }

    #[tokio::test]
    async fn removing_protection_asks_to_type_the_environment() {
        let (mut app, mut rx, st) = composite_app().await;
        let lib = st.ensure_library("lab").await.unwrap();
        let prod = st.create_env("lab", "Prod", lib, false).await.unwrap();
        app.push(Kind::Environments);
        drain(&mut app, &mut rx).await;
        let protected = async || st.environments("lab").await.unwrap()[1].protected;
        let toggle = |app: &mut App, now: bool| {
            env_action(app, "lab", Act::ToggleProtected, json!({ "id": prod, "name": "Prod", "protected": now }));
        };
        toggle(&mut app, false);
        drain(&mut app, &mut rx).await;
        assert!(app.modal.is_none(), "adding protection asks nothing");
        assert!(protected().await);

        toggle(&mut app, true);
        drain(&mut app, &mut rx).await;
        let Some(Modal::Confirm { title, typed, on_yes, .. }) = app.modal.take() else { panic!("no confirmation") };
        assert_eq!((title.as_str(), typed.as_deref()), ("Remove protection from Prod", Some("Prod")));
        assert!(protected().await, "nothing before the confirmation");
        on_yes.unwrap()(&mut app);
        drain(&mut app, &mut rx).await;
        assert!(!protected().await);
        assert_eq!(last_notice(&app), (Level::Ok, "Prod protected: false"));
    }

    #[tokio::test]
    async fn delete_environment_shows_what_it_has_then_asks_to_type_its_name() {
        let (mut app, mut rx, st) = composite_app().await;
        let lib = st.ensure_library("lab").await.unwrap();
        let web = st.create_cv("lab", "web", "").await.unwrap();
        // Dev: a bound view and a later environment. QA: a served row without a binding.
        let dev = st.create_env("lab", "Dev", lib, false).await.unwrap();
        st.create_env("lab", "Prod", dev, false).await.unwrap();
        let v = publish_empty(&st, web).await;
        st.promoted(dev, web, v.id, "active").await.unwrap();
        let qa = st.create_env("lab", "QA", lib, false).await.unwrap();
        let dist = "/pulp/api/v3/distributions/rpm/rpm/1/";
        let row = crate::store::BindingRepo {
            env_id: qa,
            cv_id: web,
            repo_href: format!("{REPOS}rpm/rpm/1/"),
            repo_name: "os".into(),
            repo_type: RepoType::Rpm,
            distribution: dist.into(),
            base_path: "qa/web/os".into(),
            publication: Some("/p/1/".into()),
        };
        st.save_binding_repo(&row).await.unwrap();
        app.push(Kind::Environments);
        drain(&mut app, &mut rx).await;
        let delete = |app: &mut App, id: i64, name: &str| {
            env_action(app, "lab", Act::Delete, json!({ "id": id, "name": name, "prior": "Library", "label": name.to_lowercase() }));
        };
        let exists = async |id: i64| st.environments("lab").await.unwrap().iter().any(|e| e.id == id);

        delete(&mut app, dev, "Dev");
        assert!(app.jobs.iter().all(|j| j.is_load), "reading the impact changes nothing: quit does not warn");
        drain(&mut app, &mut rx).await;
        assert!(app.modal.is_none(), "a refusal shows before the typed confirmation");
        let why = "read what deleting Dev removes: Dev is in use: views bound to it: web (unpromote them first); \
                   environments after it: Prod (delete them first)";
        assert_eq!(last_notice(&app), (Level::Error, why));

        delete(&mut app, qa, "QA");
        drain(&mut app, &mut rx).await;
        let Some(Modal::Confirm { title, lines, typed, on_yes, .. }) = app.modal.take() else { panic!("no confirmation") };
        assert_eq!((title.as_str(), typed.as_deref()), ("Delete environment QA", Some("QA")));
        assert_eq!(
            lines,
            [
                "Delete lifecycle environment QA (label qa). No view is bound to it.".to_string(),
                "pulp-tui forgets these served rows; their distributions stay in Pulp:".into(),
                format!("  web/os  /qa/web/os  {dist}"),
            ]
        );
        assert!(exists(qa).await, "nothing before the confirmation");
        on_yes.unwrap()(&mut app);
        drain(&mut app, &mut rx).await;
        assert_eq!(last_notice(&app), (Level::Ok, "environment QA deleted"));
        assert!(!exists(qa).await);
        assert!(st.binding_repos(qa, web).await.unwrap().is_empty());
    }
}
