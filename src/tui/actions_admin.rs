//! Users, groups, roles (Pulp RBAC), replication, and export and import actions.

use super::actions::{ap, caps, submit, url_with_scheme};
use super::app::{App, Level};
use super::form::{Field, FieldKind, Form, Source, format_labels, parse_labels};
use super::kinds::{Act, Kind, s, short};
use crate::pulp::client::Page;
use crate::pulp::{PulpClient, RepoType, ops};
use crate::replication::{self, PlanAction};
use anyhow::{Context, bail};
use futures::future::try_join_all;
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
        Kind::PulpExporters | Kind::FsExporters => exporter_action(app, act, item),
        Kind::Exports { .. } => export_action(app, act, item),
        Kind::Importers => importer_action(app, act, item),
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
                let (href, b, label) = (href.clone(), json!({ "password": f.get("p1") }), format!("set password of {username}"));
                let go = move |app: &mut App| submit(app, label, move |c| async move { Ok(c.patch(&href, &b).await?) });
                if !is_me {
                    go(app);
                    return Ok(());
                }
                let lines = vec![
                    format!("{username} is the user of this session."),
                    "If this session signs in with this password (Basic auth), its credentials stop working after the change:".into(),
                    "every request, also each task poll, gets 401 (authentication failed).".into(),
                    "Then update the profile's secret (its env, file or keyring entry) and Connect again.".into(),
                ];
                app.confirm_danger(format!("Change YOUR OWN password ({username})"), lines, &username.clone(), go);
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
            let staff = item["is_staff"] == true;
            app.job(format!("impact of deleting user {username}"), move |ctx| async move {
                let c = &ctx.client;
                let user: Value = c.get(&href, &[]).await?;
                let roles: Page<Value> = c.list(&format!("{href}roles/"), &[("limit", "100".into())]).await?;
                let mut lines = vec![format!("Delete user {username}.")];
                lines.push(match user["groups"].as_array() {
                    Some(g) => counted(&format!("Member of {} group(s)", g.len()), g.iter().map(|g| s(g, "name"))),
                    None => "Group memberships: not shown by this server".into(),
                });
                lines.push(assignments(&roles, "user"));
                if staff {
                    lines.push("WARNING: this is a staff (administrator) account.".into());
                }
                lines.push("Prefer disabling (A) if the account may be needed again.".into());
                ap(move |app| {
                    app.confirm_danger(format!("Delete user {username}"), lines, &username.clone(), move |app| {
                        submit(app, format!("delete user {username}"), move |c| async move { Ok(c.delete(&href).await?) })
                    })
                })
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
        if f.opt("content_object").is_some() && f.opt("domain").is_some() {
            return Err("set Object or Domain, not both: Pulp refuses a role assignment with both".into());
        }
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
        Act::Create | Act::Edit => {
            let edit = act == Act::Edit;
            let title = if edit { format!("Rename group {name}") } else { "New group".into() };
            let field = Field::text("name", "Name").required().value(if edit { name.clone() } else { String::new() });
            app.form(Form::new(title, vec![field]), move |app, f| {
                let (href, new) = (href.clone(), f.get("name").to_string());
                let label = if edit { format!("rename group {name} to {new}") } else { format!("create group {new}") };
                let b = json!({ "name": new });
                submit(app, label, move |c| async move {
                    Ok(if edit { c.patch(&href, &b).await? } else { c.post(&c.api("groups/"), &b).await? })
                });
                Ok(())
            })
        }
        Act::Delete => app.job(format!("impact of deleting group {name}"), move |ctx| async move {
            let c = &ctx.client;
            let limit = [("limit", "100".to_string())];
            let members: Page<Value> = c.list(&format!("{href}users/"), &limit).await?;
            let roles: Page<Value> = c.list(&format!("{href}roles/"), &limit).await?;
            let lines = vec![
                format!("Delete group {name}."),
                counted(
                    &format!("{} member(s) lose the permissions of this group", members.count),
                    members.results.iter().map(|u| s(u, "username")),
                ),
                assignments(&roles, "group"),
            ];
            ap(move |app| {
                app.confirm_danger(format!("Delete group {name}"), lines, &name.clone(), move |app| {
                    submit(app, format!("delete group {name}"), move |c| async move { Ok(c.delete(&href).await?) })
                })
            })
        }),
        _ => {}
    }
}

/// `what`, then the names after a colon when there are any: "2 member(s): ann, bob".
fn counted(what: &str, names: impl Iterator<Item = String>) -> String {
    let names: Vec<String> = names.collect();
    if names.is_empty() { what.to_string() } else { format!("{what}: {}", names.join(", ")) }
}

/// The role assignments (a `roles/` page) that the delete of the `owner` (user or group) removes.
fn assignments(roles: &Page<Value>, owner: &str) -> String {
    let named = roles.results.iter().map(|r| match r["content_object"].as_str() {
        Some(o) => format!("{} on {}", s(r, "role"), short(o)),
        None => s(r, "role"),
    });
    counted(&format!("{} role assignment(s), removed with the {owner}", roles.count), named)
}

/// The names (`key`) of the users or groups (`collection`) that have `role`. Their lists have no role
/// filter: each one's `roles/` is read with `role=`.
async fn holders(c: &PulpClient, collection: &str, key: &str, role: &str) -> anyhow::Result<Vec<String>> {
    let all: Vec<Value> = c.list_all(&c.api(collection), &[("fields", format!("pulp_href,{key}"))], 100_000).await?;
    let q = [("role", role.to_string()), ("limit", "1".into()), ("fields", "pulp_href".into())];
    let has = try_join_all(all.iter().map(async |x| c.list::<Value>(&format!("{}roles/", s(x, "pulp_href")), &q).await)).await?;
    Ok(all.iter().zip(has).filter(|(_, p)| p.count > 0).map(|(x, _)| s(x, key)).collect())
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
            app.job(format!("impact of deleting role {name}"), move |ctx| async move {
                let c = &ctx.client;
                let users = holders(c, "users/", "username", &name).await?;
                let groups = holders(c, "groups/", "name", &name).await?;
                let lines = vec![
                    format!("Delete role {name}. All assignments of this role are removed."),
                    counted(&format!("{} user(s) have it", users.len()), users.into_iter()),
                    counted(&format!("{} group(s) have it", groups.len()), groups.into_iter()),
                ];
                ap(move |app| {
                    app.confirm_danger(format!("Delete role {name}"), lines, &name.clone(), move |app| {
                        submit(app, format!("delete role {name}"), move |c| async move { Ok(c.delete(&href).await?) })
                    })
                })
            });
        }
        _ => {}
    }
}

// ---- replication ------------------------------------------------------------------------

