//! Users, groups, roles (Pulp RBAC) and replication actions.

use super::actions::{ap, caps, submit, url_with_scheme};
use super::app::{App, Level};
use super::form::{Field, FieldKind, Form, Source};
use super::kinds::{Act, Kind, s};
use crate::pulp::{PulpClient, RepoType};
use crate::replication::{self, PlanAction};
use anyhow::Context;
use serde_json::{Value, json};

pub fn run(app: &mut App, act: Act, item: Value) {
    match app.list.kind.clone() {
        Kind::Users => user_action(app, act, item),
        Kind::UserRoles { user, name } => role_assignment(app, act, item, user, name, false),
        Kind::Groups => group_action(app, act, item),
        Kind::GroupRoles { group, name } => role_assignment(app, act, item, group, name, true),
        Kind::GroupUsers { group, name } => member_action(app, act, item, group, name),
        Kind::Roles => role_action(app, act, item),
        Kind::UpstreamPulps => upstream_action(app, act, item),
        _ => {}
    }
}

fn me(app: &App) -> Option<String> {
    app.conn.as_ref().and_then(|c| c.caps.user.as_ref().map(|u| u.username.clone()))
}

/// Roles that grant broad or administrative access deserve an explicit warning.
fn is_powerful(role: &str) -> bool {
    role.ends_with("_owner") || role.contains("admin") || role.starts_with("core.") && role.ends_with("_creator")
}

fn user_action(app: &mut App, act: Act, item: Value) {
    let href = s(&item, "pulp_href");
    let username = s(&item, "username");
    let is_me = me(app).as_deref() == Some(username.as_str());
    match act {
        Act::Roles => app.push(Kind::UserRoles { user: href, name: username }),
        Act::Create | Act::Edit => {
            let edit = act == Act::Edit;
            let is_me = edit && is_me;
            let v = if edit { item.clone() } else { json!({ "is_active": true }) };
            let mut fields = vec![];
            if !edit {
                fields.push(Field::text("username", "Username").required());
                fields.push(Field::new("password", "Password", FieldKind::Secret).help("only for Pulp's local user database"));
            }
            fields.extend([
                Field::text("first_name", "First name").value(s(&v, "first_name")),
                Field::text("last_name", "Last name").value(s(&v, "last_name")),
                Field::text("email", "Email").value(s(&v, "email")),
                Field::new("is_staff", "Staff (bypasses RBAC)", FieldKind::Bool).value(if v["is_staff"] == true {
                    "true"
                } else {
                    "false"
                }),
                Field::new("is_active", "Active", FieldKind::Bool).value(if v["is_active"] == true { "true" } else { "false" }),
            ]);
            let form = Form::new(if edit { format!("Edit user {username}") } else { "New user".into() }, fields)
                .note("Users from an external identity provider must be managed there, not through Pulp.");
            app.form(form, move |app, f| {
                let mut b = json!({});
                for k in ["username", "password", "first_name", "last_name", "email"] {
                    if f.fields.iter().any(|x| x.key == k) && (!edit || f.changed(k)) && !(k == "password" && f.get(k).is_empty()) {
                        b[k] = f.get(k).into();
                    }
                }
                for k in ["is_staff", "is_active"] {
                    if !edit || f.changed(k) {
                        b[k] = f.bool(k).into();
                    }
                }
                let mut warn = vec![];
                if b["is_staff"] == true {
                    warn.push("PRIVILEGE ESCALATION: staff users have full access and bypass RBAC.".to_string());
                }
                if is_me && (b["is_staff"] == false || b["is_active"] == false) {
                    warn.push("You are changing YOUR OWN account: you may lose access.".into());
                }
                let (href, label) =
                    (href.clone(), format!("{} user {}", if edit { "update" } else { "create" }, f.opt("username").unwrap_or(&username)));
                let go = move |app: &mut App| {
                    submit(app, label, move |c| async move {
                        Ok(if edit { c.patch(&href, &b).await? } else { c.post(&c.api("users/"), &b).await? })
                    })
                };
                if warn.is_empty() {
                    go(app);
                } else {
                    app.confirm("Confirm user change", warn, go);
                }
                Ok(())
            });
        }
        Act::Password => {
            let form = Form::new(
                format!("Set password of {username}"),
                vec![
                    Field::new("p1", "New password", FieldKind::Secret).required(),
                    Field::new("p2", "Repeat password", FieldKind::Secret).required(),
                ],
            )
            .note("Works only for users in Pulp's own database (not LDAP/SSO/remote-user backends).");
            app.form(form, move |app, f| {
                if f.get("p1") != f.get("p2") {
                    return Err("passwords differ".into());
                }
                let (href, b) = (href.clone(), json!({ "password": f.get("p1") }));
                submit(app, format!("set password of {username}"), move |c| async move { Ok(c.patch(&href, &b).await?) });
                Ok(())
            });
        }
        Act::ToggleActive => {
            let active = item["is_active"] != true;
            let mut lines = vec![format!("{} user {username}.", if active { "Enable" } else { "Disable" })];
            if is_me && !active {
                lines.push("WARNING: this is YOUR account; you will lose access.".into());
            }
            app.confirm("Change user state", lines, move |app| {
                submit(app, format!("set {username} active={active}"), move |c| async move {
                    Ok(c.patch(&href, &json!({ "is_active": active })).await?)
                })
            });
        }
        Act::Delete => {
            if is_me {
                app.error("refused: you cannot delete the account you are logged in with");
                return;
            }
            let mut lines = vec![format!("Delete user {username} and all of their role assignments.")];
            if item["is_staff"] == true {
                lines.push("WARNING: this is a staff (administrator) account.".into());
            }
            lines.push("Prefer disabling (A) if the account may be needed again.".into());
            app.confirm_danger(format!("Delete user {username}"), lines, &username.clone(), move |app| {
                submit(app, format!("delete user {username}"), move |c| async move { Ok(c.delete(&href).await?) })
            });
        }
        _ => {}
    }
}

