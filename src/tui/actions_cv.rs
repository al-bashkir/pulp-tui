//! Content View, CV version and lifecycle environment actions.

use super::actions::{ap, pretty};
use super::app::{App, Level};
use super::form::{Field, FieldKind, Form};
use super::kinds::{Act, Kind, s};
use crate::cv::{Mode, base_path, distribution_name};
use crate::filters::{format_rules, parse_rules};
use crate::pulp::RepoType;
use crate::store::{CvRepo, LIBRARY};
use anyhow::anyhow;
use serde_json::Value;

pub fn run(app: &mut App, act: Act, item: Value) {
    let Some(profile) = app.profile.clone() else {
        app.error("select a server profile first (Content Views belong to a server)");
        return;
    };
    match app.list.kind.clone() {
        Kind::ContentViews => cv_action(app, &profile, act, item),
        Kind::CvVersions { cv, name } => version_action(app, &profile, cv, name, act, item),
        Kind::Environments => env_action(app, &profile, act, item),
        _ => {}
    }
}

fn local<F>(app: &mut App, label: &str, f: F)
where
    F: FnOnce(&crate::store::Store) -> anyhow::Result<String> + Send + 'static,
{
    let store = app.store.clone();
    let fut = async move {
        let msg = tokio::task::spawn_blocking(move || f(&store)).await??;
        ap(move |app| {
            app.ok(msg);
            app.reload();
        })
    };
    app.spawn(label.into(), false, fut);
}

fn env_names(app: &App, profile: &str) -> Vec<String> {
    let _ = app.store.ensure_library(profile);
    app.store.environments(profile).map(|e| e.into_iter().map(|e| e.name).collect()).unwrap_or_default()
}