/// The warning for replication with deletion `policy`, None for `nodelete`. pulpcore's `remove_missing`
/// deletes for every other policy; empty (or no field, older servers) is its default, `all`.
fn deletion_warning(policy: &str) -> Option<String> {
    let what = match policy {
        "nodelete" => return None,
        "labeled" => {
            "the local repositories, remotes and distributions that earlier replications from this upstream created and that are gone upstream"
        }
        _ => "every local repository, remote and distribution of this domain that does not exist upstream",
    };
    let name = if policy.is_empty() { "empty (server default: `all`)".to_string() } else { format!("`{policy}`") };
    Some(format!("!!! policy {name} may DELETE {what} !!!"))
}

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
            // pulpcore copies these to the remotes that replication creates (`_build_remote_settings`).
            if has("remote_policy") {
                let mut p = caps
                    .as_ref()
                    .and_then(|c| c.field_enum("UpstreamPulp", "remote_policy").map(<[String]>::to_vec))
                    .unwrap_or_else(|| ["immediate", "on_demand", "streamed"].map(String::from).to_vec());
                p.insert(0, String::new());
                let help = "of the remotes that replication creates; empty = server default (immediate)";
                fields.push(
                    Field::new("remote_policy", "Replica download policy", FieldKind::Choice(p)).value(s(&v, "remote_policy")).help(help),
                );
            }
            let number = |k: &str, label: &str, value: String| {
                Field::new(k, label, FieldKind::Number).value(value).help("for the replica remotes; empty = server default")
            };
            for (k, label) in [("download_concurrency", "Download concurrency"), ("max_retries", "Max retries")] {
                fields.extend(has(k).then(|| number(k, label, s(&v, k))));
            }
            for (k, label) in [
                ("total_timeout", "Total timeout (s)"),
                ("connect_timeout", "Connect timeout (s)"),
                ("sock_connect_timeout", "Socket connect timeout (s)"),
                ("sock_read_timeout", "Socket read timeout (s)"),
            ] {
                // Floats on the server: `s()` would show `3600.0`, which is not a whole number.
                fields.extend(has(k).then(|| number(k, label, v[k].as_f64().map(|x| x.to_string()).unwrap_or_default())));
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
                        (_, FieldKind::Number) => {
                            b[fld.key.as_str()] = val.parse::<i64>().map_err(|_| format!("{} must be a number", fld.label))?.into()
                        }
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
                let policy = f.get("policy");
                match deletion_warning(policy) {
                    // An empty policy is no word to type: the Upstream name instead.
                    Some(w) => app.confirm_danger(
                        "Destructive replication policy",
                        vec![w],
                        if policy.is_empty() { f.get("name") } else { policy },
                        go,
                    ),
                    None => go(app),
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
            if let Some(w) = deletion_warning(&policy) {
                lines.insert(0, w);
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

// ---- export and import ---------------------------------------------------------------

fn exporter_action(app: &mut App, act: Act, item: Value) {
    let (href, name, path) = (s(&item, "pulp_href"), s(&item, "name"), s(&item, "path"));
    let fs = app.list.kind == Kind::FsExporters;
    match act {
        Act::Exports => app.push(Kind::Exports { exporter: href, name }),
        Act::Create | Act::Edit => exporter_form(app, fs, act == Act::Edit, &item),
        Act::Export if fs => fs_export(app, href, name, path),
        Act::Export => pulp_export(app, href, name, &item),
        Act::Delete => app.job(format!("impact of deleting exporter {name}"), move |ctx| async move {
            let n = ctx.client.list::<Value>(&format!("{href}exports/"), &[("limit", "1".into())]).await?.count;
            let lines = vec![
                format!("Delete exporter {name} and the records of its {n} export(s)."),
                format!("The export files in {path} on the Pulp server stay."),
            ];
            ap(move |app| {
                app.confirm_danger(format!("Delete exporter {name}"), lines, &name.clone(), move |app| {
                    submit(app, format!("delete exporter {name}"), move |c| async move { Ok(c.delete(&href).await?) })
                })
            })
        }),
        _ => {}
    }
}

/// A Pulp exporter has repositories; a filesystem exporter (`fs`) has a method instead.
fn exporter_form(app: &mut App, fs: bool, edit: bool, item: &Value) {
    let href = s(item, "pulp_href");
    let mut fields = vec![
        Field::text("name", "Name").required().value(s(item, "name")),
        Field::text("path", "Directory on the Pulp server").required().value(s(item, "path")).help("inside its ALLOWED_EXPORT_PATHS"),
    ];
    let caps = caps(app);
    if !fs {
        let repos: Vec<&str> = item["repositories"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let mut pick = Field::pick("repositories", "Repositories", Source::Repos(None)).multi().required().value(repos.join("\n"));
        if !repos.is_empty() {
            pick.shown = format!("{} selected", repos.len());
        }
        fields.push(pick);
    } else if caps.as_ref().is_none_or(|c| c.has_field("FilesystemExporter", "method")) {
        let methods = caps
            .as_ref()
            .and_then(|c| c.field_enum("FilesystemExporter", "method").map(<[String]>::to_vec))
            .unwrap_or_else(|| ["write", "hardlink", "symlink"].map(String::from).to_vec());
        let method = Field::new("method", "Method", FieldKind::Choice(methods)).help("hardlink and symlink need Pulp's file storage");
        fields.push(if edit { method.value(s(item, "method")) } else { method });
    }
    let (kind, collection, note) = match fs {
        false => ("Pulp", "exporters/core/pulp/", "An export writes a .tar file (or chunks) and a -toc.json file into this directory."),
        true => (
            "filesystem",
            "exporters/core/filesystem/",
            "An export makes this directory and writes the files of one publication or version into it.",
        ),
    };
    let title = if edit { format!("Edit exporter {}", s(item, "name")) } else { format!("New {kind} exporter") };
    app.form(Form::new(title, fields).note(note), move |app, f| {
        let mut b = json!({});
        for fld in f.fields.iter().filter(|x| !edit || x.changed()) {
            let k = fld.key.as_str();
            b[k] = match k {
                "repositories" => f.get(k).split('\n').filter(|v| !v.is_empty()).collect::<Vec<_>>().into(),
                _ => f.get(k).into(),
            };
        }
        let href = href.clone();
        submit(app, format!("{} exporter {}", if edit { "update" } else { "create" }, f.get("name")), move |c| async move {
            Ok(if edit { c.patch(&href, &b).await? } else { c.post(&c.api(collection), &b).await? })
        });
        Ok(())
    });
}

/// A repository of a Pulp exporter: its latest version, and its version in the exporter's last export.
struct ExportRepo {
    href: String,
    name: String,
    latest: String,
    last: Option<String>,
}

/// The form opens after the repositories and the last export of the exporter are loaded: its note
/// shows the start point of an incremental export, and it refuses what Pulp would refuse.
fn pulp_export(app: &mut App, href: String, name: String, item: &Value) {
    let hrefs: Vec<String> = item["repositories"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
    let (last, path) = (s(item, "last_export"), s(item, "path"));
    app.job(format!("load exporter {name}"), move |ctx| async move {
        let c = &ctx.client;
        let last = if last.is_empty() { None } else { Some(c.get::<Value>(&last, &[]).await?) };
        let exported: Vec<&str> =
            last.iter().flat_map(|e| e["exported_resources"].as_array().into_iter().flatten()).filter_map(Value::as_str).collect();
        let mut repos = vec![];
        for repo in hrefs {
            let r: Value = c.get(&repo, &[]).await?;
            let last = exported.iter().find(|v| ops::repo_of_version(v) == repo).map(|v| v.to_string());
            repos.push(ExportRepo { name: s(&r, "name"), latest: s(&r, "latest_version_href"), last, href: repo });
        }
        let since = last.map(|e| s(&e, "pulp_created"));
        ap(move |app| pulp_export_form(app, href, name, path, repos, since))
    });
}

/// Full or incremental export; the result names the TOC file to import.
fn pulp_export_form(app: &mut App, href: String, name: String, path: String, repos: Vec<ExportRepo>, since: Option<String>) {
    let has = |field: &str| caps(app).is_none_or(|c| c.has_field("PulpExport", field));
    let (incremental, versions) = (has("full"), has("versions"));
    let start_field = incremental && has("start_versions");
    let mut fields = vec![Field::text("chunk_size", "Chunk size").help("e.g. 1GB: files of at most this size; empty = one file")];
    if incremental {
        fields.push(Field::new("incremental", "Incremental", FieldKind::Bool).help("only the content added since the start versions"));
    }
    if versions {
        fields.push(Field::text("versions", "Versions").help("repository=number,...; a repository not named: its latest version"));
    }
    if start_field {
        fields.push(
            Field::text("start_versions", "Start versions").help("incremental: repository=number,...; not named: its last export version"),
        );
    }
    let mut form = Form::new(format!("Export {name}"), fields)
        .note(format!("Export of the {} repositories of {name} into {path} on the Pulp server:", repos.len()));
    // shortcut: the note lists 8 repositories at most, show a scrollable list when exporters get many.
    for r in repos.iter().take(8) {
        let last = match (&since, &r.last) {
            (None, _) => String::new(),
            (Some(_), Some(v)) => format!(", last export version {}", ops::version_number(v)),
            (Some(_), None) => ", not in the last export".into(),
        };
        form = form.note(format!("  {}: latest version {}{last}", r.name, ops::version_number(&r.latest)));
    }
    if repos.len() > 8 {
        form = form.note(format!("  ... and {} more", repos.len() - 8));
    }
    if incremental {
        form = form.note(match &since {
            Some(at) => format!("Last export: {at}. Incremental: only the content added since it."),
            None if start_field => "No previous export: an incremental export needs start versions.".into(),
            None => "No previous export: only a full export can start here.".into(),
        });
    }
    if versions {
        form = form.note("Versions: empty = the latest version of each repository.");
    }
    if start_field {
        form = form.note("Start versions: empty = the versions of the last export.");
    }
    app.form(form, move |app, f| {
        let mut b = json!({});
        if let Some(size) = f.opt("chunk_size") {
            b["chunk_size"] = size.into();
        }
        let chosen = chosen_versions(f.get("versions"), &name, &repos).map_err(|e| format!("Versions: {e}"))?;
        if chosen.iter().any(Option::is_some) {
            // Pulp needs exactly one version of each repository of the exporter (serializers/exporter.py).
            b["versions"] = repos.iter().zip(chosen).map(|(r, v)| v.unwrap_or_else(|| r.latest.clone())).collect();
        }
        let chosen = chosen_versions(f.get("start_versions"), &name, &repos).map_err(|e| format!("Start versions: {e}"))?;
        let some_chosen = chosen.iter().any(Option::is_some);
        if some_chosen && !f.bool("incremental") {
            return Err("Start versions: only for an incremental export".into());
        }
        if f.bool("incremental") {
            // Pulp answers 400 without a previous export or start versions (serializers/exporter.py validate),
            // and the task fails with a KeyError for a repository without a start version (tasks/export.py _version_match).
            let starts: Vec<Option<String>> = repos.iter().zip(chosen).map(|(r, v)| v.or_else(|| r.last.clone())).collect();
            if let Some((r, _)) = repos.iter().zip(&starts).find(|(_, v)| v.is_none()) {
                let why = match since {
                    None => format!("{name} has no previous export"),
                    Some(_) => format!("{} is not in the last export of {name}", r.name),
                };
                let give = if start_field { format!("give the start version of {} or ", r.name) } else { String::new() };
                return Err(format!("Incremental: {why}; {give}make a full export first"));
            }
            b["full"] = false.into();
            if some_chosen {
                // Pulp needs exactly one start version of each repository, like `versions`.
                b["start_versions"] = starts.into_iter().flatten().collect();
            }
        }
        let (href, name) = (href.clone(), name.clone());
        app.job(format!("export {name}"), move |ctx| async move {
            let c = &ctx.client;
            let export = ops::create(c, &format!("{href}exports/"), &b, "/exports/", &ctx.on_task()).await?;
            let v: Value = c.get(&export, &[]).await?;
            let mut lines = vec![format!("TOC: {}", s(&v["toc_info"], "file")), String::new(), "Files:".into()];
            lines.extend(v["output_file_info"].as_object().into_iter().flatten().map(|(f, _)| format!("  {f}")));
            lines.push(String::new());
            lines.push("Copy the files to the other server, then import the TOC there (Importers, I).".into());
            ap(move |app| app.text(format!("Export of {name}"), lines))
        });
        Ok(())
    });
}

/// `name=number,...` -> per repository of `exporter` (in its order) the href of the named version.
fn chosen_versions(text: &str, exporter: &str, repos: &[ExportRepo]) -> Result<Vec<Option<String>>, String> {
    let pairs = parse_labels(text)?;
    let pairs = pairs.as_object().cloned().unwrap_or_default();
    if let Some(k) = pairs.keys().find(|k| !repos.iter().any(|r| &&r.name == k)) {
        let names: Vec<&str> = repos.iter().map(|r| r.name.as_str()).collect();
        return Err(format!("{k} is not a repository of {exporter} ({})", names.join(", ")));
    }
    let version = |r: &ExportRepo, n: &str| match n.parse::<u64>() {
        Ok(n) => Ok(Some(format!("{}versions/{n}/", r.href))),
        Err(_) => Err(format!("the version of {} must be a number, not {n:?}", r.name)),
    };
    repos.iter().map(|r| pairs.get(&r.name).and_then(Value::as_str).map_or(Ok(None), |n| version(r, n))).collect()
}

/// Export the files of one publication (the newest of the version) or of one repository version.
fn fs_export(app: &mut App, href: String, name: String, path: String) {
    let what = vec!["publication".to_string(), "repository version".to_string()];
    let form = Form::new(
        format!("Export {name}"),
        vec![
            Field::pick("repo", "Repository", Source::Repos(None)).required(),
            Field::new("number", "Version number", FieldKind::Number).help("empty = latest"),
            Field::new("what", "Export", FieldKind::Choice(what)).help("publication: the newest publication of the version"),
        ],
    )
    .note(format!("Pulp makes {path} on the Pulp server and writes the files into it."))
    .note("It must not exist yet: for the next export, edit the exporter's directory first.");
    app.form(form, move |app, f| {
        let repo = f.get("repo").to_string();
        let t = RepoType::from_href(&repo).ok_or("pick an RPM, File or Deb repository")?;
        let (number, publication) = (f.number("number"), f.get("what") == "publication");
        let (href, name, path) = (href.clone(), name.clone(), path.clone());
        app.job(format!("export {name}"), move |ctx| async move {
            let c = &ctx.client;
            let v = match number {
                Some(n) => format!("{repo}versions/{n}/"),
                None => s(&c.get::<Value>(&repo, &[]).await?, "latest_version_href"),
            };
            let body = if publication {
                let q = [("repository_version", v.clone()), ("ordering", "-pulp_created".into()), ("limit", "1".into())];
                let newest = c.list::<Value>(&c.api(t.publications()), &q).await?.results.first().map(|p| s(p, "pulp_href"));
                let p = newest
                    .ok_or_else(|| anyhow::anyhow!("{} has no publication: publish it, or export the repository version", short(&v)))?;
                json!({ "publication": p })
            } else {
                json!({ "repository_version": v })
            };
            ops::create(c, &format!("{href}exports/"), &body, "/exports/", &ctx.on_task()).await?;
            let what = if publication { "publication" } else { "repository version" };
            ap(move |app| app.ok(format!("export {name}: {what} written to {path}")))
        });
        Ok(())
    });
}

/// An export record: Pulp deletes the record only, never the files.
fn export_action(app: &mut App, act: Act, item: Value) {
    if act != Act::Delete {
        return;
    }
    let href = s(&item, "pulp_href");
    let (id, toc) = (short(&href), s(&item["toc_info"], "file"));
    let toc = if toc.is_empty() { String::new() } else { format!(" (TOC {toc})") };
    let lines = vec![
        format!("Delete the record of export {id}{toc}."),
        "The export files stay on the Pulp server; delete them there when they are no longer needed.".into(),
    ];
    app.confirm_danger("Delete export", lines, &id.clone(), move |app| {
        submit(app, format!("delete export {id}"), move |c| async move { Ok(c.delete(&href).await?) })
    });
}

fn importer_action(app: &mut App, act: Act, item: Value) {
    let (href, name) = (s(&item, "pulp_href"), s(&item, "name"));
    match act {
        Act::Imports => app.push(Kind::Imports { importer: href, name }),
        Act::Create | Act::Edit => importer_form(app, act == Act::Edit, &item),
        Act::Import => import_form(app, href, name, item["repo_mapping"].clone()),
        Act::Delete => app.job(format!("impact of deleting importer {name}"), move |ctx| async move {
            let n = ctx.client.list::<Value>(&format!("{href}imports/"), &[("limit", "1".into())]).await?.count;
            let lines = vec![
                format!("Delete importer {name} and the records of its {n} import(s)."),
                "The imported repository versions stay.".into(),
            ];
            ap(move |app| {
                app.confirm_danger(format!("Delete importer {name}"), lines, &name.clone(), move |app| {
                    submit(app, format!("delete importer {name}"), move |c| async move { Ok(c.delete(&href).await?) })
                })
            })
        }),
        _ => {}
    }
}

fn importer_form(app: &mut App, edit: bool, item: &Value) {
    let href = s(item, "pulp_href");
    let fields = vec![
        Field::text("name", "Name").required().value(s(item, "name")),
        Field::text("repo_mapping", "Repository mapping")
            .value(format_labels(&item["repo_mapping"]))
            .help("source=destination,...: a repository of the export, then one of this server"),
    ];
    let title = if edit { format!("Edit importer {}", s(item, "name")) } else { "New Pulp importer".into() };
    let form = Form::new(title, fields)
        .note("Every destination repository must exist. A repository of the export without a pair goes")
        .note("to the repository of the same name; Pulp skips it when there is none.");
    app.form(form, move |app, f| {
        let mapping = parse_labels(f.get("repo_mapping")).map_err(|e| format!("Repository mapping: {e}"))?;
        let mut b = json!({});
        if !edit || f.changed("name") {
            b["name"] = f.get("name").into();
        }
        if !edit || f.changed("repo_mapping") {
            b["repo_mapping"] = mapping;
        }
        let dests: Vec<String> =
            b["repo_mapping"].as_object().into_iter().flatten().filter_map(|(_, d)| d.as_str().map(str::to_string)).collect();
        let href = href.clone();
        submit(app, format!("{} importer {}", if edit { "update" } else { "create" }, f.get("name")), move |c| async move {
            // pulpcore 3.123 answers 500 to a PATCH with an unknown destination, and has dropped the old mapping then.
            for d in &dests {
                if ops::find_by_name::<Value>(&c, &c.api("repositories/"), d).await?.is_none() {
                    bail!("no repository named {d}: every destination must exist");
                }
            }
            Ok(if edit { c.patch(&href, &b).await? } else { c.post(&c.api("importers/core/pulp/"), &b).await? })
        });
        Ok(())
    });
}

/// The import check is the preview; the import runs only after the confirmation.
fn import_form(app: &mut App, href: String, name: String, mapping: Value) {
    let toc = Field::text("toc", "TOC file on the Pulp server").required().help("the -toc.json of the export, inside ALLOWED_IMPORT_PATHS");
    let form = Form::new(format!("Import with {name}"), vec![toc])
        .note("Pulp checks the file and the mapping first; nothing is imported before you confirm.");
    app.form(form, move |app, f| {
        let toc = f.get("toc").to_string();
        let (href, name, mapping) = (href.clone(), name.clone(), mapping.clone());
        app.job(format!("import check {toc}"), move |ctx| async move {
            let (ok, lines) = ops::import_check(&ctx.client, &toc, &mapping).await?;
            ap(move |app| match ok {
                true => confirm_import(app, href, name, toc, &mapping, lines),
                false => app.text(format!("Import check of {toc} failed"), lines),
            })
        });
        Ok(())
    });
}

fn confirm_import(app: &mut App, href: String, name: String, toc: String, mapping: &Value, mut lines: Vec<String>) {
    lines.push(String::new());
    lines.push("Each repository of the export gets a new version in its destination repository:".into());
    lines.extend(mapping.as_object().into_iter().flatten().map(|(src, dst)| format!("  {src} -> {}", dst.as_str().unwrap_or_default())));
    lines.push("  any other: the repository of the same name (Pulp skips it when there is none)".into());
    lines.push("The new version holds exactly the exported content; older versions stay.".into());
    app.confirm_prod(format!("Import {toc}?"), lines, &name.clone(), move |app| {
        app.job(format!("import {name}"), move |ctx| async move {
            let (p, task) = (ctx.clone(), ctx.on_task());
            let on = move |g: &crate::pulp::models::TaskGroup| {
                // The Task Center shows, stores and re-attaches the group, and reports how it ended.
                task(&g.as_task());
                p.progress(format!("tasks: {} done, {} running, {} waiting, {} failed", g.completed, g.running, g.waiting, g.failed))
            };
            let (group, mut lines) = ops::pulp_import(&ctx.client, &href, &toc, &on).await?;
            let repos = group.tasks.iter().filter(|t| t["name"].as_str().is_some_and(|n| n.ends_with("import_repository_version"))).count();
            ap(move |app| {
                let done = group.outcome() == "completed";
                if done && repos == 0 {
                    lines.insert(0, "WARNING: no repository was imported: Pulp found no destination repository.".into());
                    app.notify(Level::Warn, format!("import {name}: no repository was imported"));
                }
                app.text(format!("Import of {toc}{}", if done { "" } else { " FAILED" }), lines)
            })
        })
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
    use crate::tui::kinds::Kind;
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

    /// pulpcore's `remove_missing` deletes for every policy but `nodelete`; empty is its default, `all`.
    /// (policy, word to type on the form save, what the warning says it deletes)
    const POLICIES: [(&str, Option<&str>, &str); 4] = [
        ("nodelete", None, ""),
        ("labeled", Some("labeled"), "that earlier replications from this upstream created and that are gone upstream"),
        ("all", Some("all"), "every local repository, remote and distribution of this domain that does not exist upstream"),
        ("", Some("up"), "(server default: `all`) may DELETE every local repository, remote and distribution of this domain"),
    ];

    #[tokio::test]
    async fn replicate_needs_a_typed_confirmation_for_every_policy_but_nodelete() {
        let server = MockServer::start().await;
        let (mut app, _rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        for (policy, typed, deletes) in POLICIES {
            let item = json!({ "pulp_href": UP, "name": "up", "base_url": "https://upstream.example.com/", "policy": policy });
            super::upstream_action(&mut app, crate::tui::kinds::Act::Replicate, item);
            let Some(Modal::Confirm { lines, typed: word, .. }) = app.modal.take() else { panic!("{policy:?}: no confirmation") };
            assert_eq!(word.as_deref(), typed.map(|_| "up"), "{policy:?}: Replicate types the upstream name");
            let warned = lines[0].starts_with("!!!") && lines[0].contains(deletes);
            assert_eq!(warned, typed.is_some(), "{policy:?}: {lines:?}");
        }
    }

    #[tokio::test]
    async fn upstream_save_needs_a_typed_confirmation_for_every_policy_but_nodelete() {
        let server = MockServer::start().await;
        let (mut app, _rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        for (policy, typed, deletes) in POLICIES {
            super::upstream_action(&mut app, crate::tui::kinds::Act::Create, Value::Null);
            let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
            for (k, v) in [("name", "up"), ("base_url", "https://upstream.example.com"), ("policy", policy)] {
                form.fields.iter_mut().find(|f| f.key == k).unwrap().value = v.into();
            }
            submit(&mut app, &form).unwrap();
            match (app.modal.take(), typed) {
                (None, None) => assert_eq!(app.jobs.len(), 1, "nodelete: saved at once"),
                (Some(Modal::Confirm { lines, typed: Some(word), .. }), Some(typed)) => {
                    assert_eq!(word, typed, "{policy:?}: form save");
                    assert!(lines.iter().any(|l| l.starts_with("!!!") && l.contains(deletes)), "{policy:?}: {lines:?}");
                }
                (m, _) => panic!("{policy:?}: form save shows {:?}", m.is_some()),
            }
        }
    }

    #[tokio::test]
    async fn upstream_download_settings_follow_the_schema_and_send_only_what_is_set() {
        let server = MockServer::start().await;
        serve(&server, "POST", "/pulp/api/v3/upstream-pulps/", 201, json!({ "pulp_href": UP })).await;
        serve(&server, "PATCH", UP, 200, json!({ "pulp_href": UP })).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        let new = ["download_concurrency", "max_retries", "total_timeout", "connect_timeout", "sock_connect_timeout", "sock_read_timeout"];
        let mut props: serde_json::Map<String, Value> = new.iter().map(|k| (k.to_string(), json!({}))).collect();
        props.insert("name".into(), json!({}));
        props.insert("policy".into(), json!({})); // nodelete: saved without a typed confirmation
        // The generated name (`RemotePolicyEnum` today) can change: the choices come through the property's `$ref`.
        props.insert("remote_policy".into(), json!({ "allOf": [{ "$ref": "#/components/schemas/RemotePolicy7Enum" }] }));
        let mut caps = crate::pulp::Capabilities::default();
        caps.load_openapi(&json!({ "components": { "schemas": {
            "UpstreamPulp": { "properties": props },
            "RemotePolicy7Enum": { "enum": ["immediate", "on_demand", "streamed", "later"] }
        } } }));
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        let form = |app: &mut App, act, item| {
            super::upstream_action(app, act, item);
            let Some(Modal::Form { form, submit }) = app.modal.take() else { panic!("no form") };
            (form, submit)
        };
        let set = |f: &mut super::Form, k: &str, v: &str| f.fields.iter_mut().find(|x| x.key == k).unwrap().value = v.into();
        let (mut f, mut submit) = form(&mut app, crate::tui::kinds::Act::Create, Value::Null);
        let keys: Vec<String> = f.fields.iter().map(|x| x.key.clone()).collect();
        assert_eq!(keys[keys.len() - 7..], [&["remote_policy"], &new[..]].concat());
        let policy = f.fields.iter().find(|x| x.key == "remote_policy").unwrap();
        assert_eq!(policy.kind, super::FieldKind::Choice(["", "immediate", "on_demand", "streamed", "later"].map(String::from).to_vec()));
        for (k, v) in [("name", "up"), ("base_url", "https://upstream.example.com"), ("remote_policy", "on_demand")] {
            set(&mut f, k, v);
        }
        set(&mut f, "download_concurrency", "x");
        assert_eq!(submit(&mut app, &f), Err("Download concurrency must be a number".into()));
        set(&mut f, "download_concurrency", "4");
        set(&mut f, "sock_read_timeout", "600");
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;

        let item = json!({ "pulp_href": UP, "name": "up", "base_url": "https://upstream.example.com", "api_root": "pulp/",
            "remote_policy": "streamed", "total_timeout": 3600.0, "max_retries": 5, "policy": "nodelete" });
        let (mut f, mut submit) = form(&mut app, crate::tui::kinds::Act::Edit, item);
        assert_eq!(["remote_policy", "total_timeout", "max_retries"].map(|k| f.get(k)), ["streamed", "3600", "5"], "as stored");
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        set(&mut f, "remote_policy", "");
        set(&mut f, "max_retries", "");
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;

        let sent = server.received_requests().await.unwrap();
        let bodies: Vec<Value> = sent.iter().filter(|r| r.method.as_str() != "GET").map(|r| r.body_json().unwrap()).collect();
        let picked = |b: &Value| {
            [&["remote_policy"], &new[..]].concat().into_iter().filter_map(|k| b.get(k).map(|v| (k, v.clone()))).collect::<Vec<_>>()
        };
        assert_eq!(
            picked(&bodies[0]),
            [("remote_policy", json!("on_demand")), ("download_concurrency", json!(4)), ("sock_read_timeout", json!(600))],
            "create: only what is filled"
        );
        assert_eq!(picked(&bodies[1]), [], "untouched edit");
        assert_eq!(picked(&bodies[2]), [("remote_policy", Value::Null), ("max_retries", Value::Null)], "cleared = server default");
        assert_eq!(bodies.len(), 3);
    }

    #[tokio::test]
    async fn upstream_without_download_settings_in_the_schema_shows_none() {
        let server = MockServer::start().await;
        let (mut app, _rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        let mut caps = crate::pulp::Capabilities::default();
        caps.schemas.insert("UpstreamPulp".into(), ["name", "base_url", "api_root"].map(String::from).into());
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        super::upstream_action(&mut app, crate::tui::kinds::Act::Create, Value::Null);
        let Some(Modal::Form { form, .. }) = app.modal.take() else { panic!("no form") };
        let shown = ["remote_policy", "download_concurrency", "max_retries", "total_timeout", "sock_read_timeout"];
        assert!(form.fields.iter().all(|f| !shown.contains(&f.key.as_str())));
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

    #[tokio::test]
    async fn group_edit_renames_the_group() {
        use crate::tui::kinds::{Act, Kind};
        use wiremock::matchers::body_json;
        let server = MockServer::start().await;
        let g = "/pulp/api/v3/groups/7/";
        Mock::given(method("PATCH"))
            .and(path(g))
            .and(body_json(json!({ "name": "ops-2" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "pulp_href": g, "name": "ops-2" })))
            .expect(1)
            .mount(&server)
            .await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        assert!(Kind::Groups.actions().contains(&Act::Edit));
        super::group_action(&mut app, Act::Edit, json!({ "pulp_href": g, "name": "ops" }));
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
        assert_eq!((form.title.as_str(), form.get("name")), ("Rename group ops", "ops"), "prefilled");
        form.fields[0].value = "ops-2".into();
        submit(&mut app, &form).unwrap();
        settle(&mut app, &mut rx).await;
        assert!(notices(&app).contains(&(Level::Ok, "rename group ops to ops-2: done".into())), "{:?}", notices(&app));
    }

    fn page(results: Value) -> Value {
        json!({ "count": results.as_array().unwrap().len(), "next": null, "results": results })
    }

    /// Connected as `root`; `run` opens a delete. No confirm before its impact job; then the confirm's lines and typed word.
    async fn delete_confirm(server: &MockServer, run: impl FnOnce(&mut App)) -> (Vec<String>, Option<String>) {
        let (mut app, mut rx) = connected(server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        let mut caps = (*app.conn.as_ref().unwrap().caps).clone();
        caps.user = Some(crate::pulp::caps::CurrentUser { href: "/u/0/".into(), username: "root".into(), is_staff: Some(true) });
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        run(&mut app);
        assert!(app.modal.is_none(), "the impact loads first");
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, typed, .. }) = app.modal.take() else { panic!("no confirm: {:?}", notices(&app)) };
        (lines, typed)
    }

    #[tokio::test]
    async fn user_delete_names_its_groups_and_role_assignments() {
        let server = MockServer::start().await;
        let u = "/pulp/api/v3/users/7/";
        serve(&server, "GET", u, 200, json!({ "pulp_href": u, "username": "ann", "groups": [{ "name": "ops" }, { "name": "dev" }] })).await;
        let roles = page(json!([{ "role": "core.task_viewer", "content_object": null },
            { "role": "file.filerepository_owner", "content_object": "/pulp/api/v3/repositories/file/file/0190abcd-1234-5678/" }]));
        serve(&server, "GET", &format!("{u}roles/"), 200, roles).await;
        let item = json!({ "pulp_href": u, "username": "ann", "is_staff": true });
        let (lines, typed) = delete_confirm(&server, |app| super::user_action(app, crate::tui::kinds::Act::Delete, item)).await;
        assert_eq!(typed.as_deref(), Some("ann"));
        assert_eq!(
            lines,
            [
                "Delete user ann.",
                "Member of 2 group(s): ops, dev",
                "2 role assignment(s), removed with the user: core.task_viewer, file.filerepository_owner on 0190abcd-1234",
                "WARNING: this is a staff (administrator) account.",
                "Prefer disabling (A) if the account may be needed again.",
            ]
        );
        // A server whose user has no `groups` field: the confirm does not claim "no group".
        serve(&server, "GET", "/pulp/api/v3/users/8/", 200, json!({ "username": "bob" })).await;
        serve(&server, "GET", "/pulp/api/v3/users/8/roles/", 200, page(json!([]))).await;
        let item = json!({ "pulp_href": "/pulp/api/v3/users/8/", "username": "bob" });
        let (lines, _) = delete_confirm(&server, |app| super::user_action(app, crate::tui::kinds::Act::Delete, item)).await;
        assert_eq!(lines[1..3], ["Group memberships: not shown by this server", "0 role assignment(s), removed with the user"]);
    }

    #[tokio::test]
    async fn group_delete_names_its_members_and_role_assignments() {
        let server = MockServer::start().await;
        let g = "/pulp/api/v3/groups/3/";
        serve(&server, "GET", &format!("{g}users/"), 200, page(json!([{ "username": "ann" }, { "username": "bob" }]))).await;
        serve(&server, "GET", &format!("{g}roles/"), 200, page(json!([{ "role": "rpm.rpmrepository_viewer", "content_object": null }])))
            .await;
        let item = json!({ "pulp_href": g, "name": "ops" });
        let (lines, typed) = delete_confirm(&server, |app| super::group_action(app, crate::tui::kinds::Act::Delete, item)).await;
        assert_eq!(typed.as_deref(), Some("ops"));
        assert_eq!(
            lines,
            [
                "Delete group ops.",
                "2 member(s) lose the permissions of this group: ann, bob",
                "1 role assignment(s), removed with the group: rpm.rpmrepository_viewer",
            ]
        );
    }

    /// `users/` and `groups/` have no role filter: each one's `roles/` is read with `role=`.
    #[tokio::test]
    async fn role_delete_names_the_users_and_groups_that_have_it() {
        use wiremock::matchers::query_param;
        let server = MockServer::start().await;
        let (u7, u8, g3) = ("/pulp/api/v3/users/7/", "/pulp/api/v3/users/8/", "/pulp/api/v3/groups/3/");
        serve(
            &server,
            "GET",
            "/pulp/api/v3/users/",
            200,
            page(json!([{ "pulp_href": u7, "username": "ann" }, { "pulp_href": u8, "username": "bob" }])),
        )
        .await;
        serve(&server, "GET", "/pulp/api/v3/groups/", 200, page(json!([{ "pulp_href": g3, "name": "ops" }]))).await;
        for (owner, has) in [(u7, true), (u8, false), (g3, true)] {
            let results = if has { json!([{ "role": "custom.viewer" }]) } else { json!([]) };
            Mock::given(method("GET"))
                .and(path(format!("{owner}roles/")))
                .and(query_param("role", "custom.viewer"))
                .respond_with(ResponseTemplate::new(200).set_body_json(page(results)))
                .mount(&server)
                .await;
        }
        let item = json!({ "pulp_href": "/pulp/api/v3/roles/9/", "name": "custom.viewer", "locked": false });
        let (lines, typed) = delete_confirm(&server, |app| super::role_action(app, crate::tui::kinds::Act::Delete, item)).await;
        assert_eq!(typed.as_deref(), Some("custom.viewer"));
        assert_eq!(
            lines,
            ["Delete role custom.viewer. All assignments of this role are removed.", "1 user(s) have it: ann", "1 group(s) have it: ops"]
        );
    }

    /// Pulp answers 400 to a role assignment with both `content_object` and `domain`.
    #[tokio::test]
    async fn assign_role_refuses_an_object_and_a_domain_together() {
        let server = MockServer::start().await;
        let (mut app, _rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        let mut caps = (*app.conn.as_ref().unwrap().caps).clone();
        caps.status = Some(serde_json::from_value(json!({ "versions": [], "domain_enabled": true })).unwrap());
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        let user = "/pulp/api/v3/users/1/".to_string();
        super::role_assignment(&mut app, crate::tui::kinds::Act::Create, Value::Null, user, "bob".into(), false);
        let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
        set(&mut form, "role", "file.filerepository_viewer");
        set(&mut form, "content_object", "/pulp/api/v3/repositories/file/file/1/");
        set(&mut form, "domain", "/pulp/api/v3/domains/1/");
        assert_eq!(submit(&mut app, &form), Err("set Object or Domain, not both: Pulp refuses a role assignment with both".into()));
        assert!(app.jobs.is_empty() && app.modal.is_none(), "nothing sent");
        set(&mut form, "domain", "");
        assert_eq!(submit(&mut app, &form), Ok(()));
        assert_eq!(app.jobs.len(), 1, "an object alone is sent");
    }

    #[tokio::test]
    async fn own_password_change_warns_and_needs_the_typed_user_name() {
        use crate::tui::kinds::Act;
        let server = MockServer::start().await;
        let (mut app, _rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        let mut caps = (*app.conn.as_ref().unwrap().caps).clone();
        caps.user = Some(crate::pulp::caps::CurrentUser { href: "/u/1/".into(), username: "bob".into(), is_staff: None });
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        let set_password = |app: &mut App, user: &str| {
            super::user_action(app, Act::Password, json!({ "pulp_href": format!("/pulp/api/v3/users/{user}/"), "username": user }));
            let Some(Modal::Form { mut form, mut submit }) = app.modal.take() else { panic!("no form") };
            set(&mut form, "p1", "n3w");
            set(&mut form, "p2", "n3w");
            submit(app, &form).unwrap();
        };
        set_password(&mut app, "bob");
        assert!(app.jobs.is_empty(), "nothing sent before the confirm");
        let Some(Modal::Confirm { lines, typed, .. }) = app.modal.take() else { panic!("no warning") };
        assert_eq!(typed.as_deref(), Some("bob"));
        assert!(lines.iter().any(|l| l.contains("401")) && lines.iter().any(|l| l.contains("profile's secret")), "{lines:?}");
        set_password(&mut app, "ann");
        assert!(app.modal.is_none(), "another user's password is sent at once");
        assert_eq!(app.jobs.len(), 1);
    }

    #[test]
    fn powerful_roles() {
        assert!(super::is_powerful("rpm.rpmrepository_owner"));
        assert!(super::is_powerful("core.task_admin"));
        assert!(!super::is_powerful("rpm.rpmrepository_viewer"));
    }

    const EXP: &str = "/pulp/api/v3/exporters/core/pulp/1/";

    /// The form that `act` on the current screen opens for `item`.
    fn open_form(app: &mut App, act: crate::tui::kinds::Act, item: Value) -> (super::Form, crate::tui::app::Submit) {
        super::run(app, act, item);
        let Some(Modal::Form { form, submit }) = app.modal.take() else { panic!("no form: {:?}", notices(app)) };
        (form, submit)
    }

    fn set(f: &mut super::Form, k: &str, v: &str) {
        f.fields.iter_mut().find(|x| x.key == k).unwrap().value = v.into();
    }

    /// Bodies of the requests that are not GET, with their method and path.
    async fn sent(server: &MockServer) -> Vec<(String, String, Value)> {
        let reqs = server.received_requests().await.unwrap();
        reqs.iter()
            .filter(|r| r.method.as_str() != "GET")
            .map(|r| (r.method.to_string(), r.url.path().to_string(), r.body_json().unwrap_or(Value::Null)))
            .collect()
    }

    #[tokio::test]
    async fn pulp_exporter_create_edit_and_delete_send_what_the_operator_set() {
        use crate::tui::kinds::Act;
        let server = MockServer::start().await;
        serve(&server, "POST", "/pulp/api/v3/exporters/core/pulp/", 201, json!({ "pulp_href": EXP })).await;
        serve(&server, "PATCH", EXP, 202, json!({ "task": "/pulp/api/v3/tasks/1/" })).await;
        serve(&server, "GET", &format!("{EXP}exports/"), 200, json!({ "count": 3, "next": null, "results": [] })).await;
        serve(&server, "DELETE", EXP, 202, json!({ "task": "/pulp/api/v3/tasks/2/" })).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        app.list.kind = Kind::PulpExporters;

        let (mut f, mut submit) = open_form(&mut app, Act::Create, Value::Null);
        assert_eq!(f.fields.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), ["name", "path", "repositories"]);
        for (k, v) in [("name", "dvd"), ("path", "/var/lib/pulp/exports/dvd"), ("repositories", "/r/1/\n/r/2/")] {
            set(&mut f, k, v);
        }
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;

        let item = json!({ "pulp_href": EXP, "name": "dvd", "path": "/var/lib/pulp/exports/dvd", "repositories": ["/r/1/", "/r/2/"] });
        let (mut f, mut submit) = open_form(&mut app, Act::Edit, item.clone());
        let repos = f.fields.iter().find(|x| x.key == "repositories").unwrap();
        assert_eq!((repos.value.as_str(), repos.shown.as_str()), ("/r/1/\n/r/2/", "2 selected"), "as stored");
        set(&mut f, "path", "/var/lib/pulp/exports/dvd2");
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;

        // The impact first, then the typed name.
        super::run(&mut app, Act::Delete, item);
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, typed: Some(typed), on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm") };
        assert_eq!(typed, "dvd");
        assert_eq!(
            lines,
            [
                "Delete exporter dvd and the records of its 3 export(s).",
                "The export files in /var/lib/pulp/exports/dvd on the Pulp server stay."
            ]
        );
        assert_eq!(sent(&server).await.len(), 2, "nothing deleted before the confirmation");
        yes(&mut app);
        settle(&mut app, &mut rx).await;

        let repos = json!(["/r/1/", "/r/2/"]);
        assert_eq!(
            sent(&server).await,
            [
                (
                    "POST".into(),
                    "/pulp/api/v3/exporters/core/pulp/".into(),
                    json!({ "name": "dvd", "path": "/var/lib/pulp/exports/dvd", "repositories": repos })
                ),
                ("PATCH".into(), EXP.into(), json!({ "path": "/var/lib/pulp/exports/dvd2" })),
                ("DELETE".into(), EXP.into(), Value::Null),
            ]
        );
        assert!(app.tasks.items.iter().any(|t| t.href == "/pulp/api/v3/tasks/2/"), "the delete task is tracked");
    }

    /// The Export form of a Pulp exporter: it opens after its repositories and last export are loaded.
    async fn open_export(app: &mut App, rx: &mut UnboundedReceiver<Msg>, item: Value) -> (super::Form, crate::tui::app::Submit) {
        (app.list.kind, app.modal) = (Kind::PulpExporters, None);
        super::run(app, crate::tui::kinds::Act::Export, item);
        settle(app, rx).await;
        let Some(Modal::Form { form, submit }) = app.modal.take() else { panic!("no form: {:?}", notices(app)) };
        (form, submit)
    }

    const R1: &str = "/pulp/api/v3/repositories/file/file/1/";
    const R2: &str = "/pulp/api/v3/repositories/file/file/2/";

    /// Repository `web` (R1, latest version 5) and `os` (R2, latest version 2).
    async fn export_repos(server: &MockServer) {
        serve(server, "GET", R1, 200, json!({ "name": "web", "latest_version_href": format!("{R1}versions/5/") })).await;
        serve(server, "GET", R2, 200, json!({ "name": "os", "latest_version_href": format!("{R2}versions/2/") })).await;
    }

    /// A server with `export_repos` and a last export `EXP/exports/8/` of web version 3, whose export POST completes as a task.
    async fn export_server() -> MockServer {
        let server = MockServer::start().await;
        export_repos(&server).await;
        let last = format!("{EXP}exports/8/");
        let exported =
            json!({ "pulp_href": last, "pulp_created": "2026-11-01T10:00:00Z", "exported_resources": [format!("{R1}versions/3/")] });
        serve(&server, "GET", &last, 200, exported).await;
        serve(&server, "POST", &format!("{EXP}exports/"), 202, json!({ "task": "/pulp/api/v3/tasks/7/" })).await;
        let done = json!({ "pulp_href": "/pulp/api/v3/tasks/7/", "state": "completed", "created_resources": [format!("{EXP}exports/9/")] });
        serve(&server, "GET", "/pulp/api/v3/tasks/7/", 200, done).await;
        serve(&server, "GET", &format!("{EXP}exports/9/"), 200, json!({ "toc_info": { "file": "t" } })).await;
        server
    }

    /// Exporter `dvd` of `repos`; `last`: with the last export of `export_server`.
    fn exporter(repos: &[&str], last: bool) -> Value {
        let last = if last { format!("{EXP}exports/8/").into() } else { Value::Null };
        json!({ "pulp_href": EXP, "name": "dvd", "path": "/x", "repositories": repos, "last_export": last })
    }

    #[tokio::test]
    async fn an_incremental_pulp_export_starts_at_the_last_export() {
        let server = export_server().await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;

        // No previous export: Pulp answers 400, so the form refuses it first.
        let (mut f, mut submit) = open_export(&mut app, &mut rx, exporter(&[R1], false)).await;
        assert_eq!(f.notes[1..3], ["  web: latest version 5", "No previous export: an incremental export needs start versions."]);
        set(&mut f, "incremental", "true");
        let e = submit(&mut app, &f).unwrap_err();
        assert_eq!(e, "Incremental: dvd has no previous export; give the start version of web or make a full export first");

        // A repository added after the last export: the task would fail late, so the form refuses it.
        let (mut f, mut submit) = open_export(&mut app, &mut rx, exporter(&[R1, R2], true)).await;
        assert_eq!(
            f.notes[1..4],
            [
                "  web: latest version 5, last export version 3",
                "  os: latest version 2, not in the last export",
                "Last export: 2026-11-01T10:00:00Z. Incremental: only the content added since it.",
            ]
        );
        set(&mut f, "incremental", "true");
        let e = submit(&mut app, &f).unwrap_err();
        assert_eq!(e, "Incremental: os is not in the last export of dvd; give the start version of os or make a full export first");
        assert!(sent(&server).await.is_empty(), "nothing sent");

        let (mut f, mut submit) = open_export(&mut app, &mut rx, exporter(&[R1], true)).await;
        set(&mut f, "incremental", "true");
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        assert_eq!(sent(&server).await, [("POST".into(), format!("{EXP}exports/"), json!({ "full": false }))]);
    }

    #[tokio::test]
    async fn an_incremental_pulp_export_can_start_at_chosen_versions() {
        let server = export_server().await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        let (mut f, mut submit) = open_export(&mut app, &mut rx, exporter(&[R1, R2], true)).await;
        assert!(f.notes.contains(&"Start versions: empty = the versions of the last export.".to_string()), "{:?}", f.notes);
        set(&mut f, "start_versions", "os=1");
        assert_eq!(submit(&mut app, &f).unwrap_err(), "Start versions: only for an incremental export");
        set(&mut f, "incremental", "true");
        set(&mut f, "start_versions", "os=one");
        assert_eq!(submit(&mut app, &f).unwrap_err(), "Start versions: the version of os must be a number, not \"one\"");
        assert!(sent(&server).await.is_empty(), "nothing sent");
        // Pulp needs one start version of each repository: web keeps the version of the last export.
        set(&mut f, "start_versions", "os=1");
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        // No previous export: every repository needs its start version.
        let (mut f, mut submit) = open_export(&mut app, &mut rx, exporter(&[R1], false)).await;
        set(&mut f, "incremental", "true");
        set(&mut f, "start_versions", "web=2");
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        let starts = json!([format!("{R1}versions/3/"), format!("{R2}versions/1/")]);
        let exports = format!("{EXP}exports/");
        assert_eq!(
            sent(&server).await,
            [
                ("POST".into(), exports.clone(), json!({ "full": false, "start_versions": starts })),
                ("POST".into(), exports, json!({ "full": false, "start_versions": [format!("{R1}versions/2/")] })),
            ]
        );
    }

    #[tokio::test]
    async fn a_pulp_export_of_chosen_versions_sends_one_version_per_repository() {
        let server = export_server().await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        let item = json!({ "pulp_href": EXP, "name": "dvd", "path": "/x", "repositories": [R1, R2] });
        let (mut f, mut submit) = open_export(&mut app, &mut rx, item).await;
        assert!(f.notes.contains(&"Versions: empty = the latest version of each repository.".to_string()), "{:?}", f.notes);
        for (text, error) in [
            ("web=x", "Versions: the version of web must be a number, not \"x\""),
            ("gone=1", "Versions: gone is not a repository of dvd (web, os)"),
            ("web", "Versions: label \"web\" must be key=value"),
        ] {
            set(&mut f, "versions", text);
            assert_eq!(submit(&mut app, &f).unwrap_err(), error);
        }
        assert!(sent(&server).await.is_empty(), "nothing sent");
        // Pulp needs one version of each repository of the exporter: the others are their latest.
        set(&mut f, "versions", " web = 3 ");
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        let versions = json!([format!("{R1}versions/3/"), format!("{R2}versions/2/")]);
        assert_eq!(sent(&server).await, [("POST".into(), format!("{EXP}exports/"), json!({ "versions": versions }))]);
    }

    #[tokio::test]
    async fn a_pulp_export_waits_for_its_task_and_shows_the_toc() {
        let (task, export) = ("/pulp/api/v3/tasks/7/", format!("{EXP}exports/9/"));
        let toc = "/var/lib/pulp/exports/dvd/export-9-20261102_1000-toc.json";
        let item = json!({ "pulp_href": EXP, "name": "dvd", "path": "/var/lib/pulp/exports/dvd", "repositories": [R1] });
        for state in ["completed", "failed"] {
            let server = MockServer::start().await;
            serve(&server, "POST", &format!("{EXP}exports/"), 202, json!({ "task": task })).await;
            let done = json!({ "pulp_href": task, "state": state, "created_resources": [export], "error": { "description": "disk full" } });
            serve(&server, "GET", task, 200, done).await;
            let files = json!({ "/var/lib/pulp/exports/dvd/export-9-20261102_1000.tar.0000": "ab", toc: "cd" });
            serve(&server, "GET", &export, 200, json!({ "pulp_href": export, "toc_info": { "file": toc }, "output_file_info": files }))
                .await;
            export_repos(&server).await;
            let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
            let (mut f, mut submit) = open_export(&mut app, &mut rx, item.clone()).await;
            assert_eq!(f.notes[0], "Export of the 1 repositories of dvd into /var/lib/pulp/exports/dvd on the Pulp server:");
            set(&mut f, "chunk_size", "1GB");
            submit(&mut app, &f).unwrap();
            settle(&mut app, &mut rx).await;
            assert_eq!(sent(&server).await, [("POST".into(), format!("{EXP}exports/"), json!({ "chunk_size": "1GB" }))]);
            let shown = match &app.modal {
                Some(Modal::Text { title, lines, .. }) => Some((title.clone(), lines.clone())),
                _ => None,
            };
            if state == "failed" {
                assert_eq!(shown, None, "no result for a failed task");
                assert!(notices(&app).iter().any(|(l, t)| *l == Level::Error && t.contains("disk full")), "{:?}", notices(&app));
                continue;
            }
            let (title, lines) = shown.expect("the result");
            assert_eq!(title, "Export of dvd");
            assert_eq!(lines[0], format!("TOC: {toc}"));
            assert!(lines.contains(&"  /var/lib/pulp/exports/dvd/export-9-20261102_1000.tar.0000".to_string()), "{lines:?}");
            assert!(app.tasks.items.iter().any(|t| t.href == task), "the export task is in the Task Center");
        }
    }

    #[tokio::test]
    async fn deleting_an_export_names_it_and_keeps_its_files() {
        use crate::tui::kinds::Act;
        let export = format!("{EXP}exports/0190abcd-1234-5678/");
        let server = MockServer::start().await;
        serve(&server, "DELETE", &export, 204, Value::Null).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        app.list.kind = Kind::Exports { exporter: EXP.into(), name: "dvd".into() };
        let item = json!({ "pulp_href": export, "toc_info": { "file": "/var/lib/pulp/exports/dvd/export-1-toc.json" } });
        super::run(&mut app, Act::Delete, item);
        let Some(Modal::Confirm { lines, typed: Some(typed), on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm") };
        assert_eq!(typed, "0190abcd-1234");
        assert_eq!(
            lines,
            [
                "Delete the record of export 0190abcd-1234 (TOC /var/lib/pulp/exports/dvd/export-1-toc.json).",
                "The export files stay on the Pulp server; delete them there when they are no longer needed."
            ]
        );
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        assert_eq!(sent(&server).await, [("DELETE".to_string(), export, Value::Null)]);
    }

    const FS: &str = "/pulp/api/v3/exporters/core/filesystem/1/";

    #[tokio::test]
    async fn a_filesystem_exporter_takes_the_methods_of_the_schema() {
        use crate::tui::kinds::Act;
        let server = MockServer::start().await;
        serve(&server, "POST", "/pulp/api/v3/exporters/core/filesystem/", 201, json!({ "pulp_href": FS })).await;
        serve(&server, "PATCH", FS, 202, json!({ "task": "/pulp/api/v3/tasks/1/" })).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        let mut caps = crate::pulp::Capabilities::default();
        // `MethodEnum` today: the generated name can change, the property's `$ref` does not.
        caps.load_openapi(&json!({ "components": { "schemas": {
            "FilesystemExporter": { "properties": { "name": {}, "path": {}, "method": { "allOf": [{ "$ref": "#/components/schemas/Method1Enum" }] } } },
            "Method1Enum": { "enum": ["write", "hardlink", "symlink", "reflink"] }
        } } }));
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        app.list.kind = Kind::FsExporters;

        let (mut f, mut submit) = open_form(&mut app, Act::Create, Value::Null);
        assert_eq!(f.fields.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), ["name", "path", "method"]);
        let method = f.fields.iter().find(|x| x.key == "method").unwrap();
        assert_eq!(method.kind, super::FieldKind::Choice(["write", "hardlink", "symlink", "reflink"].map(String::from).to_vec()));
        for (k, v) in [("name", "web"), ("path", "/var/lib/pulp/exports/web"), ("method", "hardlink")] {
            set(&mut f, k, v);
        }
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        let item = json!({ "pulp_href": FS, "name": "web", "path": "/var/lib/pulp/exports/web", "method": "hardlink" });
        let (mut f, mut submit) = open_form(&mut app, Act::Edit, item);
        assert_eq!(f.get("method"), "hardlink", "as stored");
        set(&mut f, "method", "symlink");
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        let body = json!({ "name": "web", "path": "/var/lib/pulp/exports/web", "method": "hardlink" });
        assert_eq!(
            sent(&server).await,
            [
                ("POST".into(), "/pulp/api/v3/exporters/core/filesystem/".into(), body),
                ("PATCH".into(), FS.into(), json!({ "method": "symlink" })),
            ]
        );
    }

    #[tokio::test]
    async fn a_filesystem_export_sends_the_publication_or_the_repository_version() {
        use crate::tui::kinds::Act;
        use wiremock::matchers::query_param;
        const REPO: &str = "/pulp/api/v3/repositories/file/file/1/";
        const PUBS: &str = "/pulp/api/v3/publications/file/file/";
        let (task, v3) = ("/pulp/api/v3/tasks/5/", format!("{REPO}versions/3/"));
        let server = MockServer::start().await;
        serve(&server, "GET", REPO, 200, json!({ "pulp_href": REPO, "latest_version_href": v3 })).await;
        let newest = json!({ "count": 2, "results": [{ "pulp_href": format!("{PUBS}9/") }] });
        let by_version = |v: &str| Mock::given(method("GET")).and(path(PUBS)).and(query_param("repository_version", v));
        by_version(&v3).respond_with(ResponseTemplate::new(200).set_body_json(newest)).mount(&server).await;
        let none = ResponseTemplate::new(200).set_body_json(json!({ "count": 0, "results": [] }));
        by_version(&format!("{REPO}versions/2/")).respond_with(none).mount(&server).await;
        serve(&server, "POST", &format!("{FS}exports/"), 202, json!({ "task": task })).await;
        let done = json!({ "pulp_href": task, "state": "completed", "created_resources": [format!("{FS}exports/1/")] });
        serve(&server, "GET", task, 200, done).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        app.list.kind = Kind::FsExporters;
        let item = json!({ "pulp_href": FS, "name": "web", "path": "/var/lib/pulp/exports/web", "method": "write" });
        let mut export = async |what: &str, number: &str| {
            let (mut f, mut submit) = open_form(&mut app, Act::Export, item.clone());
            for (k, v) in [("repo", REPO), ("number", number), ("what", what)] {
                set(&mut f, k, v);
            }
            submit(&mut app, &f).unwrap();
            settle(&mut app, &mut rx).await;
            notices(&app).pop().unwrap()
        };
        assert_eq!(export("publication", "").await, (Level::Ok, "export web: publication written to /var/lib/pulp/exports/web".into()));
        let (level, text) = export("publication", "2").await;
        assert!(level == Level::Error && text.contains("1@v2 has no publication"), "{text}");
        assert_eq!(export("repository version", "2").await.0, Level::Ok);
        let posts: Vec<Value> = sent(&server).await.into_iter().map(|(_, _, b)| b).collect();
        assert_eq!(
            posts,
            [json!({ "publication": format!("{PUBS}9/") }), json!({ "repository_version": format!("{REPO}versions/2/") })],
            "the newest publication of the latest version; nothing for a version without one"
        );
    }

    const IMP: &str = "/pulp/api/v3/importers/core/pulp/1/";

    /// `GET repositories/?name=` answers each of `names` with one repository, and `gone` with none.
    async fn repositories_named(server: &MockServer, names: &[&str]) {
        use wiremock::matchers::query_param;
        let named = |n: &str| Mock::given(method("GET")).and(path("/pulp/api/v3/repositories/")).and(query_param("name", n));
        for n in names {
            let one = json!({ "count": 1, "results": [{ "name": n, "pulp_href": format!("/r/{n}/") }] });
            named(n).respond_with(ResponseTemplate::new(200).set_body_json(one)).mount(server).await;
        }
        let none = ResponseTemplate::new(200).set_body_json(json!({ "count": 0, "results": [] }));
        named("gone").respond_with(none).mount(server).await;
    }

    #[tokio::test]
    async fn a_pulp_importer_maps_sources_to_destinations_that_exist() {
        use crate::tui::kinds::Act;
        let server = MockServer::start().await;
        serve(&server, "POST", "/pulp/api/v3/importers/core/pulp/", 201, json!({ "pulp_href": IMP })).await;
        serve(&server, "PATCH", IMP, 200, json!({ "pulp_href": IMP })).await;
        serve(&server, "GET", &format!("{IMP}imports/"), 200, json!({ "count": 2, "next": null, "results": [] })).await;
        serve(&server, "DELETE", IMP, 204, Value::Null).await;
        repositories_named(&server, &["web-copy", "os"]).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        app.list.kind = Kind::Importers;

        let (mut f, mut submit) = open_form(&mut app, Act::Create, Value::Null);
        assert_eq!(f.fields.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), ["name", "repo_mapping"]);
        set(&mut f, "name", "dvd");
        set(&mut f, "repo_mapping", "web");
        assert_eq!(submit(&mut app, &f), Err("Repository mapping: label \"web\" must be key=value".into()));
        set(&mut f, "repo_mapping", "web = web-copy, os=os");
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;

        let item = json!({ "pulp_href": IMP, "name": "dvd", "repo_mapping": { "os": "os", "web": "web-copy" } });
        let (mut f, mut submit) = open_form(&mut app, Act::Edit, item.clone());
        assert_eq!(f.get("repo_mapping"), "os=os,web=web-copy", "as stored");
        // pulpcore answers 500 to a PATCH with an unknown destination and has then dropped the old mapping.
        set(&mut f, "repo_mapping", "os=os,web=gone");
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        let n = notices(&app);
        assert!(n.iter().any(|(l, t)| *l == Level::Error && t.contains("no repository named gone")), "{n:?}");
        set(&mut f, "repo_mapping", "os=os");
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;

        super::run(&mut app, Act::Delete, item);
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, typed: Some(typed), on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm") };
        assert_eq!(typed, "dvd");
        assert_eq!(lines, ["Delete importer dvd and the records of its 2 import(s).", "The imported repository versions stay."]);
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        let mapping = json!({ "web": "web-copy", "os": "os" });
        assert_eq!(
            sent(&server).await,
            [
                ("POST".into(), "/pulp/api/v3/importers/core/pulp/".into(), json!({ "name": "dvd", "repo_mapping": mapping })),
                ("PATCH".into(), IMP.into(), json!({ "repo_mapping": { "os": "os" } })),
                ("DELETE".into(), IMP.into(), Value::Null),
            ],
            "nothing sent for the unknown destination"
        );
    }

    #[tokio::test]
    async fn an_import_is_checked_first_and_runs_as_a_task_group_after_the_confirmation() {
        use crate::tui::kinds::Act;
        const GROUP: &str = "/pulp/api/v3/task-groups/3/";
        let toc = "/var/lib/pulp/exports/dvd/export-9-20261102_1000-toc.json";
        let item = json!({ "pulp_href": IMP, "name": "dvd", "repo_mapping": { "web": "web-copy" } });
        let check = |valid: bool, messages: Value| {
            json!({ "toc": { "context": toc, "is_valid": valid, "messages": messages },
                "repo_mapping": { "context": "{\"web\":\"web-copy\"}", "is_valid": true, "messages": [] } })
        };
        let server = MockServer::start().await;
        let check_path = "/pulp/api/v3/importers/core/pulp/import-check/";
        serve(&server, "POST", check_path, 200, check(false, json!([format!("file {toc} does not exist")]))).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        app.list.kind = Kind::Importers;
        let (mut f, mut submit) = open_form(&mut app, Act::Import, item.clone());
        set(&mut f, "toc", toc);
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        let Some(Modal::Text { title, lines, .. }) = app.modal.take() else { panic!("no check result: {:?}", notices(&app)) };
        assert_eq!(title, format!("Import check of {toc} failed"));
        assert_eq!(lines[..2], ["toc: NOT valid".to_string(), format!("  file {toc} does not exist")]);
        assert_eq!(sent(&server).await.len(), 1, "no import without a valid check");

        server.reset().await;
        serve(&server, "POST", check_path, 200, check(true, json!([]))).await;
        serve(&server, "POST", &format!("{IMP}imports/"), 202, json!({ "task_group": GROUP })).await;
        let group = json!({ "pulp_href": GROUP, "description": format!("Import of {toc}"), "all_tasks_dispatched": false, "completed": 2,
            "tasks": [{ "name": "pulpcore.app.tasks.importer.pulp_import", "state": "completed", "pulp_href": "/t/1/" },
                      { "name": "pulpcore.app.tasks.importer.import_repository_version", "state": "completed", "pulp_href": "/t/2/" }] });
        serve(&server, "GET", GROUP, 200, group).await;
        let (mut f, mut submit) = open_form(&mut app, Act::Import, item);
        set(&mut f, "toc", toc);
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm: {:?}", notices(&app)) };
        assert_eq!(lines[..2], ["toc: valid", "repo_mapping: valid"]);
        assert!(lines.contains(&"  web -> web-copy".to_string()), "{lines:?}");
        assert_eq!(sent(&server).await.len(), 1, "only the check before the confirmation");
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        let Some(Modal::Text { title, lines, .. }) = app.modal.take() else { panic!("no result: {:?}", notices(&app)) };
        assert_eq!(title, format!("Import of {toc}"));
        assert!(lines.iter().any(|l| l.contains("completed  import_repository_version")), "{lines:?}");
        assert_eq!(
            sent(&server).await.into_iter().map(|(_, p, b)| (p, b)).collect::<Vec<_>>(),
            [
                (check_path.to_string(), json!({ "toc": toc, "repo_mapping": "{\"web\":\"web-copy\"}" })),
                (format!("{IMP}imports/"), json!({ "toc": toc }))
            ]
        );
        let row = app.tasks.items.iter().find(|t| t.href == GROUP).expect("the group is in the Task Center");
        assert_eq!((row.label.as_str(), row.state.as_str()), ("import dvd", "completed"), "final without all_tasks_dispatched");
    }
}