fn role_assignment(app: &mut App, act: Act, item: Value, owner: String, owner_name: String, group: bool) {
    let who = if group { format!("group {owner_name}") } else { format!("user {owner_name}") };
    let is_me = !group && me(app).as_deref() == Some(owner_name.as_str());
    match act {
        Act::Create => assign_role(app, owner, who),
        Act::Delete => {
            let href = s(&item, "pulp_href");
            let role = s(&item, "role");
            let mut lines = vec![format!("Remove role {role} from {who}.")];
            if is_me {
                lines.push("WARNING: this is YOUR account; you may lose permissions you need right now.".into());
            }
            app.confirm("Remove role", lines, move |app| {
                submit(app, format!("remove {role}"), move |c| async move { Ok(c.delete(&href).await?) })
            });
        }
        _ => {}
    }
}

/// The Assign role form for `owner` (a user or group href; `who` names it). A powerful role asks first.
fn assign_role(app: &mut App, owner: String, who: String) {
    let mut fields = vec![
        Field::pick("role", "Role", Source::Roles).required(),
        Field::pick("content_object", "Object", Source::RoleObjects).help("empty = model-level (all objects)"),
    ];
    if caps(app).is_some_and(|c| c.status.as_ref().is_some_and(|s| s.domain_enabled == Some(true))) {
        fields.push(Field::pick("domain", "Domain", Source::Domains).help("optional domain scope"));
    }
    let form = Form::new(format!("Assign role to {who}"), fields);
    app.form(form, move |app, f| {
        let role = f.get("role").to_string();
        let mut b = json!({ "role": role, "content_object": f.opt("content_object") });
        if let Some(d) = f.opt("domain") {
            b["domain"] = d.into();
        }
        let url = format!("{owner}roles/");
        let label = format!("assign {role} to {who}");
        let go = move |app: &mut App| submit(app, label, move |c| async move { Ok(c.post(&url, &b).await?) });
        if is_powerful(&role) {
            let scope = if f.opt("content_object").is_none() { "ALL objects of its type" } else { "the given object" };
            app.confirm(
                "Grant powerful role?",
                vec![
                    format!("{role} grants owner/admin level permissions on {scope}."),
                    "Check this is intended (privilege escalation).".into(),
                ],
                go,
            );
        } else {
            go(app);
        }
        Ok(())
    });
}

fn group_action(app: &mut App, act: Act, item: Value) {
    let href = s(&item, "pulp_href");
    let name = s(&item, "name");
    match act {
        Act::Members => app.push(Kind::GroupUsers { group: href, name }),
        Act::Roles => app.push(Kind::GroupRoles { group: href, name }),
        Act::Create => app.form(Form::new("New group", vec![Field::text("name", "Name").required()]), |app, f| {
            let b = json!({ "name": f.get("name") });
            submit(app, format!("create group {}", f.get("name")), move |c| async move { Ok(c.post(&c.api("groups/"), &b).await?) });
            Ok(())
        }),
        Act::Delete => app.confirm_danger(
            format!("Delete group {name}"),
            vec!["Members lose all permissions granted through this group.".into()],
            &name.clone(),
            move |app| submit(app, format!("delete group {name}"), move |c| async move { Ok(c.delete(&href).await?) }),
        ),
        _ => {}
    }
}

fn member_action(app: &mut App, act: Act, item: Value, group: String, name: String) {
    match act {
        Act::Create => app.form(
            Form::new(format!("Add member to {name}"), vec![Field::pick("username", "Username", Source::Users).required()]),
            move |app, f| {
                let (url, b) = (format!("{group}users/"), json!({ "username": f.get("username") }));
                submit(app, format!("add {} to {name}", f.get("username")), move |c| async move { Ok(c.post(&url, &b).await?) });
                Ok(())
            },
        ),
        Act::Delete => {
            let (href, user) = (s(&item, "pulp_href"), s(&item, "username"));
            let mut lines = vec![format!("Remove {user} from group {name}.")];
            if me(app).as_deref() == Some(user.as_str()) {
                lines.push("WARNING: this is YOUR account; you may lose permissions.".into());
            }
            app.confirm("Remove member", lines, move |app| {
                submit(app, format!("remove {user} from {name}"), move |c| async move { Ok(c.delete(&href).await?) })
            });
        }
        _ => {}
    }
}