fn cv_action(app: &mut App, profile: &str, act: Act, item: Value) {
    let id = item["id"].as_i64().unwrap_or_default();
    let name = s(&item, "name");
    let repos: Vec<String> = item["repos"].as_array().into_iter().flatten().map(|r| s(r, "name")).collect();
    let p = profile.to_string();
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
                local(app, "create content view", move |st| {
                    st.create_cv(&p, &n, &d)?;
                    Ok(format!("content view {n} created; add repositories with a"))
                });
                Ok(())
            });
        }
        Act::Edit => {
            let form = Form::new(format!("Edit {name}"), vec![Field::text("description", "Description").value(s(&item, "description"))]);
            app.form(form, move |app, f| {
                let d = f.get("description").to_string();
                local(app, "update content view", move |st| {
                    st.update_cv(id, &d)?;
                    Ok("saved".into())
                });
                Ok(())
            });
        }
        Act::CvAddRepo => {
            let form = Form::new(
                format!("Add repository to {name}"),
                vec![
                    Field::new("type", "Type", FieldKind::Choice(vec!["rpm".into(), "file".into()])),
                    Field::text("repo", "Repository (name)").required(),
                    Field::text("filters", "Filter rules")
                        .help("rpm: include name=x* arch=.. evr>=1.0; exclude name=*-debuginfo | file: exclude path=isos/*"),
                ],
            )
            .note("Each repository keeps its own type and distribution; formats are never merged.");
            app.form(form, move |app, f| {
                let t = if f.get("type") == "file" { RepoType::File } else { RepoType::Rpm };
                let filters = parse_rules(f.get("filters"), t)?;
                let repo = f.get("repo").to_string();
                app.job(format!("add {repo} to {}", f.get("repo")), move |ctx| async move {
                    let href = super::actions::href_by_name(&ctx.client, t.repositories(), &repo).await?;
                    ctx.store.set_cv_repo(id, &CvRepo { repo_href: href, repo_name: repo.clone(), repo_type: t, filters })?;
                    ap(move |app| {
                        app.ok(format!("{repo} added (takes effect on next publish)"));
                        app.reload();
                    })
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
            let defs = app.store.cv_repos(id).unwrap_or_default();
            app.form(form, move |app, f| {
                let r = defs.iter().find(|d| d.repo_name == f.get("repo")).cloned().ok_or("unknown repository")?;
                if act == Act::CvRemoveRepo {
                    local(app, "remove repository", move |st| {
                        st.remove_cv_repo(id, &r.repo_href)?;
                        Ok(format!("{} removed from the definition; published versions are unchanged", r.repo_name))
                    });
                } else {
                    let filters = parse_rules(f.get("filters"), r.repo_type)?;
                    local(app, "update filters", move |st| {
                        let msg = format!("filters of {}: {}", r.repo_name, format_rules(&filters));
                        st.set_cv_repo(id, &CvRepo { filters, ..r })?;
                        Ok(msg)
                    });
                }
                Ok(())
            });
        }
        Act::CvPreview => app.job(format!("preview {name}"), move |ctx| async move {
            let latest = ctx.store.versions(id)?.into_iter().find(|v| v.state == "published").map(|v| v.id);
            let lines = ctx.cv().preview(id, latest).await?;
            ap(move |app| app.text(format!("Preview of {name} (draft vs latest published)"), lines))
        }),
        Act::CvPublish => {
            let form = Form::new(format!("Publish {name}"), vec![Field::text("notes", "Change notes")])
                .note("A preview of the exact content is shown before anything is created.");
            app.form(form, move |app, f| {
                let notes = f.get("notes").to_string();
                let name = name.clone();
                app.job(format!("preview {name}"), move |ctx| async move {
                    let latest = ctx.store.versions(id)?.into_iter().find(|v| v.state == "published").map(|v| v.id);
                    let mut lines = ctx.cv().preview(id, latest).await?;
                    lines.insert(0, "Publishing creates a new immutable version and makes it active in Library.".into());
                    lines.insert(1, String::new());
                    ap(move |app| {
                        app.confirm(format!("Publish {name}"), lines, move |app| {
                            app.job(format!("publish {name}"), move |ctx| async move {
                                let v = ctx.cv().publish(id, &notes, &ctx.log(), &ctx.on_task()).await?;
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
            let envs = env_names(app, profile);
            let published: Vec<String> =
                item["versions"].as_array().into_iter().flatten().filter(|v| v["state"] == "published").map(|v| s(v, "number")).collect();
            if published.is_empty() {
                app.error("no published versions");
                return;
            }
            let mode = if act == Act::Promote { Mode::Promote } else { Mode::Rollback };
            let form = Form::new(
                format!("{} {name}", if mode == Mode::Promote { "Promote" } else { "Roll back" }),
                vec![
                    Field::new("version", "Version", FieldKind::Choice(published)),
                    Field::new("env", "Environment", FieldKind::Choice(envs)),
                ],
            );
            app.form(form, move |app, f| {
                let number: i64 = f.get("version").parse().map_err(|_| "pick a version")?;
                let vid = app
                    .store
                    .versions(id)
                    .ok()
                    .and_then(|vs| vs.into_iter().find(|v| v.number == number))
                    .map(|v| v.id)
                    .ok_or("unknown version")?;
                promote_preview(app, &p, id, name.clone(), vid, f.get("env").to_string(), mode);
                Ok(())
            });
        }
        Act::CvDrift => app.job(format!("verify {name}"), move |ctx| async move {
            let mut lines = vec![];
            for e in ctx.store.environments(ctx.client.profile())? {
                if let Some(b) = ctx.store.binding(e.id, id)? {
                    let n = ctx.store.version(b.version_id)?.number;
                    let issues = ctx.cv().verify(id, e.id).await?;
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
                    ctx.cv().delete_view(id, &ctx.log(), &ctx.on_task()).await?;
                    ap(move |app| {
                        app.ok(format!("content view {name} deleted"));
                        app.reload();
                    })
                });
            });
        }
        _ => {}
    }
}

/// Show the exact distribution changes of a promotion/rollback, then run it.
fn promote_preview(app: &mut App, profile: &str, cv: i64, name: String, vid: i64, env: String, mode: Mode) {
    let Some(e) = app.store.environments(profile).ok().and_then(|es| es.into_iter().find(|e| e.name == env)) else {
        app.error(format!("unknown environment {env}"));
        return;
    };
    let prefix = app.cfg.settings.cv_base_path_prefix.clone();
    app.job(format!("preview {name} -> {env}"), move |ctx| async move {
        let st = &ctx.store;
        let v = st.version(vid)?;
        let current = st.binding(e.id, cv)?;
        let cur_num = match &current {
            Some(b) => format!("v{} ({})", st.version(b.version_id)?.number, b.state),
            None => "nothing".into(),
        };
        let mut lines = vec![format!("{name}: {env} now serves {cur_num}; will serve v{}", v.number), String::new()];
        let vrs = st.version_repos(vid)?;
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
        for b in st.binding_repos(e.id, cv)? {
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
            let ok = st.binding(prior, cv)?.is_some_and(|b| b.version_id == vid && b.state == "active");
            if !ok {
                lines.push(String::new());
                lines.push("BLOCKED: the version is not active in the prior environment.".into());
            }
        }
        let protected = e.protected;
        let (title, danger_title, typed) = (format!("{name} -> {env}"), format!("{env} is protected"), env.clone());
        ap(move |app| {
            let go = move |app: &mut App| {
                app.job(
                    format!("{} {name} v{} -> {env}", if mode == Mode::Promote { "promote" } else { "rollback" }, v.number),
                    move |ctx| async move {
                        let res = ctx.cv().promote(cv, vid, e.id, mode, &ctx.log(), &ctx.on_task()).await;
                        let issues = ctx.cv().verify(cv, e.id).await.unwrap_or_else(|e| vec![e.to_string()]);
                        ap(move |app| {
                            match (res, issues.is_empty()) {
                                (Ok(()), true) => app.ok(format!("{name} v{} active in {env}; served content verified", v.number)),
                                (Ok(()), false) => {
                                    app.notify(Level::Warn, format!("{name} in {env}: bound, verification found: {}", issues.join("; ")))
                                }
                                (Err(e), _) => {
                                    app.error(format!("{e:#} (partial states are recorded; promote again to retry or roll back)"))
                                }
                            }
                            app.reload();
                        })
                    },
                );
            };
            if protected {
                app.confirm_danger(danger_title, lines, &typed, go);
            } else {
                app.confirm(title, lines, go);
            }
        })
    });
}

fn version_action(app: &mut App, profile: &str, cv: i64, name: String, act: Act, item: Value) {
    let vid = item["id"].as_i64().unwrap_or_default();
    let number = s(&item, "number");
    match act {
        Act::Promote | Act::Rollback => {
            if item["state"] != "published" {
                app.error("only published versions can be bound to environments");
                return;
            }
            let envs = env_names(app, profile);
            let mode = if act == Act::Promote { Mode::Promote } else { Mode::Rollback };
            let p = profile.to_string();
            app.form(
                Form::new(format!("v{number} -> environment"), vec![Field::new("env", "Environment", FieldKind::Choice(envs))]),
                move |app, f| {
                    promote_preview(app, &p, cv, name.clone(), vid, f.get("env").to_string(), mode);
                    Ok(())
                },
            );
        }
        Act::Compare => {
            let marked: Vec<i64> = app.list.marked.iter().filter_map(|m| m.parse().ok()).filter(|m| *m != vid).collect();
            let [other] = marked.as_slice() else {
                app.error("mark exactly one other version with Space, then compare");
                return;
            };
            let (a, b) = if *other < vid { (*other, vid) } else { (vid, *other) };
            app.job("compare content view versions", move |ctx| async move {
                let lines = ctx.cv().compare_versions(a, b).await?;
                ap(move |app| app.text(format!("{name}: version comparison"), lines))
            });
        }
        Act::Resume => {
            if item["state"] == "published" {
                app.notify(Level::Info, "version is already published");
                return;
            }
            app.job(format!("resume {name} v{number}"), move |ctx| async move {
                let v = ctx.cv().resume(vid, &ctx.log(), &ctx.on_task()).await?;
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
                    ctx.cv().delete_version(vid, &ctx.log(), &ctx.on_task()).await?;
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

fn env_action(app: &mut App, profile: &str, act: Act, item: Value) {
    let id = item["id"].as_i64().unwrap_or_default();
    let name = s(&item, "name");
    let p = profile.to_string();
    match act {
        Act::Create => {
            let envs = env_names(app, profile);
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
                local(app, "create environment", move |st| {
                    let pid = st
                        .environments(&p)?
                        .into_iter()
                        .find(|e| e.name == prior)
                        .map(|e| e.id)
                        .ok_or_else(|| anyhow!("unknown prior {prior}"))?;
                    st.create_env(&p, &n, pid, prot)?;
                    Ok(format!("environment {n} created after {prior}"))
                });
                Ok(())
            });
        }
        Act::ToggleProtected => {
            let prot = item["protected"] != true;
            local(app, "toggle protected", move |st| {
                st.set_env_protected(id, prot)?;
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
                    local(app, "delete environment", move |st| {
                        st.delete_env(id)?;
                        Ok(format!("environment {name} deleted"))
                    })
                },
            );
        }
        _ => {}
    }
}
