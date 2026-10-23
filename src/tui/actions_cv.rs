//! Content View, CV version and lifecycle environment actions.

use super::actions::{ap, pretty};
use super::app::{App, Level, NO_PROFILE, unavailable};
use super::form::{Field, FieldKind, Form, PickItem, Source};
use super::kinds::{Act, Kind, s};
use crate::cv::{Mode, base_path, distribution_name};
use crate::filters::{format_rules, parse_rules};
use crate::pulp::RepoType;
use crate::store::{CvRepo, LIBRARY, Store};
use anyhow::anyhow;
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
                ],
            );
            app.form(form, move |app, f| {
                let (n, d, p) = (f.get("name").to_string(), f.get("description").to_string(), p.clone());
                crate::store::validate_name(&n).map_err(|e| e.to_string())?;
                local(app, "create content view", async move |st| {
                    st.create_cv(&p, &n, &d).await?;
                    Ok(format!("content view {n} created; add repositories with a"))
                });
                Ok(())
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
                                ap(move |app| {
                                    app.ok(format!("{name} version {} published and active in Library", v.number));
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
            let lines = vec![
                format!("Delete content view {name} and everything pulp-tui created for it:"),
                "  - its distributions in every environment (consumers lose these URLs)".into(),
                "  - its snapshot repositories (with their versions and publications)".into(),
                "Source repositories are NOT touched.".into(),
            ];
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
                    "Break it only if that session is dead. Its workflow stops before its next step.".into(),
                ];
                app.confirm_danger(format!("Break lock on {name}"), lines, &name.clone(), move |app| {
                    app.job(format!("break lock on {name}"), move |ctx| async move {
                        let svc = ctx.cv()?;
                        let msg = match svc.store.break_lock(id).await? {
                            Some(h) => {
                                svc.audit("cv.lock.break", &format!("{name}: {} ({}) since {}", h.owner, h.operation, h.acquired_at)).await;
                                format!("lock on {name} broken")
                            }
                            None => format!("{name} is not locked"),
                        };
                        ap(move |app| {
                            app.ok(msg);
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
        // What promote will use: the prefix this server already has, else the setting.
        let prefix = st.stored_prefix(&svc.scope).await?.unwrap_or_else(|| svc.prefix.clone());
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
                base_path(&prefix, &name, &env, &r.repo_name)
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
                app.confirm(title, lines, go);
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
                let v = ctx.cv()?.locked(cv, &format!("resume v{number}")).await?.resume(vid, &ctx.log(), &ctx.on_task()).await?;
                ap(move |app| {
                    app.ok(format!("{name} v{} published", v.number));
                    app.reload();
                })
            });
        }
        Act::Delete => {
            let lines = vec![
                format!("Delete {name} version {number}: its snapshot version and publication are removed"),
                "from Pulp unless shared with another version. Refused while bound to an environment.".into(),
                "The version number is never reused.".into(),
            ];
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