fn role_action(app: &mut App, act: Act, item: Value) {
    let href = s(&item, "pulp_href");
    let name = s(&item, "name");
    let locked = item["locked"] == true;
    match act {
        Act::Create | Act::Edit => {
            if act == Act::Edit && locked {
                app.error("system-defined (locked) roles cannot be edited; create a custom role instead");
                return;
            }
            let edit = act == Act::Edit;
            let perms = item["permissions"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(","))
                .unwrap_or_default();
            let form = Form::new(
                if edit { format!("Edit role {name}") } else { "New role".into() },
                vec![
                    Field::text("name", "Name").required().value(name.clone()),
                    Field::text("description", "Description").value(s(&item, "description")),
                    Field::text("permissions", "Permissions (comma separated)")
                        .required()
                        .value(perms)
                        .help("e.g. rpm.view_rpmrepository,rpm.sync_rpmrepository"),
                ],
            );
            app.form(form, move |app, f| {
                let perms: Vec<&str> = f.get("permissions").split(',').map(str::trim).filter(|p| !p.is_empty()).collect();
                let b = json!({ "name": f.get("name"), "description": f.get("description"), "permissions": perms });
                let href = href.clone();
                submit(app, format!("{} role {}", if edit { "update" } else { "create" }, f.get("name")), move |c| async move {
                    Ok(if edit { c.patch(&href, &b).await? } else { c.post(&c.api("roles/"), &b).await? })
                });
                Ok(())
            });
        }
        Act::Delete => {
            if locked {
                app.error("system-defined (locked) roles cannot be deleted");
                return;
            }
            app.confirm_danger(
                format!("Delete role {name}"),
                vec!["All assignments of this role are removed.".into()],
                &name.clone(),
                move |app| submit(app, format!("delete role {name}"), move |c| async move { Ok(c.delete(&href).await?) }),
            );
        }
        _ => {}
    }
}

// ---- replication ------------------------------------------------------------------------

