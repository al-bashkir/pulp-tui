//! Content View, CV version and lifecycle environment actions.

use super::actions::{ap, caps, confirm_mirror, pretty, sync_policies};
use super::app::{App, Ctx, Level, NO_PROFILE, unavailable};
use super::form::{Field, FieldKind, Form, PickItem, Source};
use super::kinds::{Act, Kind, s};
use crate::cv::{Mode, check_repo_clash, distribution_name};
use crate::filters::{format_rules, parse_rules};
use crate::pulp::ops::{self, OnTask};
use crate::pulp::{Mutation, PulpClient, RepoType};
use crate::store::{Component, CvRepo, CvVersion, LIBRARY, Store};
use anyhow::anyhow;
use futures::future::join_all;
use serde_json::Value;
use std::future::Future;
use std::sync::Arc;

pub fn run(app: &mut App, act: Act, item: Value) {
    let Some(profile) = app.profile.clone() else {
        app.error(NO_PROFILE);
        return;
    };
    let Some((_, scope)) = cv_target(app, &profile) else { return };
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

/// The Content View store and the scope of profile `name` (`App::scope`); on failure, an error notice and `None`.
pub fn cv_target(app: &mut App, name: &str) -> Option<(Arc<Store>, String)> {
    let st = cv_store(app)?;
    match app.scope(name) {
        Ok(scope) => Some((st, scope)),
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
        return Err("filter rules need repositories of one type; add RPM and File repositories separately".into());
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

/// The notice of a publish with the results of the auto publish that followed: a warning when one failed.
fn auto_notice(msg: String, auto: Vec<Result<String, String>>) -> (Level, String) {
    if auto.is_empty() {
        return (Level::Ok, msg);
    }
    let level = if auto.iter().any(Result::is_err) { Level::Warn } else { Level::Ok };
    let results: Vec<String> = auto.into_iter().map(|r| r.unwrap_or_else(|e| e)).collect();
    (level, format!("{msg}; auto publish: {}", results.join("; ")))
}

/// What happened to one repository of a "Sync repositories" run.
#[derive(Debug, PartialEq)]
enum RepoSync {
    /// The sync task (href) is running.
    Submitted(String),
    /// The task finished and created this repository version (href).
    NewVersion(String),
    NoChange,
    /// The error text: a rejected request, a failed or canceled task, or a task that could not be read.
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
        // `wait_task` accepts a skipped task as done, but a skipped sync did not sync.
        Ok(t) if t.state == "skipped" => RepoSync::Failed("task skipped".into()),
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
            app.track_task(h, &format!("sync {name} ({policy})"));
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
                        .help("rpm: include name=x* arch=.. evr>=1.0; exclude name=*-debuginfo | file: exclude path=isos/*"),
                ],
            )
            .note("Each repository keeps its own type and distribution; formats are never merged.")
            .note("Filter rules apply to every selected repository, so they need repositories of one type.");
            app.form(form, move |app, f| {
                let repos = new_cv_repos(&f.picked("repos"), f.get("filters"))?;
                local(app, "add repositories", async move |st| {
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
                let picked = f
                    .picked("components")
                    .into_iter()
                    .filter_map(|i| Some(Component { cv_id: i.value.parse().ok()?, name: i.label, pinned: None }))
                    .collect();
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
            let form = Form::new(format!("Publish {name}"), vec![Field::text("notes", "Change notes")])
                .note("A preview of the exact content is shown before anything is created.");
            app.form(form, move |app, f| {
                let notes = f.get("notes").to_string();
                let name = name.clone();
                app.job(format!("preview {name}"), move |ctx| async move {
                    let svc = ctx.cv()?;
                    let latest = svc.store.versions(id).await?.into_iter().find(|v| v.state == "published").map(|v| v.id);
                    let mut lines = svc.preview(id, latest).await?;
                    lines.insert(0, "Publishing creates a new immutable version and makes it active in Library.".into());
                    lines.insert(1, String::new());
                    ap(move |app| {
                        app.confirm(format!("Publish {name}"), lines, move |app| {
                            app.job(format!("publish {name}"), move |ctx| async move {
                                let v = ctx.cv()?.locked(id, "publish").await?.publish(id, &notes, &ctx.log(), &ctx.on_task()).await?;
                                // After the lock of this view is released; composites take their own locks.
                                let auto = ctx.cv()?.auto_publish(id, &ctx.log(), &ctx.on_task()).await;
                                ap(move |app| {
                                    let (level, text) =
                                        auto_notice(format!("{name} version {} published and active in Library", v.number), auto);
                                    app.notify(level, text);
                                    app.reload();
                                })
                            });
                        })
                    })
                });
                Ok(())
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
            let t = if has("rpm") { RepoType::Rpm } else { RepoType::File };
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
            if has("rpm") && has("file") {
                form = form.note("File repositories: additive stays additive; any mirror policy means mirror.");
            }
            // All marked, so the picker needs no server request.
            form.set_options(0, Ok(items));
            app.form(form, move |app, f| {
                let repos = new_cv_repos(&f.picked("repos"), "")?;
                let (policy, name) = (f.get("policy").to_string(), name.clone());
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
                confirm_mirror(app, &policy, go);
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
                        promote_preview(app, id, name.clone(), vid, f.get("env").to_string(), mode);
                        Ok(())
                    });
                })
            });
        }
        Act::CvDrift => app.job(format!("verify {name}"), move |ctx| async move {
            let (svc, mut lines) = (ctx.cv()?, vec![]);
            for e in svc.store.environments(&svc.scope).await? {
                if let Some(b) = svc.store.binding(e.id, id).await? {
                    let n = svc.store.version(b.version_id).await?.number;
                    let issues = svc.verify(id, e.id).await?;
                    lines.push(format!(
                        "{}: v{n} ({}) {}",
                        e.name,
                        b.state,
                        if issues.is_empty() { "OK - served content matches" } else { "DRIFT" }
                    ));
                    lines.extend(issues.into_iter().map(|i| format!("    {i}")));
                }
            }
            if lines.is_empty() {
                lines.push("not bound to any environment".into());
            } else {
                lines.push(String::new());
                lines.push("Reconcile drift by promoting the bound version again (idempotent).".into());
            }
            ap(move |app| app.text(format!("Verification of {name}"), lines))
        }),
        Act::Delete => {
            // A composite owns distributions only: its versions borrow the components' objects.
            let lines = if item["composite"] == true {
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
            app.confirm_danger(format!("Delete content view {name}"), lines, &name.clone(), move |app| {
                app.job(format!("delete content view {name}"), move |ctx| async move {
                    ctx.cv()?.locked(id, "delete").await?.delete_view(id, &ctx.log(), &ctx.on_task()).await?;
                    ap(move |app| {
                        app.ok(format!("content view {name} deleted"));
                        app.reload();
                    })
                });
            });
        }
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

/// Show the exact distribution changes of a promotion/rollback, then run it.
fn promote_preview(app: &mut App, cv: i64, name: String, vid: i64, env: String, mode: Mode) {
    app.job(format!("preview {name} -> {env}"), move |ctx| async move {
        let svc = ctx.cv()?;
        let st = &svc.store;
        let envs = st.environments(&svc.scope).await?;
        let e = envs.into_iter().find(|e| e.name == env).ok_or_else(|| anyhow!("unknown environment {env}"))?;
        let v = st.version(vid).await?;
        let current = st.binding(e.id, cv).await?;
        let cur_num = match &current {
            Some(b) => format!("v{} ({})", st.version(b.version_id).await?.number, b.state),
            None => "nothing".into(),
        };
        let mut lines = vec![format!("{name}: {env} now serves {cur_num}; will serve v{}", v.number), String::new()];
        let vrs = st.version_repos(vid).await?;
        for vr in &vrs {
            let r = vr.repo();
            lines.push(format!(
                "  {} ({}): distribution {} at /{}",
                r.repo_name,
                r.repo_type.label(),
                distribution_name(&name, &env, &r.repo_name),
                svc.served_path(&name, &e, r).await?
            ));
        }
        for b in st.binding_repos(e.id, cv).await? {
            if !vrs.iter().any(|vr| vr.repo().repo_href == b.repo_href) && b.publication.is_some() {
                lines.push(format!("  {}: not part of v{} -> its distribution stops serving (kept for rollback)", b.repo_name, v.number));
            }
        }
        if mode == Mode::Rollback {
            lines.push(String::new());
            lines.push("Rollback only changes bindings; newer versions stay available.".into());
            lines.push("Files of the replaced publications may stay downloadable for Pulp's".into());
            lines.push("DISTRIBUTED_PUBLICATION_RETENTION_PERIOD; repository metadata switches immediately.".into());
        } else if let Some(prior) = e.prior_id {
            let ok = st.binding(prior, cv).await?.is_some_and(|b| b.version_id == vid && b.state == "active");
            if !ok {
                lines.push(String::new());
                lines.push("BLOCKED: the version is not active in the prior environment.".into());
            }
        }
        let protected = e.protected;
        let (title, danger_title, typed) = (format!("{name} -> {env}"), format!("{env} is protected"), env.clone());
        ap(move |app| {
            let go = move |app: &mut App| {
                let verb = if mode == Mode::Promote { "promote" } else { "rollback" };
                app.job(format!("{verb} {name} v{} -> {env}", v.number), move |ctx| async move {
                    let svc = ctx.cv()?.locked(cv, &format!("{verb} to {env}")).await?;
                    let res = svc.promote(cv, vid, e.id, mode, &ctx.log(), &ctx.on_task()).await;
                    let issues = svc.verify(cv, e.id).await.unwrap_or_else(|e| vec![e.to_string()]);
                    ap(move |app| {
                        match (res, issues.is_empty()) {
                            (Ok(()), true) => app.ok(format!("{name} v{} active in {env}; served content verified", v.number)),
                            (Ok(()), false) => {
                                app.notify(Level::Warn, format!("{name} in {env}: bound, verification found: {}", issues.join("; ")))
                            }
                            (Err(e), _) => app.error(format!("{e:#} (partial states are recorded; promote again to retry or roll back)")),
                        }
                        app.reload();
                    })
                });
            };
            if protected {
                app.confirm_danger(danger_title, lines, &typed, go);
            } else {
                app.confirm_prod(title, lines, &typed, go);
            }
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
                            promote_preview(app, cv, name.clone(), vid, f.get("env").to_string(), mode);
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
            if item["state"] == "published" {
                app.notify(Level::Info, "version is already published");
                return;
            }
            app.job(format!("resume {name} v{number}"), move |ctx| async move {
                let svc = ctx.cv()?.locked(cv, &format!("resume v{number}")).await?;
                // A stale list can show a published version as failed: resume returns at once, nothing new to auto publish.
                let was_published = svc.store.version(vid).await?.state == "published";
                let v = svc.resume(vid, &ctx.log(), &ctx.on_task()).await?;
                drop(svc); // release the lock of this view; composites take their own
                let auto = if was_published { vec![] } else { ctx.cv()?.auto_publish(cv, &ctx.log(), &ctx.on_task()).await };
                ap(move |app| {
                    let (level, text) = auto_notice(format!("{name} v{} published", v.number), auto);
                    app.notify(level, text);
                    app.reload();
                })
            });
        }
        Act::Delete => {
            // Only a composite version has components; it borrows their snapshots and publications.
            let mut lines = if item["components"].as_array().is_some_and(|c| !c.is_empty()) {
                vec![
                    format!("Delete {name} version {number}: only this composite version is removed."),
                    "The snapshots and publications of its component versions are NOT touched. Refused while bound to an environment."
                        .into(),
                ]
            } else {
                vec![
                    format!("Delete {name} version {number}: its snapshot version and publication are removed"),
                    "from Pulp unless shared with another version. Refused while bound to an environment.".into(),
                ]
            };
            lines.push("The version number is never reused.".into());
            app.confirm_danger(format!("Delete {name} v{number}"), lines, &format!("v{number}"), move |app| {
                app.job(format!("delete {name} v{number}"), move |ctx| async move {
                    ctx.cv()?.locked(cv, &format!("delete v{number}")).await?.delete_version(vid, &ctx.log(), &ctx.on_task()).await?;
                    ap(move |app| {
                        app.ok("version deleted");
                        app.reload();
                    })
                });
            });
        }
        Act::WatchTask => {
            let Some(st) = cv_store(app) else { return };
            // Read again: the list is stale while another operator publishes.
            app.spawn("read pending task".into(), true, async move {
                // Repositories publish one at a time, so at most one row has a pending task.
                let pending = st.version_repos(vid).await?.into_iter().find_map(|r| Some((r.repo().repo_name.clone(), r.pending_task?)));
                ap(move |app| match pending {
                    Some((repo, h)) => app.watch_task(&h, &format!("{name} v{number}: copy {repo}")),
                    None => app.notify(Level::Info, format!("v{number} has no pending Pulp task now (only the copy step stores one)")),
                })
            });
        }
        Act::Inspect => app.text(format!("{name} v{number}"), pretty(&item)),
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
            local(app, "toggle protected", async move |st| {
                st.set_env_protected(id, prot).await?;
                Ok(format!("{name} protected: {prot}"))
            });
        }
        Act::Delete => {
            if name == LIBRARY && item["prior"] == "" {
                app.error("Library cannot be deleted");
                return;
            }
            app.confirm(
                format!("Delete environment {name}"),
                vec!["Only allowed when no content view is bound and no environment follows it.".into()],
                move |app| {
                    local(app, "delete environment", async move |st| {
                        st.delete_env(id).await?;
                        Ok(format!("environment {name} deleted"))
                    })
                },
            );
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
        PickItem { label: name.into(), value: format!("/pulp/api/v3/repositories/{t}/{t}/{name}/"), note: t.into() }
    }

    #[test]
    fn mixed_types_with_rules_rejected() {
        let e = new_cv_repos(&[item("a", "rpm"), item("b", "file")], "exclude name=*-debuginfo").unwrap_err();
        assert!(e.contains("one type"), "{e}");
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
        assert_eq!(out[0].1, RepoSync::Failed("task skipped".into()));
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
    async fn cv_sync_form_marks_every_repository() {
        let mut app = test_app().await;
        let both = json!([
            { "name": "os", "type": "rpm", "href": format!("{REPOS}rpm/rpm/1/") },
            { "name": "iso", "type": "file", "href": format!("{REPOS}file/file/2/") }
        ]);
        let form = sync_form(&mut app, both).expect("no form");
        assert_eq!((form.picked("repos").len(), form.fields[0].shown.as_str()), (2, "2 selected"));
        assert_eq!(form.fields[1].kind, FieldKind::Choice(super::super::actions::sync_policies(None, RepoType::Rpm)));
        // A file-only view gets the File values.
        let files = json!([{ "name": "iso", "type": "file", "href": format!("{REPOS}file/file/2/") }]);
        let form = sync_form(&mut app, files).expect("no form");
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
    async fn watch_opens_the_pending_copy_task_of_a_version() {
        let (mut app, mut rx, st) = composite_app().await;
        let cv = st.create_cv("lab", "web", "").await.unwrap();
        let v = st.begin_version(cv, "", &[cv_repo("os", "rpm", "1")]).await.unwrap();
        let row = json!({ "id": v.id, "number": v.number });
        version_action(&mut app, "lab", cv, "web".into(), Act::WatchTask, row.clone());
        drain(&mut app, &mut rx).await;
        assert!(app.modal.is_none());
        assert_eq!(last_notice(&app), (Level::Info, "v1 has no pending Pulp task now (only the copy step stores one)"));

        let mut vr = st.version_repos(v.id).await.unwrap().remove(0);
        vr.pending_task = Some(format!("{TASKS}7/"));
        st.save_version_repo(&vr).await.unwrap();
        version_action(&mut app, "lab", cv, "web".into(), Act::WatchTask, row);
        drain(&mut app, &mut rx).await;
        let Some(Modal::Text { title, watch, .. }) = &app.modal else { panic!("no watch view") };
        assert_eq!((title.as_str(), watch.clone()), ("Watch task web v1: copy os", Some(format!("{TASKS}7/"))));
        assert_eq!(app.tasks.items[0].href, format!("{TASKS}7/"), "tracked, so the Task Center polls it");
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
        assert_eq!(auto_notice("x published".into(), vec![]), (Level::Ok, "x published".into()));
        assert_eq!(
            auto_notice("x published".into(), vec![ok("a v2 published"), ok("b v1 published")]),
            (Level::Ok, "x published; auto publish: a v2 published; b v1 published".into())
        );
        assert_eq!(
            auto_notice("x published".into(), vec![ok("a v2 published"), Err("b: locked".into())]),
            (Level::Warn, "x published; auto publish: a v2 published; b: locked".into())
        );
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
    async fn resume_of_an_already_published_version_does_not_auto_publish() {
        let server = MockServer::start().await;
        let (mut app, mut rx, st) = composite_app().await;
        app.conn = Some(crate::tui::app::Conn { client: client(&server), caps: Default::default(), scope: "lab".into() });
        let (c, _, extras, _) = rocky(&st).await;
        st.set_auto_publish(c, true).await.unwrap();
        let v = st.versions(extras).await.unwrap().remove(0);
        // The list is stale: it shows `failed`, the database says `published`.
        let item = json!({ "id": v.id, "number": v.number, "state": "failed" });
        version_action(&mut app, "lab", extras, "extras".into(), Act::Resume, item);
        drain(&mut app, &mut rx).await;
        assert!(st.versions(c).await.unwrap().is_empty(), "no composite version for a version that was already published");
    }

    fn confirm_lines(app: &mut App) -> Vec<String> {
        match app.modal.take() {
            Some(Modal::Confirm { lines, .. }) => lines,
            _ => panic!("no confirmation"),
        }
    }

    #[tokio::test]
    async fn deleting_a_composite_does_not_promise_snapshot_removal() {
        let mut app = test_app().await;
        cv_action(&mut app, "lab", Act::Delete, json!({ "id": 1, "name": "rocky9", "composite": true }));
        assert_eq!(
            confirm_lines(&mut app),
            [
                "Delete composite content view rocky9: its distributions in every environment (consumers lose these URLs).",
                "Component views and their snapshots are NOT touched."
            ]
        );
        cv_action(&mut app, "lab", Act::Delete, json!({ "id": 1, "name": "web", "composite": false }));
        assert!(confirm_lines(&mut app).iter().any(|l| l.contains("its snapshot repositories (with their versions and publications)")));

        let version = |components: Value| json!({ "id": 5, "number": 2, "state": "published", "components": components });
        version_action(&mut app, "lab", 1, "rocky9".into(), Act::Delete, version(json!(["os v1"])));
        let lines = confirm_lines(&mut app);
        assert!(
            lines.iter().any(|l| l.contains("NOT touched")) && !lines.iter().any(|l| l.contains("snapshot version and publication")),
            "{lines:?}"
        );
        version_action(&mut app, "lab", 1, "web".into(), Act::Delete, version(json!([])));
        assert!(confirm_lines(&mut app).iter().any(|l| l.contains("its snapshot version and publication are removed")));
    }
}