fn upstream_action(app: &mut App, act: Act, item: Value) {
    let href = s(&item, "pulp_href");
    let name = s(&item, "name");
    let caps = caps(app);
    let dst_origin = app.profile.as_ref().and_then(|p| app.cfg.profiles.get(p)).map(|p| p.origin()).unwrap_or_default();
    match act {
        Act::Create | Act::Edit => {
            let edit = act == Act::Edit;
            let v = if edit { item.clone() } else { Value::Null };
            let has = |f: &str| caps.as_ref().is_none_or(|c| c.has_field("UpstreamPulp", f));
            let mut fields = vec![
                Field::text("name", "Name").required().value(s(&v, "name")),
                Field::text("base_url", "Upstream base URL")
                    .required()
                    .value(s(&v, "base_url"))
                    .help("as reachable from THIS Pulp server, e.g. https://upstream.example.com"),
                Field::text("api_root", "Upstream API root")
                    .value(if edit { s(&v, "api_root") } else { "pulp/".into() })
                    .help("relative, with trailing slash: pulp/"),
                Field::text("domain", "Upstream domain").value(s(&v, "domain")),
                Field::text("username", "Username").help("write-only; empty = unchanged"),
                Field::new("password", "Password", FieldKind::Secret),
                Field::text("ca_cert", "CA cert (PEM file path)"),
                Field::text("client_cert", "Client cert (PEM file path)"),
                Field::text("client_key", "Client key (PEM file path)"),
                Field::new("tls_validation", "Validate TLS", FieldKind::Bool).value(if v["tls_validation"] == false {
                    "false"
                } else {
                    "true"
                }),
            ];
            if has("q_select") {
                fields.push(
                    Field::text("q_select", "Distribution filter (q)")
                        .value(s(&v, "q_select"))
                        .help(r#"e.g. name__startswith="rhel9" or pulp_label_select="env=prod""#),
                );
            } else if has("pulp_label_select") {
                fields.push(Field::text("pulp_label_select", "Label selector").value(s(&v, "pulp_label_select")));
            }
            if has("policy") {
                let mut policies = caps
                    .as_ref()
                    .and_then(|c| c.field_enum("UpstreamPulp", "policy").map(|e| e.to_vec()))
                    .unwrap_or_else(|| vec!["nodelete".into(), "labeled".into(), "all".into()]);
                policies.sort_by_key(|p| p != "nodelete"); // non-destructive default first
                fields.push(Field::new("policy", "Deletion policy", FieldKind::Choice(policies)).value(if edit {
                    s(&v, "policy")
                } else {
                    "nodelete".into()
                }));
            }
            let form = Form::new(
                if edit { format!("Edit upstream {name}") } else { "New Upstream Pulp (native replication, tech preview)".into() },
                fields,
            )
            .note("nodelete: never delete local objects. labeled: may delete objects created by earlier replications.")
            .note("all: replication manages ALL local objects of the domain and may DELETE unrelated local repositories.");
            app.form(form, move |app, f| {
                // Replication talks to the upstream API over HTTP.
                let origin = url_with_scheme(f.get("base_url"), &["http", "https"])?.origin().ascii_serialization();
                replication::guard_self(&origin, &dst_origin).map_err(|e| e.to_string())?;
                let mut b = json!({});
                let mut files = vec![];
                for fld in &f.fields {
                    let val = fld.value.trim().to_string();
                    let write_only = matches!(fld.key.as_str(), "username" | "password" | "ca_cert" | "client_cert" | "client_key");
                    if (write_only || !edit) && val.is_empty() || edit && !write_only && !fld.changed() {
                        continue;
                    }
                    match (fld.key.as_str(), &fld.kind) {
                        ("ca_cert" | "client_cert" | "client_key", _) => files.push((fld.key.clone(), val)),
                        (_, FieldKind::Bool) => b[fld.key.as_str()] = (val == "true").into(),
                        _ if val.is_empty() => b[fld.key.as_str()] = Value::Null,
                        _ => b[fld.key.as_str()] = val.into(),
                    }
                }
                let href = href.clone();
                let go = move |app: &mut App| {
                    submit(
                        app,
                        format!("{} upstream {}", if edit { "update" } else { "create" }, b["name"].as_str().unwrap_or("")),
                        move |c| async move {
                            for (k, path) in files {
                                b[k.as_str()] = tokio::fs::read_to_string(&path).await.with_context(|| format!("read {path}"))?.into();
                            }
                            Ok(if edit { c.patch(&href, &b).await? } else { c.post(&c.api("upstream-pulps/"), &b).await? })
                        },
                    )
                };
                if f.get("policy") == "all" {
                    app.confirm_danger(
                        "Destructive replication policy",
                        vec![
                            "Policy `all` lets replication DELETE local repositories, remotes and".into(),
                            "distributions of this domain that do not exist upstream.".into(),
                        ],
                        "all",
                        go,
                    );
                } else {
                    go(app);
                }
                Ok(())
            });
        }
        Act::Replicate => {
            // Also here: an Upstream Pulp made with another tool never passed the check of the form.
            let origin = url::Url::parse(&s(&item, "base_url")).map(|u| u.origin().ascii_serialization()).unwrap_or_default();
            if let Err(e) = replication::guard_self(&origin, &dst_origin) {
                return app.error(e.to_string());
            }
            let policy = s(&item, "policy");
            let mut lines = vec![
                format!("Replicate from {} into this server ({dst_origin}).", s(&item, "base_url")),
                format!(
                    "Distribution filter: {}",
                    if s(&item, "q_select").is_empty() { "ALL upstream distributions".into() } else { s(&item, "q_select") }
                ),
                format!("Deletion policy: {}", if policy.is_empty() { "server default".to_string() } else { policy.clone() }),
                "Pulp creates/updates repositories, remotes and distributions with the upstream names.".into(),
                "This is a technology preview of pulpcore; check the compatibility notes.".into(),
            ];
            let go = move |app: &mut App| {
                app.job(format!("replicate {name}"), move |ctx| async move {
                    let (p, task) = (ctx.clone(), ctx.on_task());
                    let on = move |g: &crate::pulp::models::TaskGroup| {
                        // The Task Center shows, stores and re-attaches the group, and reports how it ended.
                        task(&g.as_task());
                        p.progress(format!(
                            "tasks: {} done, {} running, {} waiting, {} failed",
                            g.completed, g.running, g.waiting, g.failed
                        ))
                    };
                    let res = replication::replicate_native(&ctx.client, &href, &on).await;
                    ap(move |app| match res {
                        Ok(lines) => app.text(format!("Replication of {name}"), lines),
                        Err(e) => app.text(format!("Replication of {name} FAILED"), format!("{e:#}").lines().map(str::to_string).collect()),
                    })
                });
            };
            if policy == "all" {
                lines.insert(0, "!!! policy `all` may DELETE local objects not present upstream !!!".into());
                app.confirm_danger(format!("Replicate {}", s(&item, "name")), lines, &s(&item, "name"), go);
            } else {
                app.confirm(format!("Replicate {}", s(&item, "name")), lines, go);
            }
        }
        Act::ReplicationHistory => app.job("replication history", move |ctx| async move {
            let c = &ctx.client;
            let want = format!("Replication of {name}");
            // task-groups/ has no filters (newest first): page until five match, at most 2000 groups.
            let (mut groups, mut seen) = (vec![], 0);
            let mut page = c.list::<Value>(&c.api("task-groups/"), &[("limit", "100".into())]).await?;
            loop {
                seen += page.results.len();
                groups.extend(page.results.into_iter().filter(|g| g["description"].as_str() == Some(want.as_str())));
                match page.next {
                    Some(next) if groups.len() < 5 && seen < 2000 => page = c.list(&next, &[]).await?,
                    _ => break,
                }
            }
            let mut lines = vec![];
            for g in groups.iter().take(5) {
                lines.push(format!(
                    "{}  completed {} failed {} canceled {}",
                    s(g, "pulp_href"),
                    g["completed"],
                    g["failed"],
                    g["canceled"]
                ));
                for t in g["tasks"].as_array().into_iter().flatten() {
                    lines.push(format!("    {:<10} {}", s(t, "state"), s(t, "name")));
                }
            }
            if lines.is_empty() {
                lines.push(format!("no replication task groups found for this upstream in the newest {seen} task groups"));
            }
            ap(move |app| app.text(format!("Replication history of {name}"), lines))
        }),
        Act::FallbackReplicate => fallback(app),
        Act::Delete => app.confirm_danger(
            format!("Delete upstream {name}"),
            vec!["Only the upstream definition is removed; replicated content stays.".into()],
            &name.clone(),
            move |app| submit(app, format!("delete upstream {name}"), move |c| async move { Ok(c.delete(&href).await?) }),
        ),
        _ => {}
    }
}

/// The types a "Types" choice of the fallback form selects. Deb has no fallback replication.
fn replication_types(choice: &str) -> Vec<RepoType> {
    match choice {
        "rpm" => vec![RepoType::Rpm],
        "file" => vec![RepoType::File],
        _ => vec![RepoType::Rpm, RepoType::File],
    }
}

/// Content synchronization from another configured profile when native replication is
/// unavailable or unsuitable.
pub fn fallback(app: &mut App) {
    let Some(dst) = app.profile.clone() else {
        app.error("connect to the destination server first");
        return;
    };
    let others: Vec<String> = app.cfg.profiles.keys().filter(|p| **p != dst).cloned().collect();
    if others.is_empty() {
        app.error("add the upstream server as another profile first (Servers)");
        return;
    }
    let form = Form::new(
        format!("Fallback replication into {dst}"),
        vec![
            Field::new("source", "Source profile", FieldKind::Choice(others)),
            Field::new("types", "Types", FieldKind::Choice(vec!["rpm+file".into(), "rpm".into(), "file".into()])),
            Field::text("filter", "Distribution name contains"),
            Field::new("mirror", "Mirror (new versions match upstream)", FieldKind::Bool).help("off = additive (non-destructive)"),
        ],
    )
    .note("Content synchronization only: creates remotes/repositories/distributions from published upstream content.")
    .note("It does NOT copy content guards or Pulp metadata and NEVER deletes destination objects.");
    app.form(form, move |app, f| {
        let src_name = f.get("source").to_string();
        let src_profile = app.cfg.profiles.get(&src_name).cloned().ok_or("unknown profile")?;
        let dst_profile = app.cfg.profiles.get(&dst).cloned().ok_or("unknown profile")?;
        replication::guard_self(&src_profile.origin(), &dst_profile.origin()).map_err(|e| e.to_string())?;
        let types = replication_types(f.get("types"));
        let (filter, mirror, settings) = (f.get("filter").to_string(), f.bool("mirror"), app.cfg.settings.clone());
        app.job(format!("plan replication from {src_name}"), move |ctx| async move {
            let src = PulpClient::new(&src_name, &src_profile, &settings)?;
            let base = ctx.caps.content_base();
            let plan = replication::plan(&src, &ctx.client, &types, &filter, base.as_deref()).await?;
            let mut lines: Vec<String> = vec![
                format!("Dry run: {src_name} -> {} ({})", ctx.client.profile(), if mirror { "mirror" } else { "additive" }),
                String::new(),
            ];
            lines.extend(plan.iter().map(ToString::to_string));
            let last = ctx.store.last_replications(ctx.client.profile(), &src_name).await?;
            if !last.is_empty() {
                lines.push(String::new());
                lines.push("Last successful runs:".into());
                lines.extend(last.iter().map(|(p, at)| format!("  /{p}  {at}")));
            }
            let n = plan.iter().filter(|p| p.action != PlanAction::Skip).count();
            ap(move |app| {
                if n == 0 {
                    app.text("Fallback replication: nothing to do", lines);
                    return;
                }
                app.confirm(format!("Replicate {n} distribution(s)?"), lines, move |app| {
                    app.job(format!("fallback replication from {src_name}"), move |ctx| async move {
                        let res = replication::run(&ctx.client, &ctx.store, &src_name, &plan, mirror, &ctx.log(), &ctx.on_task()).await?;
                        let failed = res.iter().filter(|l| l.starts_with("failed")).count();
                        ap(move |app| {
                            if failed > 0 {
                                app.notify(Level::Warn, format!("{failed} item(s) failed; rerun is safe (idempotent)"));
                            } else {
                                app.ok("fallback replication completed");
                            }
                            app.text("Fallback replication result", res);
                        })
                    });
                })
            })
        });
        Ok(())
    });
}

#[cfg(test)]
mod tests {
    use crate::pulp::RepoType;

    #[test]
    fn replication_types_stay_rpm_and_file() {
        assert_eq!(super::replication_types("rpm+file"), [RepoType::Rpm, RepoType::File]);
        assert_eq!(super::replication_types("rpm"), [RepoType::Rpm]);
        assert_eq!(super::replication_types("file"), [RepoType::File]);
    }

    #[tokio::test]
    async fn replicate_refuses_an_upstream_that_is_this_server() {
        use crate::config::{AuthMethod, Config, Profile};
        use crate::tui::app::App;
        let mut cfg = Config::default();
        cfg.profiles.insert("lab".into(), Profile { url: "https://pulp.example.com".into(), auth: AuthMethod::None, ..Default::default() });
        let local = std::sync::Arc::new(crate::store::Store::open_in_memory().await.unwrap());
        let mut app = App::new(cfg, "/dev/null".into(), local, Err(String::new()));
        app.profile = Some("lab".into());
        let item = serde_json::json!({ "pulp_href": "/u/1/", "name": "loop", "base_url": "https://pulp.example.com/" });
        super::upstream_action(&mut app, crate::tui::kinds::Act::Replicate, item);
        assert!(app.modal.is_none(), "no confirmation");
        assert!(app.notices.iter().any(|n| n.text.contains("refusing self-replication")));
    }

    use crate::tui::app::{App, Level, Modal, Msg};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use tokio::sync::mpsc::UnboundedReceiver;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const UP: &str = "/pulp/api/v3/upstream-pulps/1/";
    const GROUP: &str = "/pulp/api/v3/task-groups/1/";

    /// A replication group: `replicate_distributions` completed, one sync is in `state`.
    fn group(state: &str) -> Value {
        let mut g = json!({ "pulp_href": GROUP, "description": "Replication of up", "all_tasks_dispatched": true, "completed": 1,
            "tasks": [{ "name": "pulpcore.app.tasks.replica.replicate_distributions", "state": "completed" },
                      { "name": "pulp_rpm.app.tasks.synchronizing.synchronize", "state": state }] });
        g[state] = (g[state].as_u64().unwrap_or(0) + 1).into();
        g
    }

    async fn serve(server: &MockServer, verb: &str, p: &str, status: u16, body: Value) {
        Mock::given(method(verb)).and(path(p)).respond_with(ResponseTemplate::new(status).set_body_json(body)).mount(server).await;
    }

    /// An app on `store`, connected to `server` as profile `lab`, with its jobs done.
    async fn connected(server: &MockServer, store: Arc<crate::store::Store>) -> (App, UnboundedReceiver<Msg>) {
        use crate::config::{AuthMethod, Config, Profile};
        serve(server, "GET", "/pulp/api/v3/status/", 200, json!({ "versions": [] })).await;
        let mut cfg = Config::default();
        cfg.profiles.insert("lab".into(), Profile { url: server.uri(), auth: AuthMethod::None, ..Default::default() });
        let mut app = App::new(cfg, "/dev/null".into(), store.clone(), Ok(store));
        let mut rx = app.rx.take().unwrap();
        app.connect("lab");
        settle(&mut app, &mut rx).await;
        (app, rx)
    }

    async fn settle(app: &mut App, rx: &mut UnboundedReceiver<Msg>) {
        while !app.jobs.is_empty() {
            let m = rx.recv().await.unwrap();
            app.handle_msg(m);
        }
    }

    /// Confirm Replicate of upstream `up` (its POST answers with `GROUP`).
    async fn start_replication(server: &MockServer) -> (App, UnboundedReceiver<Msg>) {
        serve(server, "POST", &format!("{UP}replicate/"), 202, json!({ "task_group": GROUP })).await;
        let (mut app, rx) = connected(server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        let item = json!({ "pulp_href": UP, "name": "up", "base_url": "https://upstream.example.com/" });
        super::upstream_action(&mut app, crate::tui::kinds::Act::Replicate, item);
        let Some(Modal::Confirm { on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirmation") };
        yes(&mut app);
        (app, rx)
    }

    fn notices(app: &App) -> Vec<(Level, String)> {
        app.notices.iter().map(|n| (n.level, n.text.clone())).collect()
    }

    #[tokio::test]
    async fn a_replication_with_canceled_or_skipped_tasks_is_not_a_success() {
        for (state, counts) in [("canceled", "canceled 1"), ("skipped", "skipped 1")] {
            let server = MockServer::start().await;
            serve(&server, "GET", GROUP, 200, group(state)).await;
            let (mut app, mut rx) = start_replication(&server).await;
            settle(&mut app, &mut rx).await;
            let n = notices(&app);
            assert!(n.iter().any(|(l, t)| *l == Level::Warn && t.contains(counts)), "{state}: {n:?}");
            assert!(!n.iter().any(|(l, t)| *l == Level::Ok && t.contains("replicate up")), "{state}: {n:?}");
        }
        let server = MockServer::start().await;
        serve(&server, "GET", GROUP, 200, group("completed")).await;
        let (mut app, mut rx) = start_replication(&server).await;
        settle(&mut app, &mut rx).await;
        assert!(notices(&app).iter().any(|(l, t)| *l == Level::Ok && t.contains("completed")), "{:?}", notices(&app));
    }

    /// Apply job messages until `done` holds (at most 5 s).
    async fn until(app: &mut App, rx: &mut UnboundedReceiver<Msg>, done: impl Fn(&App) -> bool) {
        let wait = async {
            while !done(app) {
                let m = rx.recv().await.unwrap();
                app.handle_msg(m);
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), wait).await.expect("timed out");
    }

    #[tokio::test]
    async fn a_replication_is_tracked_and_reattached_after_a_restart() {
        let server = MockServer::start().await;
        serve(&server, "GET", GROUP, 200, group("running")).await;
        let (mut app, mut rx) = start_replication(&server).await;
        let stored =
            |app: &App| app.tasks.items.iter().any(|t| t.href == GROUP) && !app.jobs.iter().any(|j| j.label.starts_with("persist"));
        until(&mut app, &mut rx, stored).await;
        let row = &app.tasks.items[0];
        assert_eq!((row.label.as_str(), row.state.as_str()), ("replicate up", "running"), "visible in Tasks while it runs, as its job");
        let store = app.store.clone();
        let refs = store.tasks("lab", 10).await.unwrap();
        assert_eq!((refs.len(), refs[0].href.as_str(), refs[0].state.as_str()), (1, GROUP, "running"));
        drop((app, rx)); // quit while the replication runs

        // The next start re-attaches it; a canceled task ends it with a warning, and the list reloads.
        server.reset().await;
        serve(&server, "GET", GROUP, 200, group("canceled")).await;
        let (mut app, mut rx) = connected(&server, store.clone()).await;
        assert!(notices(&app).contains(&(Level::Info, "re-attached to 1 unfinished task(s) from a previous session".into())));
        app.open(crate::tui::app::Screen::List(crate::tui::kinds::Kind::UpstreamPulps));
        settle(&mut app, &mut rx).await;
        let generation = app.list.generation;
        app.tick();
        until(&mut app, &mut rx, |app| app.tasks.items[0].is_final()).await;
        assert!(app.list.generation > generation, "the list reloads");
        assert_eq!(app.tasks.items[0].state, "canceled");
        let n = notices(&app);
        assert!(n.iter().any(|(l, t)| *l == Level::Warn && t.contains("replicate up") && t.contains("canceled 1")), "{n:?}");
        settle(&mut app, &mut rx).await;
        assert_eq!(store.tasks("lab", 10).await.unwrap()[0].state, "canceled", "stored final: not re-attached again");
    }

    #[tokio::test]
    async fn upstream_base_url_needs_http_or_https() {
        let server = MockServer::start().await;
        let (mut app, _rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        super::upstream_action(&mut app, crate::tui::kinds::Act::Create, Value::Null);
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
        let mut set = |k: &str, v: &str| form.fields.iter_mut().find(|f| f.key == k).unwrap().value = v.into();
        set("name", "up");
        set("base_url", "foo:8000/x");
        assert_eq!(submit(&mut app, &form), Err("invalid URL: use http://, https:// (not foo:)".into()));
        assert!(app.jobs.is_empty(), "nothing sent");
        form.fields.iter_mut().find(|f| f.key == "base_url").unwrap().value = "https://upstream.example.com".into();
        assert_eq!(submit(&mut app, &form), Ok(()));
        assert_eq!(app.jobs.len(), 1, "the create is sent");
    }

    #[tokio::test]
    async fn replication_history_pages_through_task_groups_until_it_has_five() {
        use wiremock::matchers::query_param;
        const GROUPS: &str = "/pulp/api/v3/task-groups/";
        let server = MockServer::start().await;
        let g = |n: usize, d: &str| json!({ "pulp_href": format!("{GROUPS}{n}/"), "description": d, "completed": 1, "failed": 0, "canceled": 0, "tasks": [] });
        let page = |results: Vec<Value>, next: Option<u32>| {
            let next = next.map(|o| format!("{}{GROUPS}?limit=100&offset={o}", server.uri()));
            ResponseTemplate::new(200).set_body_json(json!({ "count": 300, "next": next, "results": results }))
        };
        // task-groups/ has no filters and lists newest first; a busy server has other groups on page 1.
        let at = |o: &str| Mock::given(method("GET")).and(path(GROUPS)).and(query_param("offset", o));
        at("100").respond_with(page((100..106).map(|n| g(n, "Replication of up")).collect(), Some(200))).mount(&server).await;
        at("200").respond_with(page(vec![g(200, "Replication of up")], None)).mount(&server).await;
        let first = (0..3).map(|n| g(n, "Replication of other")).collect();
        Mock::given(method("GET")).and(path(GROUPS)).respond_with(page(first, Some(100))).mount(&server).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        super::upstream_action(&mut app, crate::tui::kinds::Act::ReplicationHistory, json!({ "pulp_href": UP, "name": "up" }));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Text { lines, .. }) = &app.modal else { panic!("no history: {:?}", notices(&app)) };
        let rows: Vec<&String> = lines.iter().filter(|l| l.starts_with(GROUPS)).collect();
        assert_eq!(rows.len(), 5, "the five newest replications of up, from page 2: {lines:?}");
        assert!(rows[0].starts_with(&format!("{GROUPS}100/")), "{lines:?}");
        let reqs = server.received_requests().await.unwrap();
        assert!(!reqs.iter().any(|r| r.url.query().unwrap_or_default().contains("offset=200")), "five found: page 3 is not read");
    }

    #[tokio::test]
    async fn upstream_policy_choices_come_from_the_policy_property() {
        let server = MockServer::start().await;
        let (mut app, _rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        let mut caps = crate::pulp::Capabilities::default();
        // `Policy357Enum` today: the generated name can change between versions.
        caps.load_openapi(&json!({ "components": { "schemas": {
            "UpstreamPulp": { "properties": { "policy": { "allOf": [{ "$ref": "#/components/schemas/PolicyC4dEnum" }] } } },
            "PolicyC4dEnum": { "enum": ["all", "labeled", "nodelete", "keep"] }
        } } }));
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        super::upstream_action(&mut app, crate::tui::kinds::Act::Create, Value::Null);
        let Some(Modal::Form { form, .. }) = app.modal.take() else { panic!("no form") };
        let policy = form.fields.iter().find(|f| f.key == "policy").unwrap();
        assert_eq!(policy.kind, super::FieldKind::Choice(["nodelete", "all", "labeled", "keep"].map(String::from).to_vec()));
    }

    #[tokio::test]
    async fn assign_role_shows_the_domain_only_when_domains_are_enabled() {
        let server = MockServer::start().await;
        let (mut app, _rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        let form_keys = |app: &mut App| {
            super::role_assignment(app, crate::tui::kinds::Act::Create, Value::Null, "/u/1/".into(), "bob".into(), false);
            let Some(Modal::Form { form, .. }) = app.modal.take() else { panic!("no form") };
            form.fields.iter().map(|f| f.key.clone()).collect::<Vec<_>>()
        };
        assert_eq!(form_keys(&mut app), ["role", "content_object"], "status without domain_enabled");
        let conn = app.conn.as_mut().unwrap();
        let mut caps = (*conn.caps).clone();
        caps.status = Some(serde_json::from_value(json!({ "versions": [], "domain_enabled": true })).unwrap());
        conn.caps = Arc::new(caps);
        assert_eq!(form_keys(&mut app), ["role", "content_object", "domain"]);
    }

    #[tokio::test]
    async fn assign_and_remove_role_send_what_the_operator_confirmed() {
        use crate::tui::kinds::Act;
        use wiremock::matchers::body_json;
        let server = MockServer::start().await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        // Logged in as bob, domains enabled (set before each form: the jobs below may read the server again).
        let as_bob = |app: &mut App| {
            let conn = app.conn.as_mut().unwrap();
            let mut caps = (*conn.caps).clone();
            caps.status = Some(serde_json::from_value(json!({ "versions": [], "domain_enabled": true })).unwrap());
            caps.user = Some(crate::pulp::caps::CurrentUser { href: "/u/".into(), username: "bob".into(), is_staff: None });
            conn.caps = Arc::new(caps);
        };
        let (user, roles) = ("/pulp/api/v3/users/1/", "/pulp/api/v3/users/1/roles/");
        let created = ResponseTemplate::new(201).set_body_json(json!({ "pulp_href": format!("{roles}5/") }));
        let viewer = json!({ "role": "file.filerepository_viewer", "content_object": null, "domain": "/d/1/" });
        Mock::given(method("POST")).and(path(roles)).and(body_json(viewer)).respond_with(created.clone()).expect(1).mount(&server).await;
        let admin = json!({ "role": "core.task_admin", "content_object": null });
        Mock::given(method("POST")).and(path(roles)).and(body_json(admin)).respond_with(created).expect(1).mount(&server).await;
        Mock::given(method("DELETE"))
            .and(path(format!("{roles}5/")))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let assign = |app: &mut App, role: &str, domain: &str| {
            as_bob(app);
            super::role_assignment(app, Act::Create, Value::Null, user.into(), "bob".into(), false);
            let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
            for (k, v) in [("role", role), ("domain", domain)] {
                form.fields.iter_mut().find(|f| f.key == k).unwrap().value = v.into();
            }
            submit(app, &form).unwrap();
        };
        assign(&mut app, "file.filerepository_viewer", "/d/1/");
        assert!(app.modal.is_none(), "an ordinary role is sent at once");
        settle(&mut app, &mut rx).await;
        assign(&mut app, "core.task_admin", "");
        let Some(Modal::Confirm { title, lines, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirmation") };
        assert_eq!(title, "Grant powerful role?");
        assert_eq!(lines[0], "core.task_admin grants owner/admin level permissions on ALL objects of its type.");
        yes(&mut app);
        settle(&mut app, &mut rx).await;

        let item = json!({ "pulp_href": format!("{roles}5/"), "role": "core.task_admin" });
        as_bob(&mut app);
        super::role_assignment(&mut app, Act::Delete, item, user.into(), "bob".into(), false);
        let Some(Modal::Confirm { lines, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirmation") };
        assert_eq!(
            lines,
            ["Remove role core.task_admin from user bob.", "WARNING: this is YOUR account; you may lose permissions you need right now."]
        );
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        let errors: Vec<_> = notices(&app).into_iter().filter(|(l, _)| *l == Level::Error).collect();
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn powerful_roles() {
        assert!(super::is_powerful("rpm.rpmrepository_owner"));
        assert!(super::is_powerful("core.task_admin"));
        assert!(!super::is_powerful("rpm.rpmrepository_viewer"));
    }
}
