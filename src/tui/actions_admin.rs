//! Users, groups, roles (Pulp RBAC), replication, and export and import actions.

use super::actions::{ap, caps, group_progress, snapshot_refusal, submit, unmark_when_deleted, url_with_scheme};
use super::app::{App, Level};
use super::form::{Field, FieldKind, Form, Source, format_labels, parse_labels};
use super::kinds::{Act, Kind, guard_type, s, short};
use crate::pulp::client::Page;
use crate::pulp::{Capabilities, PulpClient, PulpError, RepoType, ops};
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
        Kind::ContentGuards => guard_action(app, act, item),
        Kind::Uploads if act == Act::Delete => delete_uploads(app, item),
        Kind::Domains => domain_action(app, act, item),
        Kind::AccessPolicies if act == Act::ResetPolicy => reset_policy(app, item),
        // W: the last task of the schedule, not the schedule.
        Kind::TaskSchedules if act == Act::WatchTask => match s(&item, "last_task") {
            t if t.is_empty() => app.error(format!("schedule {} has not run a task yet", s(&item, "name"))),
            t => app.watch_task(&t, &s(&item, "name"), None),
        },
        Kind::UpstreamPulps => upstream_action(app, act, item),
        Kind::PulpExporters | Kind::FsExporters => exporter_action(app, act, item),
        Kind::Exports { .. } => export_action(app, act, item),
        Kind::Importers => importer_action(app, act, item),
        _ => {}
    }
}

/// The user of this session: `caps.user`, else the Basic-auth name (when `users/?username=` gave nothing).
/// None with bearer, certificate or no auth on a server without `login/`.
fn me(app: &App) -> Option<String> {
    let c = app.conn.as_ref()?;
    c.caps.user.as_ref().map(|u| u.username.clone()).or_else(|| c.client.username().map(String::from))
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
            let (staff, unknown) = (item["is_staff"] == true, me(app).is_none());
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
                if unknown {
                    lines.push(
                        "WARNING: cannot tell whether this is your own user (the server does not name the user of this session).".into(),
                    );
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
            // The row's pulp_href is the user itself; a DELETE to it deletes the account.
            let id = s(&item, "pulp_href").trim_end_matches('/').rsplit('/').next().unwrap_or_default().to_string();
            let (href, user) = (format!("{group}users/{id}/"), s(&item, "username"));
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

/// Create starts from the selected role's permissions only: its name would be a duplicate (400).
fn role_form(app: &mut App, edit: bool, item: &Value) {
    let href = s(item, "pulp_href");
    let perms =
        item["permissions"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(",")).unwrap_or_default();
    let own = |k: &str| if edit { s(item, k) } else { String::new() };
    let form = Form::new(
        if edit { format!("Edit role {}", s(item, "name")) } else { "New role".into() },
        vec![
            Field::text("name", "Name").required().value(own("name")),
            Field::text("description", "Description").value(own("description")),
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

fn role_action(app: &mut App, act: Act, item: Value) {
    let href = s(&item, "pulp_href");
    let name = s(&item, "name");
    let locked = item["locked"] == true;
    match act {
        Act::Edit if locked => app.error("system-defined (locked) roles cannot be edited; create a custom role instead"),
        Act::Create | Act::Edit => role_form(app, act == Act::Edit, &item),
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

/// Who holds which role on one object (`{href}list_roles/`) and what the current user may do with it
/// (`my_permissions/`); then add or remove a role for users and groups (`add_role/`, `remove_role/`).
pub fn object_roles(app: &mut App, item: Value) {
    let href = s(&item, "pulp_href");
    let name = Some(s(&item, "name")).filter(|n| !n.is_empty()).unwrap_or_else(|| short(&href));
    if caps(app).is_some_and(|c| !c.has_object_roles(&href)) {
        return app.error(format!("not supported by this server: object roles of {}/", crate::pulp::caps::collection(&href)));
    }
    let me = me(app);
    app.job(format!("read roles of {name}"), move |ctx| async move {
        let c = &ctx.client;
        let (roles, mine): (Value, Value) =
            (c.get(&format!("{href}list_roles/"), &[]).await?, c.get(&format!("{href}my_permissions/"), &[]).await?);
        let names = |v: &Value| {
            let n: Vec<&str> = v.as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            if n.is_empty() { "none".to_string() } else { n.join(", ") }
        };
        let rows = roles["roles"].as_array().into_iter().flatten();
        let mut notes: Vec<String> =
            rows.map(|r| format!("{}: users {}; groups {}", s(r, "role"), names(&r["users"]), names(&r["groups"]))).collect();
        if notes.is_empty() {
            notes.push("No role on this object.".into());
        }
        notes.push(format!("My permissions: {}", names(&mine["permissions"])));
        ap(move |app| {
            // Viewing is read-only; adding and removing is not.
            if app.read_only() {
                return app
                    .text(format!("Roles on {name}"), [notes, vec!["Read-only mode: no role is added or removed here.".into()]].concat());
            }
            let fields = vec![
                Field::new("change", "Change", FieldKind::Choice(vec!["add".into(), "remove".into()])),
                Field::pick("role", "Role", Source::Roles).required().help("Pulp refuses a role without a permission for this object"),
                Field::pick("users", "Users", Source::Users).multi(),
                Field::pick("groups", "Groups", Source::Groups).multi(),
            ];
            let mut form = Form::new(format!("Roles on {name}"), fields);
            form.notes = notes;
            app.form(form, move |app, f| {
                let list = |k: &str| f.get(k).split('\n').filter(|x| !x.is_empty()).map(String::from).collect::<Vec<_>>();
                let (role, users, groups) = (f.get("role").to_string(), list("users"), list("groups"));
                if users.is_empty() && groups.is_empty() {
                    return Err("pick a user or a group".into());
                }
                let add = f.get("change") == "add";
                let mut warn = vec![];
                if add && is_powerful(&role) {
                    warn.push(format!("{role} grants owner/admin level permissions on {name}."));
                }
                if let Some(m) = me.as_ref().filter(|m| !add && users.contains(m)) {
                    warn.push(format!("You remove a role from YOUR OWN account ({m}): you may lose access to {name}."));
                }
                let url = format!("{href}{}", if add { "add_role/" } else { "remove_role/" });
                let (label, b) =
                    (format!("{} role {role} on {name}", f.get("change")), json!({ "role": role, "users": users, "groups": groups }));
                let go = move |app: &mut App| submit(app, label, move |c| async move { Ok(c.post(&url, &b).await?) });
                if warn.is_empty() {
                    go(app)
                } else {
                    app.confirm("Confirm role change", warn, go)
                }
                Ok(())
            });
        })
    });
}

// ---- content guards ---------------------------------------------------------------------

/// Content guard types: the collection below `contentguards/`, the request schema, the fields beyond name and description.
const GUARD_TYPES: [(&str, &str, &[&str]); 7] = [
    ("core/rbac", "RBACContentGuard", &[]),
    ("core/header", "HeaderContentGuard", &["header_name", "header_value", "jq_filter"]),
    ("core/envvar_header", "EnvVarHeaderContentGuard", &["header_name", "env_var"]),
    ("core/content_redirect", "ContentRedirectContentGuard", &[]),
    ("core/composite", "CompositeContentGuard", &["guards"]),
    ("certguard/x509", "certguard.X509CertGuard", &["ca_certificate"]),
    ("certguard/rhsm", "certguard.RHSMCertGuard", &["ca_certificate"]),
];

/// The locked role with `download_rbaccontentguard`, which an RBAC guard checks.
const GUARD_DOWNLOADER: &str = "core.rbaccontentguard_downloader";

fn guard_action(app: &mut App, act: Act, item: Value) {
    let (href, name) = (s(&item, "pulp_href"), s(&item, "name"));
    match act {
        Act::Create => {
            let caps = caps(app);
            let has = |t: &String| caps.as_ref().is_none_or(|c| c.has_endpoint(&format!("contentguards/{t}/")));
            let types: Vec<String> = GUARD_TYPES.iter().map(|(t, ..)| t.to_string()).filter(has).collect();
            let form = Form::new("New content guard", vec![Field::new("type", "Type", FieldKind::Choice(types))]);
            app.form(form, |app, f| {
                guard_form(app, f.get("type").to_string(), Value::Null);
                Ok(())
            });
        }
        // The list has only name and description: the fields of the type come with the guard itself.
        Act::Edit => app.job(format!("read content guard {name}"), move |ctx| async move {
            let v: Value = ctx.client.get(&href, &[]).await?;
            ap(move |app| guard_form(app, guard_type(&href), v))
        }),
        Act::GuardAccess => guard_access(app, href, name),
        Act::Delete => app.job(format!("impact of deleting content guard {name}"), move |ctx| async move {
            let c = &ctx.client;
            let used = ops::guarded_distributions(c).await?;
            let mut lines = vec![format!("Delete content guard {name} ({}).", guard_type(&href))];
            match used.get(&href) {
                // pulpcore: `Distribution.content_guard` is `on_delete=SET_NULL`.
                Some(ds) => {
                    let n = ds.len();
                    lines.push(format!(
                        "Pulp removes the guard from {n} distribution(s): they become PUBLIC, anyone can download their content:"
                    ));
                    lines.extend(ds.iter().map(|d| format!("  {d}")));
                }
                None => lines.push("No distribution uses it.".into()),
            }
            let composites = "contentguards/core/composite/";
            let all = if ctx.caps.has_endpoint(composites) { c.list_all::<Value>(&c.api(composites), &[], 100_000).await? } else { vec![] };
            for g in all {
                let members: Vec<&str> = g["guards"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                let (cname, also) = (s(&g, "name"), used.get(&s(&g, "pulp_href")).map(|ds| format!(", also on {}", ds.join(", "))));
                lines.extend(match members.len() {
                    _ if !members.contains(&href.as_str()) => None,
                    // pulpcore `CompositeContentGuard.permit`: no guards = allowed.
                    1 => Some(format!(
                        "Composite guard {cname} loses its only guard: a composite without guards allows everyone{}",
                        also.unwrap_or_default()
                    )),
                    n => Some(format!("Composite guard {cname} loses it and keeps {} other guard(s).", n - 1)),
                });
            }
            ap(move |app| {
                app.confirm_danger(format!("Delete content guard {name}"), lines, &name.clone(), move |app| {
                    submit(app, format!("delete content guard {name}"), move |c| async move { Ok(c.delete(&href).await?) })
                })
            })
        }),
        _ => {}
    }
}

/// Create (`v` null) or edit (`v`: the guard) a content guard of type `t` (`core/header`). A secret or a
/// certificate is never shown: on edit, empty keeps it.
fn guard_form(app: &mut App, t: String, v: Value) {
    let edit = !v.is_null();
    let Some((_, schema, extra)) = GUARD_TYPES.iter().find(|(x, ..)| *x == t) else {
        return app.error(format!("unknown content guard type {t}"));
    };
    let (caps, href) = (caps(app), s(&v, "pulp_href"));
    let keep = |f: Field| if edit { f.help("empty = unchanged") } else { f.required() };
    let mut fields = vec![
        Field::text("name", "Name").required().value(s(&v, "name")),
        Field::text("description", "Description").value(s(&v, "description")),
    ];
    for &k in extra.iter().filter(|k| caps.as_ref().is_none_or(|c| c.has_field(schema, k))) {
        fields.push(match k {
            "header_name" => Field::text(k, "Header name").required().value(s(&v, k)),
            "header_value" => keep(Field::new(k, "Header value", FieldKind::Secret)),
            "jq_filter" => Field::text(k, "jq filter").value(s(&v, k)).help("applied to the header (JSON) first, e.g. .identity.org_id"),
            "env_var" => Field::text(k, "Environment variable").required().value(s(&v, k)).help("its name on the Pulp server"),
            "guards" => {
                let guards: Vec<&str> = v[k].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                let mut f = Field::pick(k, "Guards", Source::ContentGuards).multi().value(guards.join("\n")).exclude(vec![href.clone()]);
                if !guards.is_empty() {
                    f.shown = format!("{} selected", guards.len());
                }
                f.help("one that allows is enough; none = everyone may download")
            }
            _ => keep(Field::text(k, "CA certificate (PEM file path)")),
        });
    }
    let title = if edit { format!("Edit content guard {}", s(&v, "name")) } else { format!("New {t} content guard") };
    app.form(Form::new(title, fields), move |app, f| {
        let (mut b, mut cert) = (json!({}), None);
        for fld in f.fields.iter().filter(|x| !edit || x.changed()) {
            match (fld.key.as_str(), f.get(&fld.key)) {
                ("ca_certificate", path) => cert = Some(path.to_string()),
                ("guards", g) => b["guards"] = g.split('\n').filter(|x| !x.is_empty()).collect::<Vec<_>>().into(),
                // Pulp refuses an empty string; null clears an optional field.
                (k, "") if edit => b[k] = Value::Null,
                (_, "") => {}
                (k, val) => b[k] = val.into(),
            }
        }
        let (href, collection) = (href.clone(), format!("contentguards/{t}/"));
        submit(app, format!("{} content guard {}", if edit { "update" } else { "create" }, f.get("name")), move |c| async move {
            if let Some(path) = cert {
                b["ca_certificate"] = tokio::fs::read_to_string(&path).await.with_context(|| format!("read {path}"))?.into();
            }
            Ok(if edit { c.patch(&href, &b).await? } else { c.post(&c.api(&collection), &b).await? })
        });
        Ok(())
    });
}

/// Give or take the download role of an RBAC guard (`add_role/`, `remove_role/`); the form shows who has it now.
fn guard_access(app: &mut App, href: String, name: String) {
    let endpoint = "{r_b_a_c_content_guard_href}add_role/";
    if caps(app).is_some_and(|c| !c.has_endpoint(endpoint)) {
        return app.error(format!("not supported by this server: RBAC guard roles ({endpoint})"));
    }
    app.job(format!("read content guard {name}"), move |ctx| async move {
        let v: Value = ctx.client.get(&href, &[]).await?;
        let who = |k: &str, f: &str| {
            let names: Vec<String> = v[k].as_array().into_iter().flatten().map(|x| s(x, f)).collect();
            if names.is_empty() { "none".into() } else { names.join(", ") }
        };
        let now = format!("May download now: users {}; groups {}", who("users", "username"), who("groups", "name"));
        ap(move |app| {
            let fields = vec![
                Field::new("change", "Change", FieldKind::Choice(vec!["allow".into(), "remove".into()])),
                Field::pick("users", "Users", Source::Users).multi(),
                Field::pick("groups", "Groups", Source::Groups).multi(),
            ];
            app.form(Form::new(format!("Download access of content guard {name}"), fields).note(now), move |app, f| {
                let list = |k: &str| f.get(k).split('\n').filter(|x| !x.is_empty()).map(String::from).collect::<Vec<_>>();
                let (users, groups) = (list("users"), list("groups"));
                if users.is_empty() && groups.is_empty() {
                    return Err("pick a user or a group".into());
                }
                let (verb, path) = if f.get("change") == "allow" { ("allow", "add_role/") } else { ("remove", "remove_role/") };
                let (url, b) = (format!("{href}{path}"), json!({ "role": GUARD_DOWNLOADER, "users": users, "groups": groups }));
                submit(app, format!("{verb} download from {name}"), move |c| async move { Ok(c.post(&url, &b).await?) });
                Ok(())
            });
        })
    });
}

/// Delete the marked uploads, else the selected one. A plain confirmation, no typed name: an upload holds only
/// the chunks that a client sent (a commit deletes it), so nothing else on the server changes.
fn delete_uploads(app: &mut App, item: Value) {
    let marked: Vec<String> = app.list.marked.iter().cloned().collect();
    let mut lines = match marked.len() {
        0 => vec![format!(
            "Delete upload {} ({} bytes, created {}).",
            short(&s(&item, "pulp_href")),
            s(&item, "size"),
            s(&item, "pulp_created")
        )],
        n => std::iter::once(format!("Delete {n} marked upload(s):")).chain(marked.iter().map(|h| format!("  {}", short(h)))).collect(),
    };
    lines.push("Pulp deletes the chunks that a client sent; no content, artifact or repository changes.".into());
    lines.push("A client that still sends such an upload fails and must start again.".into());
    let hrefs = if marked.is_empty() { vec![s(&item, "pulp_href")] } else { marked };
    app.confirm("Delete uploads", lines, move |app| {
        for href in hrefs {
            // A failed delete keeps its mark; its error names the upload.
            let label = format!("delete upload {}", short(&href));
            app.job(label.clone(), move |ctx| async move {
                let m = ctx.client.delete(&href).await?;
                ap(move |app| unmark_when_deleted(app, href, m, label))
            });
        }
    });
}

/// Reset a customized access policy to the default of its plugin (`{access_policy_href}reset/`). The statements are
/// not edited here: a wrong one can lock users out, and a one-line field is no place for a JSON policy.
fn reset_policy(app: &mut App, item: Value) {
    let endpoint = "{access_policy_href}reset/";
    if caps(app).is_some_and(|c| !c.has_endpoint(endpoint)) {
        return app.error(format!("not supported by this server: access policy reset ({endpoint})"));
    }
    let (href, viewset) = (s(&item, "pulp_href"), s(&item, "viewset_name"));
    if item["customized"] != true {
        return app.error(format!("access policy {viewset} is not customized: it is the default already"));
    }
    let mut lines = vec![
        format!("Reset the access policy of {viewset} to the default of its plugin."),
        "Pulp replaces its statements, creation hooks and queryset scoping; the changes are lost.".into(),
        "Users may get or lose access to these objects at once.".into(),
        "Statements now:".into(),
    ];
    lines.extend(super::actions::pretty(&item["statements"]));
    app.confirm_danger(format!("Reset access policy {viewset}"), lines, &viewset.clone(), move |app| {
        submit(
            app,
            format!("reset access policy {viewset}"),
            move |c| async move { Ok(c.post(&format!("{href}reset/"), &json!({})).await?) },
        )
    });
}

// ---- domains ----------------------------------------------------------------------------

/// Storage classes when the OpenAPI has no enum: pulpcore's `BACKEND_CHOICES` without the deprecated S3Boto3Storage.
const STORAGE_CLASSES: [&str; 3] =
    ["pulpcore.app.models.storage.FileSystem", "storages.backends.s3.S3Storage", "storages.backends.azure_storage.AzureStorage"];

/// `S3Storage` of `storages.backends.s3.S3Storage`.
fn storage_name(class: &str) -> &str {
    class.rsplit('.').next().unwrap_or_default()
}

/// Storage class and settings of a domain create or migration. The settings can hold S3 or Azure keys: never shown.
fn storage_fields(app: &App) -> Vec<Field> {
    let classes = caps(app).and_then(|c| c.field_enum("Domain", "storage_class").map(<[String]>::to_vec));
    let example = r#"never shown; e.g. {"location": "/var/lib/pulp/media"} or {"bucket_name": "b", "access_key": "…", "secret_key": "…"}"#;
    vec![
        Field::new(
            "storage_class",
            "Storage class",
            FieldKind::Choice(classes.unwrap_or_else(|| STORAGE_CLASSES.map(String::from).to_vec())),
        ),
        Field::new("storage_settings", "Storage settings (JSON)", FieldKind::Secret).required().help(example),
    ]
}

fn storage_body(f: &Form) -> Result<Value, String> {
    let settings = serde_json::from_str::<Value>(f.get("storage_settings")).ok().filter(Value::is_object);
    Ok(json!({ "storage_class": f.get("storage_class"), "storage_settings": settings.ok_or("Storage settings must be a JSON object")? }))
}

/// What the content app does for the domain; `v` holds the current values.
fn domain_flags(v: &Value) -> Vec<Field> {
    let flag = |k: &str, label: &str, help: &str| {
        Field::new(k, label, FieldKind::Bool).value(if v[k] == true { "true" } else { "false" }).help(help)
    };
    vec![
        flag("redirect_to_object_storage", "Redirect to object storage", "downloads go to the storage, not through Pulp"),
        flag("hide_guarded_distributions", "Hide guarded distributions", "the content app does not list them"),
    ]
}

fn domain_action(app: &mut App, act: Act, item: Value) {
    let (href, name) = (s(&item, "pulp_href"), s(&item, "name"));
    if act != Act::Create && name == "default" {
        return app.error("Pulp does not edit, delete or migrate the default domain");
    }
    match act {
        Act::Create => {
            let name = Field::text("name", "Name").required().help("letters, digits, - and _; it becomes part of the URLs");
            let mut fields = vec![name, Field::text("description", "Description")];
            fields.extend(storage_fields(app));
            fields.extend(domain_flags(&json!({ "redirect_to_object_storage": true })));
            app.form(Form::new("New domain", fields), |app, f| {
                let mut b = storage_body(f)?;
                b["name"] = f.get("name").into();
                if let Some(d) = f.opt("description") {
                    b["description"] = d.into();
                }
                for k in ["redirect_to_object_storage", "hide_guarded_distributions"] {
                    b[k] = f.bool(k).into();
                }
                submit(app, format!("create domain {}", f.get("name")), move |c| async move { Ok(c.post(&c.api("domains/"), &b).await?) });
                Ok(())
            });
        }
        // The storage changes only with a migration; a rename would move every URL of the domain.
        Act::Edit => {
            let mut fields = vec![Field::text("description", "Description").value(s(&item, "description"))];
            fields.extend(domain_flags(&item));
            app.form(Form::new(format!("Edit domain {name}"), fields), move |app, f| {
                let mut b = json!({});
                for fld in f.fields.iter().filter(|x| x.changed()) {
                    b[&fld.key] = match (fld.kind == FieldKind::Bool, f.get(&fld.key)) {
                        (true, v) => (v == "true").into(),
                        (false, "") => Value::Null,
                        (false, v) => v.into(),
                    };
                }
                if b.as_object().is_some_and(|m| m.is_empty()) {
                    return Err("nothing changed".into());
                }
                let href = href.clone();
                submit(app, format!("update domain {name}"), move |c| async move { Ok(c.patch(&href, &b).await?) });
                Ok(())
            });
        }
        Act::Delete => {
            let storage = storage_name(&s(&item, "storage_class")).to_string();
            app.job(format!("impact of deleting domain {name}"), move |ctx| async move {
                let c = &ctx.client;
                let mut counts = vec![];
                let kinds = [("repositories/", "repositories"), ("remotes/", "remotes"), ("distributions/", "distributions")];
                for (path, what) in kinds.into_iter().chain([("content/", "content units"), ("artifacts/", "artifacts")]) {
                    counts.push(match c.list::<Value>(&c.domain_api(&name, path), &[("limit", "1".into())]).await {
                        Ok(p) => format!("{} {what}", p.count),
                        Err(e) if e.status() == Some(403) => format!("? {what} (you may not list them)"),
                        Err(e) => return Err(e.into()),
                    });
                }
                // pulpcore: `Domain._cleanup_orphans_pre_delete` and the delete are one transaction; artifact files go on commit.
                let lines = vec![
                    format!("Delete domain {name} (storage {storage})."),
                    format!("In it, as far as you may see: {}.", counts.join(", ")),
                    "Pulp refuses while a repository version holds content (PLP0007), or while a repository, remote,".into(),
                    "distribution or other object is in the domain: delete them first. The task then fails with Pulp's reason.".into(),
                    "Else Pulp deletes the orphan content and the artifacts of the domain with their files, and its task records.".into(),
                ];
                ap(move |app| {
                    app.confirm_danger(format!("Delete domain {name}"), lines, &name.clone(), move |app| {
                        submit(app, format!("delete domain {name}"), move |c| async move { Ok(c.delete(&href).await?) })
                    })
                })
            });
        }
        Act::MigrateStorage => {
            let endpoint = "domains/migrate/";
            if caps(app).is_some_and(|c| !c.has_endpoint(endpoint)) {
                return app.error(format!("not supported by this server: domain storage migration ({endpoint})"));
            }
            let (from, title, fields) = (
                storage_name(&s(&item, "storage_class")).to_string(),
                format!("Migrate the storage of domain {name}"),
                storage_fields(app),
            );
            app.form(Form::new(title.clone(), fields), move |app, f| {
                let b = storage_body(f)?;
                let lines = vec![
                    format!("Migrate the storage of domain {name} from {from} to {}.", storage_name(f.get("storage_class"))),
                    "Pulp copies every artifact of the domain to the new storage, then the domain uses it.".into(),
                    "Until the task ends, the other tasks of the domain wait; content is served from the old storage.".into(),
                    "Pulp does not delete the files in the old storage: remove them after the task.".into(),
                ];
                let name = name.clone();
                app.confirm_danger(title.clone(), lines, &name.clone(), move |app| {
                    // `migrate/` acts on the domain of the URL.
                    submit(app, format!("migrate the storage of domain {name}"), move |c| async move {
                        Ok(c.post(&c.domain_api(&name, endpoint), &b).await?)
                    })
                });
                Ok(())
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

/// Upstream Pulp fields that the form never pre-fills (write-only, or a PEM file path to read): empty = unchanged.
const CREDENTIALS: [(&str, &str); 5] = [
    ("username", "username"),
    ("password", "password"),
    ("ca_cert", "CA cert"),
    ("client_cert", "client cert"),
    ("client_key", "client key"),
];

/// Whether `upstream` has a value for `key`: a readable one, or a write-only one that `hidden_fields` marks set.
fn is_set(upstream: &Value, key: &str) -> bool {
    upstream[key].as_str().is_some_and(|s| !s.is_empty())
        || upstream["hidden_fields"].as_array().into_iter().flatten().any(|h| h["name"] == key && h["is_set"] == true)
}

fn upstream_action(app: &mut App, act: Act, item: Value) {
    let (href, name) = (s(&item, "pulp_href"), s(&item, "name"));
    match act {
        Act::Create | Act::Edit => upstream_form(app, act == Act::Edit, &item),
        Act::Replicate => replicate(app, &item),
        Act::ReplicationHistory => replication_history(app, href, name),
        Act::FallbackReplicate => fallback(app),
        Act::Delete => upstream_delete(app, href, name),
        _ => {}
    }
}

/// The origin of this client's profile: the destination of a replication.
fn dst_origin(app: &App) -> String {
    app.profile.as_ref().and_then(|p| app.cfg.profiles.get(p)).map(|p| p.origin()).unwrap_or_default()
}

/// The connection fields of the Upstream Pulp form (`v`: the stored object, Null for a new one).
fn upstream_connection_fields(edit: bool, v: &Value) -> Vec<Field> {
    let mut fields = vec![
        Field::text("name", "Name").required().value(s(v, "name")),
        Field::text("base_url", "Upstream base URL")
            .required()
            .value(s(v, "base_url"))
            .help("as reachable from THIS Pulp server, e.g. https://upstream.example.com"),
        Field::text("api_root", "Upstream API root")
            .value(if edit { s(v, "api_root") } else { "pulp/".into() })
            .help("relative, with trailing slash: pulp/"),
        Field::text("domain", "Upstream domain").value(s(v, "domain")),
        Field::text("username", "Username").help("write-only; empty = unchanged"),
        Field::new("password", "Password", FieldKind::Secret),
        Field::text("ca_cert", "CA cert (PEM file path)"),
        Field::text("client_cert", "Client cert (PEM file path)"),
        Field::text("client_key", "Client key (PEM file path)"),
        Field::new("tls_validation", "Validate TLS", FieldKind::Bool).value(if v["tls_validation"] == false { "false" } else { "true" }),
    ];
    // Empty means unchanged, so a stored credential is removed only with its own checkbox (sends null).
    let tls = fields.len() - 1;
    let clear = |(k, label): &(&str, &str)| {
        Field::new(&format!("clear_{k}"), &format!("Clear {label}"), FieldKind::Bool).help("removes the stored value (sends null)")
    };
    fields.splice(tls..tls, CREDENTIALS.iter().filter(|(k, _)| edit && is_set(v, k)).map(clear));
    fields
}

/// The replication fields of the Upstream Pulp form that the server's schema has.
fn upstream_replication_fields(caps: Option<&Capabilities>, edit: bool, v: &Value) -> Vec<Field> {
    let has = |f: &str| caps.is_none_or(|c| c.has_field("UpstreamPulp", f));
    let mut fields = vec![];
    if has("q_select") {
        fields.push(
            Field::text("q_select", "Distribution filter (q)")
                .value(s(v, "q_select"))
                .help(r#"e.g. name__startswith="rhel9" or pulp_label_select="env=prod""#),
        );
    } else if has("pulp_label_select") {
        fields.push(Field::text("pulp_label_select", "Label selector").value(s(v, "pulp_label_select")));
    }
    if has("policy") {
        let mut policies = caps
            .as_ref()
            .and_then(|c| c.field_enum("UpstreamPulp", "policy").map(|e| e.to_vec()))
            .unwrap_or_else(|| vec!["nodelete".into(), "labeled".into(), "all".into()]);
        policies.sort_by_key(|p| p != "nodelete"); // non-destructive default first
        fields.push(Field::new("policy", "Deletion policy", FieldKind::Choice(policies)).value(if edit {
            s(v, "policy")
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
        fields.push(Field::new("remote_policy", "Replica download policy", FieldKind::Choice(p)).value(s(v, "remote_policy")).help(help));
    }
    let number = |k: &str, label: &str, kind: FieldKind, value: String| {
        Field::new(k, label, kind).value(value).help("for the replica remotes; empty = server default")
    };
    for (k, label) in [("download_concurrency", "Download concurrency"), ("max_retries", "Max retries")] {
        fields.extend(has(k).then(|| number(k, label, FieldKind::Number, s(v, k))));
    }
    for (k, label) in [
        ("total_timeout", "Total timeout (s)"),
        ("connect_timeout", "Connect timeout (s)"),
        ("sock_connect_timeout", "Socket connect timeout (s)"),
        ("sock_read_timeout", "Socket read timeout (s)"),
    ] {
        // Floats on the server: shown as stored (3600.0 as `3600`, 2.5 as `2.5`).
        let stored = v[k].as_f64().map(|x| x.to_string()).unwrap_or_default();
        fields.extend(has(k).then(|| number(k, label, FieldKind::Decimal, stored)));
    }
    fields
}

fn upstream_form(app: &mut App, edit: bool, item: &Value) {
    let v = if edit { item.clone() } else { Value::Null };
    let mut fields = upstream_connection_fields(edit, &v);
    fields.extend(upstream_replication_fields(caps(app).as_deref(), edit, &v));
    let (href, name, dst_origin) = (s(item, "pulp_href"), s(item, "name"), dst_origin(app));
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
        let (b, files) = upstream_body(f, edit)?;
        save_upstream(app, f, edit, href.clone(), b, files);
        Ok(())
    });
}

/// Whether the form leaves `fld` out: an edit sends only changed fields, a new object only set ones, and an empty
/// credential keeps the stored one.
fn left_out(fld: &Field, edit: bool) -> bool {
    let write_only = CREDENTIALS.iter().any(|(k, _)| *k == fld.key);
    (write_only || !edit) && fld.value.trim().is_empty() || edit && !write_only && !fld.changed()
}

/// The body of the Upstream Pulp form, and the PEM files (field, path) that the job reads into it.
fn upstream_body(f: &Form, edit: bool) -> Result<(Value, Vec<(String, String)>), String> {
    let (mut b, mut files) = (json!({}), vec![]);
    for fld in f.fields.iter().filter(|fld| !left_out(fld, edit)) {
        let val = fld.value.trim();
        match fld.key.as_str() {
            "ca_cert" | "client_cert" | "client_key" => files.push((fld.key.clone(), val.to_string())),
            // After the credential fields: a checked clear wins over a typed value.
            k if k.starts_with("clear_") => {
                let k = &k["clear_".len()..];
                if val == "true" {
                    b[k] = Value::Null;
                    files.retain(|(f, _)| f != k);
                }
            }
            k => b[k] = json_value(fld, val)?,
        }
    }
    Ok((b, files))
}

/// A form value as Pulp takes it: a bool, null for empty, a number, else the text.
fn json_value(fld: &Field, val: &str) -> Result<Value, String> {
    let nan = || format!("{} must be a number", fld.label);
    Ok(match fld.kind {
        FieldKind::Bool => (val == "true").into(),
        _ if val.is_empty() => Value::Null,
        FieldKind::Number => val.parse::<i64>().map_err(|_| nan())?.into(),
        FieldKind::Decimal => val.parse::<f64>().map_err(|_| nan())?.into(),
        _ => val.into(),
    })
}

/// Send the Upstream Pulp form. Setting a destructive policy needs its typed word first; an edit that keeps the
/// stored policy was confirmed when that policy was set.
fn save_upstream(app: &mut App, f: &Form, edit: bool, href: String, b: Value, files: Vec<(String, String)>) {
    let label = format!("{} upstream {}", if edit { "update" } else { "create" }, f.get("name"));
    let go = move |app: &mut App| send_upstream(app, label, edit, href, b, files);
    let policy = f.get("policy");
    match deletion_warning(policy).filter(|_| !edit || f.changed("policy")) {
        // An empty policy is no word to type: the Upstream name instead.
        Some(w) => {
            app.confirm_danger("Destructive replication policy", vec![w], if policy.is_empty() { f.get("name") } else { policy }, go)
        }
        None => go(app),
    }
}

/// Read the PEM `files` into `b`, then create the Upstream Pulp or PATCH `href`.
fn send_upstream(app: &mut App, label: String, edit: bool, href: String, mut b: Value, files: Vec<(String, String)>) {
    submit(app, label, move |c| async move {
        for (k, path) in files {
            b[k.as_str()] = tokio::fs::read_to_string(&path).await.with_context(|| format!("read {path}"))?.into();
        }
        Ok(if edit { c.patch(&href, &b).await? } else { c.post(&c.api("upstream-pulps/"), &b).await? })
    })
}

/// Confirm and start a native replication from the Upstream Pulp `item`.
fn replicate(app: &mut App, item: &Value) {
    let (href, name, dst_origin) = (s(item, "pulp_href"), s(item, "name"), dst_origin(app));
    // Also here: an Upstream Pulp made with another tool never passed the check of the form.
    let origin = url::Url::parse(&s(item, "base_url")).map(|u| u.origin().ascii_serialization()).unwrap_or_default();
    if let Err(e) = replication::guard_self(&origin, &dst_origin) {
        return app.error(e.to_string());
    }
    let policy = s(item, "policy");
    let mut lines = vec![
        format!("Replicate from {} into this server ({dst_origin}).", s(item, "base_url")),
        format!(
            "Distribution filter: {}",
            if s(item, "q_select").is_empty() { "ALL upstream distributions".into() } else { s(item, "q_select") }
        ),
        format!("Deletion policy: {}", if policy.is_empty() { "server default".to_string() } else { policy.clone() }),
        "Pulp creates/updates repositories, remotes and distributions with the upstream names.".into(),
        "This is a technology preview of pulpcore; check the compatibility notes.".into(),
    ];
    let title = format!("Replicate {name}");
    let go = move |app: &mut App| {
        app.job(format!("replicate {name}"), move |ctx| async move {
            let res = replication::replicate_native(&ctx.client, &href, &group_progress(&ctx)).await;
            ap(move |app| show_replication(app, &name, res))
        });
    };
    if let Some(w) = deletion_warning(&policy) {
        lines.insert(0, w);
        app.confirm_danger(title, lines, &s(item, "name"), go);
    } else {
        app.confirm(title, lines, go);
    }
}

/// The result view of a replication: its title names any outcome but completed.
fn show_replication(app: &mut App, name: &str, res: anyhow::Result<Vec<String>>) {
    match res {
        Ok(lines) => app.text(format!("Replication of {name}"), lines),
        Err(e) => {
            let outcome = match e.downcast_ref() {
                Some(PulpError::TaskFailed { state, .. }) => state.to_uppercase(),
                _ => "FAILED".into(),
            };
            app.text(format!("Replication of {name} {outcome}"), format!("{e:#}").lines().map(str::to_string).collect())
        }
    }
}

const REPLICATE: &str = "pulpcore.app.tasks.replica.replicate_distributions";

/// The five newest runs of the Upstream Pulp `href`.
fn replication_history(app: &mut App, href: String, name: String) {
    app.job("replication history", move |ctx| async move {
        let c = &ctx.client;
        // A run's `replicate_distributions` reserves the Upstream Pulp (pulpcore 3.123; not 3.95): a stable link,
        // unlike the group's description, which holds the name at the time of the run.
        let q = [
            ("reserved_resources", href.clone()),
            ("name", REPLICATE.into()),
            ("ordering", "-pulp_created".into()),
            ("fields", "task_group".into()),
            ("limit", "5".into()),
        ];
        let mut groups = vec![];
        for g in c.list::<Value>(&c.api("tasks/"), &q).await?.results.iter().filter_map(|t| t["task_group"].as_str()) {
            groups.push(c.get::<Value>(g, &[]).await?);
        }
        let by_name = groups.is_empty() && runs_reserve_no_upstream(c).await?;
        if by_name {
            groups = groups_named(c, &format!("Replication of {name}")).await?;
        }
        let lines = history_lines(&groups, by_name.then_some(name.as_str()));
        ap(move |app| app.text(format!("Replication history of {name}"), lines))
    });
}

/// The History text of the task `groups`; `by_name` is the name they were found by, if no task links them.
fn history_lines(groups: &[Value], by_name: Option<&str>) -> Vec<String> {
    let mut lines = vec![];
    for g in groups {
        lines.push(format!("{}  completed {} failed {} canceled {}", s(g, "pulp_href"), g["completed"], g["failed"], g["canceled"]));
        lines.extend(g["tasks"].as_array().into_iter().flatten().map(|t| format!("    {:<10} {}", s(t, "state"), s(t, "name"))));
    }
    match by_name {
        _ if lines.is_empty() => lines.push("no replication of this upstream found (purged tasks are gone)".into()),
        Some(name) => lines.push(format!("(found by the name {name} at run time: this pulpcore does not link a run to its Upstream Pulp)")),
        None => {}
    }
    lines
}

/// The newest replication run of this server reserves no Upstream Pulp (pulpcore 3.95 and older).
async fn runs_reserve_no_upstream(c: &PulpClient) -> anyhow::Result<bool> {
    let q = [
        ("name", REPLICATE.into()),
        ("ordering", "-pulp_created".into()),
        ("fields", "reserved_resources_record".into()),
        ("limit", "1".into()),
    ];
    let newest = c.list::<Value>(&c.api("tasks/"), &q).await?.results;
    Ok(newest.first().is_some_and(|t| !t["reserved_resources_record"].to_string().contains("upstream")))
}

/// The five newest task groups with the description `want`. task-groups/ has no filters (newest first): read pages
/// of 100 until five match, at most 20 pages.
async fn groups_named(c: &PulpClient, want: &str) -> anyhow::Result<Vec<Value>> {
    let (mut groups, mut next) = (vec![], Some(c.api("task-groups/?limit=100")));
    for _ in 0..20 {
        let Some(href) = next.take().filter(|_| groups.len() < 5) else { break };
        let page = c.list::<Value>(&href, &[]).await?;
        let room = 5 - groups.len();
        groups.extend(page.results.into_iter().filter(|g| g["description"].as_str() == Some(want)).take(room));
        next = page.next;
    }
    Ok(groups)
}

/// Delete the Upstream Pulp `href`: first what uses it, then the typed name.
fn upstream_delete(app: &mut App, href: String, name: String) {
    app.job(format!("impact of deleting upstream {name}"), move |ctx| async move {
        let c = &ctx.client;
        // pulpcore labels what a replication creates with `UpstreamPulp=<pk>`; its tasks reserve the Upstream Pulp.
        let pk = href.trim_end_matches('/').rsplit('/').next().unwrap_or_default().to_string();
        let label = [("pulp_label_select", format!("UpstreamPulp={pk}")), ("limit", "1".into())];
        let mut n = vec![];
        for p in ["repositories/", "remotes/", "distributions/"] {
            n.push(c.list::<Value>(&c.api(p), &label).await?.count);
        }
        let q = [("reserved_resources", href.clone()), ("state__in", "waiting,running".into()), ("limit", "1".into())];
        let busy = c.list::<Value>(&c.api("tasks/"), &q).await?.count;
        let mut lines = vec![
            format!("Delete the Upstream Pulp definition {name}."),
            format!(
                "Its replications labelled {} repositories, {} remotes and {} distributions (UpstreamPulp={pk}): they stay.",
                n[0], n[1], n[2]
            ),
        ];
        if busy > 0 {
            lines.push(format!("{busy} replication task(s) of it are waiting or running: they fail without it."));
        }
        ap(move |app| {
            app.confirm_danger(format!("Delete upstream {name}"), lines, &name.clone(), move |app| {
                submit(app, format!("delete upstream {name}"), move |c| async move { Ok(c.delete(&href).await?) })
            })
        })
    });
}

/// The types a "Types" choice of the fallback form selects. Deb is a choice of its own: the default must not need pulp_deb.
fn replication_types(choice: &str) -> Vec<RepoType> {
    match choice {
        "rpm" => vec![RepoType::Rpm],
        "file" => vec![RepoType::File],
        "deb" => vec![RepoType::Deb],
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
            Field::new("types", "Types", FieldKind::Choice(vec!["rpm+file".into(), "rpm".into(), "file".into(), "deb".into()])),
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
    let fields = exporter_fields(caps(app).as_deref(), fs, edit, item);
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
        let b = exporter_body(f, edit);
        let href = href.clone();
        submit(app, format!("{} exporter {}", if edit { "update" } else { "create" }, f.get("name")), move |c| async move {
            Ok(if edit { c.patch(&href, &b).await? } else { c.post(&c.api(collection), &b).await? })
        });
        Ok(())
    });
}

/// The body of the exporter form: an edit sends only the changed fields.
fn exporter_body(f: &Form, edit: bool) -> Value {
    let mut b = json!({});
    for fld in f.fields.iter().filter(|x| !edit || x.changed()) {
        let k = fld.key.as_str();
        b[k] = match k {
            "repositories" => f.get(k).split('\n').filter(|v| !v.is_empty()).collect::<Vec<_>>().into(),
            _ => f.get(k).into(),
        };
    }
    b
}

/// A Pulp exporter has repositories (Content View snapshots too); a filesystem exporter (`fs`) a method instead.
fn exporter_fields(caps: Option<&Capabilities>, fs: bool, edit: bool, item: &Value) -> Vec<Field> {
    let mut fields = vec![
        Field::text("name", "Name").required().value(s(item, "name")),
        Field::text("path", "Directory on the Pulp server").required().value(s(item, "path")).help("inside its ALLOWED_EXPORT_PATHS"),
    ];
    if !fs {
        let repos: Vec<&str> = item["repositories"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let mut pick = Field::pick("repositories", "Repositories", Source::ExportRepos).multi().required().value(repos.join("\n"));
        if !repos.is_empty() {
            pick.shown = format!("{} selected", repos.len());
        }
        fields.push(pick);
    } else if caps.is_none_or(|c| c.has_field("FilesystemExporter", "method")) {
        let methods = caps
            .and_then(|c| c.field_enum("FilesystemExporter", "method").map(<[String]>::to_vec))
            .unwrap_or_else(|| ["write", "hardlink", "symlink"].map(String::from).to_vec());
        let method = Field::new("method", "Method", FieldKind::Choice(methods)).help("hardlink and symlink need Pulp's file storage");
        fields.push(if edit { method.value(s(item, "method")) } else { method });
    }
    fields
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
    let mut fields = vec![
        Field::pick("repo", "Repository", Source::ExportRepos).required(),
        Field::new("number", "Version number", FieldKind::Number).help("empty = latest"),
    ];
    if caps(app).is_none_or(|c| c.has_field("FilesystemExport", "start_repository_version")) {
        let help = "only the files that this version of the repository lacks (a publication: and its metadata); empty = all";
        fields.push(Field::new("start", "Start version number", FieldKind::Number).help(help));
    }
    fields.push(Field::new("what", "Export", FieldKind::Choice(what)).help("publication: the newest publication of the version"));
    let form = Form::new(format!("Export {name}"), fields)
        .note(format!("Pulp makes {path} on the Pulp server and writes the files into it."))
        .note("It must not exist yet: for the next export, edit the exporter's directory first.");
    app.form(form, move |app, f| {
        let repo = f.get("repo").to_string();
        let t = RepoType::from_href(&repo).ok_or("pick an RPM, File or Deb repository")?;
        let (number, publication) = (f.number("number"), f.get("what") == "publication");
        let start = f.number("start").map(|n| format!("{repo}versions/{n}/"));
        let (href, name, path) = (href.clone(), name.clone(), path.clone());
        app.job(format!("export {name}"), move |ctx| async move {
            let c = &ctx.client;
            let mut body = fs_export_source(c, t, &repo, number, publication).await?;
            if let Some(s) = start {
                body["start_repository_version"] = s.into();
            }
            ops::create(c, &format!("{href}exports/"), &body, "/exports/", &ctx.on_task()).await?;
            let what = if publication { "publication" } else { "repository version" };
            ap(move |app| app.ok(format!("export {name}: {what} written to {path}")))
        });
        Ok(())
    });
}

/// What a filesystem export of `repo` writes: version `number` (None = latest), or its newest publication.
async fn fs_export_source(c: &PulpClient, t: RepoType, repo: &str, number: Option<i64>, publication: bool) -> anyhow::Result<Value> {
    let v = match number {
        Some(n) => format!("{repo}versions/{n}/"),
        None => s(&c.get::<Value>(repo, &[]).await?, "latest_version_href"),
    };
    if !publication {
        return Ok(json!({ "repository_version": v }));
    }
    let q = [("repository_version", v.clone()), ("ordering", "-pulp_created".into()), ("limit", "1".into())];
    let newest = c.list::<Value>(&c.api(t.publications()), &q).await?.results.first().map(|p| s(p, "pulp_href"));
    let p = newest.ok_or_else(|| anyhow::anyhow!("{} has no publication: publish it, or export the repository version", short(&v)))?;
    Ok(json!({ "publication": p }))
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
        Act::Import => import_form(app, href, name),
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
                let Some(r) = ops::find_by_name::<Value>(&c, &c.api("repositories/"), d).await? else {
                    bail!("no repository named {d}: every destination must exist");
                };
                if let Some(why) = snapshot_refusal(&r) {
                    bail!(why);
                }
            }
            Ok(if edit { c.patch(&href, &b).await? } else { c.post(&c.api("importers/core/pulp/"), &b).await? })
        });
        Ok(())
    });
}

/// The import check is the preview; the import runs only after the confirmation.
fn import_form(app: &mut App, href: String, name: String) {
    let help = "the -toc.json of the export, or its .tar (an export without chunks); inside ALLOWED_IMPORT_PATHS";
    let toc = Field::text("toc", "TOC or export file on the Pulp server").required().help(help);
    let form = Form::new(format!("Import with {name}"), vec![toc])
        .note("Pulp checks the file and the mapping first; nothing is imported before you confirm.");
    let tar_ok = caps(app).is_none_or(|c| c.has_field("PulpImport", "path"));
    app.form(form, move |app, f| {
        let toc = f.get("toc").to_string();
        if ops::import_field(&toc) == "path" && !tar_ok {
            return Err("not supported by this server: import from an export .tar (path)".into());
        }
        let (href, name) = (href.clone(), name.clone());
        app.job(format!("import check {toc}"), move |ctx| async move {
            let c = &ctx.client;
            // The import uses the importer's mapping as Pulp has it now; the list row can be stale.
            let mapping = c.get::<Value>(&href, &[]).await?["repo_mapping"].take();
            let pairs = mapping.as_object().cloned().unwrap_or_default();
            let q = [("pulp_label_select", crate::cv::LABEL_CV.to_string()), ("fields", "name,pulp_labels".into())];
            let snaps: Vec<Value> = c.list_all(&c.api("repositories/"), &q, 100_000).await?;
            if let Some(why) = snaps.iter().filter(|r| pairs.values().any(|d| *d == r["name"])).find_map(snapshot_refusal) {
                bail!(why);
            }
            // Pulp names the repositories of an export only in the task (not in import-check or the TOC), and an
            // unmapped one goes into the repository of the same name: name each snapshot that this can change.
            let same: Vec<String> = snaps
                .iter()
                .filter(|r| !pairs.contains_key(&s(r, "name")))
                .map(|r| format!("  {} (Content View {})", s(r, "name"), s(&r["pulp_labels"], crate::cv::LABEL_CV)))
                .collect();
            let (ok, lines) = ops::import_check(c, &toc, &mapping).await?;
            ap(move |app| match ok {
                true => confirm_import(app, href, name, toc, &mapping, lines, same),
                false => app.text(format!("Import check of {toc} failed"), lines),
            })
        });
        Ok(())
    });
}

/// `snapshots`: the lines of the Content View snapshots that an unmapped repository of the same name would change.
fn confirm_import(app: &mut App, href: String, name: String, toc: String, mapping: &Value, mut lines: Vec<String>, snapshots: Vec<String>) {
    lines.push(String::new());
    lines.push("Each repository of the export gets a new version in its destination repository:".into());
    lines.extend(mapping.as_object().into_iter().flatten().map(|(src, dst)| format!("  {src} -> {}", dst.as_str().unwrap_or_default())));
    lines.push("  any other: the repository of the same name (Pulp skips it when there is none)".into());
    lines.push("The new version holds exactly the exported content; older versions stay.".into());
    let warn = !snapshots.is_empty();
    if warn {
        lines.push(String::new());
        lines.push("WARNING: Pulp does not tell before the import which repositories the export has. An unmapped".into());
        lines.push("one with the name of a Content View snapshot changes that snapshot outside its Content View:".into());
        lines.extend(snapshots);
        lines.push("Map such a repository to another destination (e), or type the importer name to import.".into());
    }
    let title = format!("Import {toc}?");
    let typed = name.clone();
    let run = move |app: &mut App| {
        app.job(format!("import {name}"), move |ctx| async move {
            let (group, mut lines) = ops::pulp_import(&ctx.client, &href, &toc, &group_progress(&ctx)).await?;
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
    };
    if warn { app.confirm_danger(title, lines, &typed, run) } else { app.confirm_prod(title, lines, &typed, run) }
}

#[cfg(test)]
mod tests {
    use crate::pulp::RepoType;

    #[test]
    fn replication_types_offer_deb_alone_and_keep_the_rpm_and_file_default() {
        assert_eq!(super::replication_types("rpm+file"), [RepoType::Rpm, RepoType::File]);
        assert_eq!(super::replication_types("rpm"), [RepoType::Rpm]);
        assert_eq!(super::replication_types("file"), [RepoType::File]);
        assert_eq!(super::replication_types("deb"), [RepoType::Deb]);
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

    /// The title of the result view that a job left open.
    fn result_title(app: &App) -> String {
        let Some(Modal::Text { title, .. }) = &app.modal else { panic!("no result view: {:?}", notices(app)) };
        title.clone()
    }

    #[tokio::test]
    async fn a_replication_with_canceled_or_skipped_tasks_is_not_a_success() {
        for (state, counts, title) in [
            ("canceled", "canceled 1", "Replication of up CANCELED"),
            ("skipped", "skipped 1", "Replication of up SKIPPED"),
            ("failed", "failed 1", "Replication of up FAILED"),
        ] {
            let server = MockServer::start().await;
            serve(&server, "GET", GROUP, 200, group(state)).await;
            let (mut app, mut rx) = start_replication(&server).await;
            settle(&mut app, &mut rx).await;
            let n = notices(&app);
            assert!(n.iter().any(|(l, t)| *l != Level::Ok && t.contains(counts)), "{state}: {n:?}");
            assert!(!n.iter().any(|(l, t)| *l == Level::Ok && t.contains("replicate up")), "{state}: {n:?}");
            assert_eq!(result_title(&app), title, "{state}");
        }
        let server = MockServer::start().await;
        serve(&server, "GET", GROUP, 200, group("completed")).await;
        let (mut app, mut rx) = start_replication(&server).await;
        settle(&mut app, &mut rx).await;
        assert!(notices(&app).iter().any(|(l, t)| *l == Level::Ok && t.contains("completed")), "{:?}", notices(&app));
        assert_eq!(result_title(&app), "Replication of up");
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

    /// Exporters take Content View snapshots: a published version is what goes to a disconnected server. They are marked.
    #[tokio::test]
    async fn exporters_pick_content_view_snapshots_marked_as_such() {
        use crate::tui::kinds::Act;
        use wiremock::matchers::query_param_is_missing;
        let server = MockServer::start().await;
        let rows = json!({ "count": 2, "next": null, "results": [
            { "name": "os", "pulp_href": "/h/os/", "pulp_labels": {} },
            { "name": "pulp-tui-cv-web-os", "pulp_href": "/h/snap/", "pulp_labels": { "pulp_tui_cv": "web", "pulp_tui_db": "x" } }] });
        // Like Pulp: a `pulp_label_select=!pulp_tui_cv` query would not list the snapshot.
        Mock::given(method("GET"))
            .and(path("/pulp/api/v3/repositories/rpm/rpm/"))
            .and(query_param_is_missing("pulp_label_select"))
            .respond_with(ResponseTemplate::new(200).set_body_json(rows))
            .mount(&server)
            .await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        let caps = crate::pulp::Capabilities {
            versions: [("rpm".to_string(), "3.39.5".to_string())].into(),
            paths: ["repositories/rpm/rpm/".to_string()].into(),
            ..Default::default()
        };
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        let fs = json!({ "pulp_href": "/e/1/", "name": "fs", "path": "/exports/fs" });
        for (kind, act, item, field) in
            [(Kind::PulpExporters, Act::Create, Value::Null, "repositories"), (Kind::FsExporters, Act::Export, fs, "repo")]
        {
            app.list.kind = kind.clone();
            super::exporter_action(&mut app, act, item);
            let Some(Modal::Form { form, .. }) = &mut app.modal else { panic!("no form: {:?}", notices(&app)) };
            form.focus = form.fields.iter().position(|f| f.key == field).unwrap();
            app.handle_key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Enter));
            settle(&mut app, &mut rx).await;
            let Some(Modal::Form { form, .. }) = &app.modal else { panic!("no form: {:?}", notices(&app)) };
            let shown: Vec<_> = form.visible().iter().map(|i| (i.label.clone(), i.value.clone())).collect();
            let want = [("os", "/h/os/"), ("pulp-tui-cv-web-os (snapshot of Content View web)", "/h/snap/")];
            assert_eq!(shown, want.map(|(l, v)| (l.to_string(), v.to_string())), "{kind:?}");
            app.modal = None;
        }
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

    /// Delete loads what uses the Upstream Pulp (the objects its replications labelled, its running tasks), then asks
    /// for the typed name.
    #[tokio::test]
    async fn upstream_delete_shows_what_uses_it_and_asks_for_the_name() {
        use crate::tui::kinds::Act;
        let server = MockServer::start().await;
        let count = |n: u64| json!({ "count": n, "next": null, "results": [] });
        for (p, n) in [("repositories/", 2), ("remotes/", 4), ("distributions/", 3), ("tasks/", 1)] {
            serve(&server, "GET", &format!("/pulp/api/v3/{p}"), 200, count(n)).await;
        }
        serve(&server, "DELETE", UP, 204, Value::Null).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        app.list.kind = Kind::UpstreamPulps;
        super::run(&mut app, Act::Delete, json!({ "pulp_href": UP, "name": "up" }));
        assert!(app.modal.is_none() && sent(&server).await.is_empty(), "the impact first");
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, typed: Some(typed), on_yes: Some(yes), .. }) = app.modal.take() else {
            panic!("no confirm: {:?}", notices(&app))
        };
        assert_eq!(typed, "up");
        assert_eq!(
            lines,
            [
                "Delete the Upstream Pulp definition up.",
                "Its replications labelled 2 repositories, 4 remotes and 3 distributions (UpstreamPulp=1): they stay.",
                "1 replication task(s) of it are waiting or running: they fail without it.",
            ]
        );
        let reqs = server.received_requests().await.unwrap();
        let asked: Vec<_> = reqs.iter().filter(|r| r.method.as_str() == "GET").filter_map(|r| r.url.query().map(str::to_string)).collect();
        assert!(asked.iter().any(|q| q.contains("pulp_label_select=UpstreamPulp%3D1")), "{asked:?}");
        assert!(asked.iter().any(|q| q.contains("reserved_resources=%2Fpulp%2Fapi%2Fv3%2Fupstream-pulps%2F1%2F")), "{asked:?}");
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        assert_eq!(sent(&server).await, [("DELETE".into(), UP.into(), Value::Null)]);

        // Nothing waits or runs: no task line.
        server.reset().await;
        for p in ["repositories/", "remotes/", "distributions/", "tasks/"] {
            serve(&server, "GET", &format!("/pulp/api/v3/{p}"), 200, count(0)).await;
        }
        super::run(&mut app, Act::Delete, json!({ "pulp_href": UP, "name": "up" }));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, .. }) = app.modal.take() else { panic!("no confirm: {:?}", notices(&app)) };
        assert_eq!(lines.len(), 2, "{lines:?}");
    }

    /// Create on a selected role starts from its permissions, never its name: an unchanged submit would be a duplicate.
    #[tokio::test]
    async fn role_create_copies_the_permissions_but_not_the_name() {
        use crate::tui::kinds::Act;
        let server = MockServer::start().await;
        serve(&server, "POST", "/pulp/api/v3/roles/", 201, json!({ "pulp_href": "/pulp/api/v3/roles/9/" })).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        app.list.kind = Kind::Roles;
        let selected = json!({ "pulp_href": "/pulp/api/v3/roles/1/", "name": "rpm.viewer", "description": "view RPM", "locked": true,
            "permissions": ["rpm.view_rpmrepository", "rpm.view_rpmremote"] });
        let (mut f, mut submit) = open_form(&mut app, Act::Create, selected);
        assert_eq!((f.get("name"), f.get("description")), ("", ""));
        assert_eq!(f.get("permissions"), "rpm.view_rpmrepository,rpm.view_rpmremote");
        assert!(f.validate().is_err(), "a name is required");
        set(&mut f, "name", "my.viewer");
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        let want = json!({ "name": "my.viewer", "description": "", "permissions": ["rpm.view_rpmrepository", "rpm.view_rpmremote"] });
        assert_eq!(sent(&server).await, [("POST".into(), "/pulp/api/v3/roles/".into(), want)]);
    }

    /// Edit: the label names the upstream, a stored destructive policy asks again only when it changes, and set
    /// write-only credentials can be cleared.
    #[tokio::test]
    async fn upstream_edit_confirms_only_a_policy_change_and_clears_credentials() {
        use crate::tui::kinds::Act;
        let server = MockServer::start().await;
        serve(&server, "PATCH", UP, 200, json!({ "pulp_href": UP })).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        app.list.kind = Kind::UpstreamPulps;
        let item = json!({ "pulp_href": UP, "name": "up", "base_url": "https://upstream.example.com/", "api_root": "pulp/",
            "policy": "all", "ca_cert": "PEM", "client_cert": null, "tls_validation": true,
            "hidden_fields": [{ "name": "client_key", "is_set": false }, { "name": "username", "is_set": true }, { "name": "password", "is_set": true }] });
        let (mut f, mut submit) = open_form(&mut app, Act::Edit, item);
        let keys: Vec<_> = f.fields.iter().map(|x| x.key.as_str()).collect();
        let at = keys.iter().position(|k| *k == "client_key").unwrap();
        let clear = ["clear_username", "clear_password", "clear_ca_cert", "tls_validation"];
        assert_eq!(keys[at + 1..at + 5], clear, "only what is set, after the credential fields");

        set(&mut f, "tls_validation", "false");
        submit(&mut app, &f).unwrap();
        assert!(app.modal.is_none(), "the stored policy `all` is not asked again");
        settle(&mut app, &mut rx).await;
        assert!(notices(&app).iter().any(|(l, t)| *l == Level::Ok && t == "update upstream up: done"), "{:?}", notices(&app));

        set(&mut f, "clear_password", "true");
        set(&mut f, "clear_ca_cert", "true");
        set(&mut f, "ca_cert", "/nonexistent/ca.pem"); // a checked clear wins: the file is not read
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("client.pem"), "CERT").unwrap();
        set(&mut f, "client_cert", &dir.path().join("client.pem").display().to_string()); // a new one is read
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;

        set(&mut f, "policy", "labeled");
        submit(&mut app, &f).unwrap();
        let Some(Modal::Confirm { typed: Some(typed), on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm") };
        assert_eq!(typed, "labeled");
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        let bodies: Vec<Value> = sent(&server).await.into_iter().map(|(_, _, b)| b).collect();
        assert_eq!(
            bodies,
            [
                json!({ "tls_validation": false }),
                json!({ "tls_validation": false, "password": null, "ca_cert": null, "client_cert": "CERT" }),
                json!({ "tls_validation": false, "password": null, "ca_cert": null, "client_cert": "CERT", "policy": "labeled" }),
            ]
        );
    }

    /// The runs of an upstream are the groups of the `replicate_distributions` tasks that reserve it (pulpcore 3.123
    /// `shared_resources=[server]`): also after a rename, and never another upstream's group with the same description.
    #[tokio::test]
    async fn replication_history_lists_the_groups_of_the_tasks_that_reserve_the_upstream() {
        use wiremock::matchers::query_param;
        const GROUPS: &str = "/pulp/api/v3/task-groups/";
        let server = MockServer::start().await;
        let runs = json!({ "count": 2, "next": null, "results": [{ "task_group": format!("{GROUPS}7/") }, { "task_group": format!("{GROUPS}3/") }] });
        Mock::given(method("GET"))
            .and(path("/pulp/api/v3/tasks/"))
            .and(query_param("reserved_resources", UP))
            .and(query_param("name", "pulpcore.app.tasks.replica.replicate_distributions"))
            .and(query_param("limit", "5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(runs))
            .mount(&server)
            .await;
        let task = |state: &str| json!({ "name": "pulpcore.app.tasks.replica.replicate_distributions", "state": state });
        let g = |n: u32, d: &str, state: &str| json!({ "pulp_href": format!("{GROUPS}{n}/"), "description": d, "completed": 1, "failed": 0, "canceled": 0, "tasks": [task(state)] });
        serve(&server, "GET", &format!("{GROUPS}7/"), 200, g(7, "Replication of up", "completed")).await;
        serve(&server, "GET", &format!("{GROUPS}3/"), 200, g(3, "Replication of old-name", "failed")).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        super::upstream_action(&mut app, crate::tui::kinds::Act::ReplicationHistory, json!({ "pulp_href": UP, "name": "up" }));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Text { lines, .. }) = &app.modal else { panic!("no history: {:?}", notices(&app)) };
        assert_eq!(
            lines,
            &[
                format!("{GROUPS}7/  completed 1 failed 0 canceled 0"),
                "    completed  pulpcore.app.tasks.replica.replicate_distributions".to_string(),
                format!("{GROUPS}3/  completed 1 failed 0 canceled 0"),
                "    failed     pulpcore.app.tasks.replica.replicate_distributions".to_string(),
            ]
        );
        let reqs = server.received_requests().await.unwrap();
        assert!(!reqs.iter().any(|r| r.url.path() == GROUPS), "no scan of task-groups/");
    }

    /// `tasks/` answers the query of reserved runs with none, and the newest run (`limit=1`) with `record`.
    async fn no_reserved_runs(server: &MockServer, record: Value) {
        use wiremock::matchers::query_param;
        let tasks = |results: Value| ResponseTemplate::new(200).set_body_json(json!({ "count": 1, "next": null, "results": results }));
        let at = || Mock::given(method("GET")).and(path("/pulp/api/v3/tasks/"));
        at().and(query_param("reserved_resources", UP)).respond_with(tasks(json!([]))).mount(server).await;
        at().and(query_param("limit", "1")).respond_with(tasks(json!([{ "reserved_resources_record": record }]))).mount(server).await;
    }

    #[tokio::test]
    async fn replication_history_of_a_pulpcore_whose_runs_reserve_no_upstream_pages_task_groups_by_name() {
        use wiremock::matchers::query_param;
        const GROUPS: &str = "/pulp/api/v3/task-groups/";
        let server = MockServer::start().await;
        // pulpcore 3.95.3: a run reserves the domain's servers, not its Upstream Pulp.
        no_reserved_runs(&server, json!(["pdrn:d:servers", "shared:prn:core.domain:d"])).await;
        let g = |n: usize, d: &str| json!({ "pulp_href": format!("{GROUPS}{n}/"), "description": d, "completed": 1, "failed": 0, "canceled": 0, "tasks": [] });
        let page = |results: Vec<Value>, next: Option<u32>| {
            let next = next.map(|o| format!("{}{GROUPS}?limit=100&offset={o}", server.uri()));
            ResponseTemplate::new(200).set_body_json(json!({ "count": 300, "next": next, "results": results }))
        };
        // task-groups/ has no filters and lists newest first; a busy server has other groups on page 1.
        let at = |o: &str| Mock::given(method("GET")).and(path(GROUPS)).and(query_param("offset", o));
        at("100").respond_with(page((100..104).map(|n| g(n, "Replication of up")).collect(), Some(200))).mount(&server).await;
        at("200").respond_with(page((200..202).map(|n| g(n, "Replication of up")).collect(), Some(300))).mount(&server).await;
        at("300").respond_with(page(vec![g(300, "Replication of up")], None)).mount(&server).await;
        let first = (0..3).map(|n| g(n, "Replication of other")).collect();
        Mock::given(method("GET")).and(path(GROUPS)).respond_with(page(first, Some(100))).mount(&server).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        super::upstream_action(&mut app, crate::tui::kinds::Act::ReplicationHistory, json!({ "pulp_href": UP, "name": "up" }));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Text { lines, .. }) = &app.modal else { panic!("no history: {:?}", notices(&app)) };
        let rows: Vec<&String> = lines.iter().filter(|l| l.starts_with(GROUPS)).collect();
        assert_eq!(rows.len(), 5, "the five newest replications of up: {lines:?}");
        assert!(rows[0].starts_with(&format!("{GROUPS}100/")) && rows[4].starts_with(&format!("{GROUPS}200/")), "{lines:?}");
        assert!(lines.last().unwrap().contains("by the name up"), "the match by name is said: {lines:?}");
        let reqs = server.received_requests().await.unwrap();
        assert!(!reqs.iter().any(|r| r.url.query().unwrap_or_default().contains("offset=300")), "five found: page 4 is not read");
    }

    #[tokio::test]
    async fn replication_history_by_name_reads_at_most_twenty_pages_of_task_groups() {
        const GROUPS: &str = "/pulp/api/v3/task-groups/";
        let server = MockServer::start().await;
        no_reserved_runs(&server, json!(["pdrn:d:servers"])).await;
        let other = json!({ "pulp_href": format!("{GROUPS}1/"), "description": "Replication of other", "tasks": [] });
        let next = format!("{}{GROUPS}?limit=100&offset=100", server.uri());
        let endless = json!({ "count": 9999, "next": next, "results": [other] });
        Mock::given(method("GET")).and(path(GROUPS)).respond_with(ResponseTemplate::new(200).set_body_json(endless)).mount(&server).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        super::upstream_action(&mut app, crate::tui::kinds::Act::ReplicationHistory, json!({ "pulp_href": UP, "name": "up" }));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Text { lines, .. }) = &app.modal else { panic!("no history: {:?}", notices(&app)) };
        assert_eq!(lines, &["no replication of this upstream found (purged tasks are gone)"]);
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.iter().filter(|r| r.url.path() == GROUPS).count(), 20, "at most 2000 groups at 100 a page");
    }

    #[tokio::test]
    async fn replication_history_of_a_never_run_upstream_matches_no_name_when_runs_reserve_their_upstream() {
        const GROUPS: &str = "/pulp/api/v3/task-groups/";
        let server = MockServer::start().await;
        // pulpcore 3.123.1: a run of another Upstream Pulp reserves that one.
        no_reserved_runs(&server, json!(["pdrn:d:servers", "shared:prn:core.upstreampulp:9"])).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        super::upstream_action(&mut app, crate::tui::kinds::Act::ReplicationHistory, json!({ "pulp_href": UP, "name": "up" }));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Text { lines, .. }) = &app.modal else { panic!("no history: {:?}", notices(&app)) };
        assert_eq!(lines, &["no replication of this upstream found (purged tasks are gone)"]);
        let reqs = server.received_requests().await.unwrap();
        assert!(!reqs.iter().any(|r| r.url.path() == GROUPS), "no scan of task-groups/");
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
        // Floats on the server: decimal seconds.
        set(&mut f, "sock_read_timeout", "600.5");
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
            [("remote_policy", json!("on_demand")), ("download_concurrency", json!(4)), ("sock_read_timeout", json!(600.5))],
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

    /// `caps.user` comes from `login/` (newer pulpcore) or `users/?username=` (Basic auth); without both the guard cannot work.
    #[tokio::test]
    async fn user_delete_says_when_the_current_user_is_unknown_and_basic_auth_names_it() {
        use crate::tui::kinds::Act;
        let server = MockServer::start().await;
        let u = "/pulp/api/v3/users/7/";
        serve(&server, "GET", u, 200, json!({ "username": "ann", "groups": [] })).await;
        serve(&server, "GET", &format!("{u}roles/"), 200, page(json!([]))).await;
        // No `login/` and no Basic-auth name (bearer, certificate or no auth): nobody is named.
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        let item = json!({ "pulp_href": u, "username": "ann" });
        super::user_action(&mut app, Act::Delete, item.clone());
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, .. }) = app.modal.take() else { panic!("no confirm: {:?}", notices(&app)) };
        let unknown = "WARNING: cannot tell whether this is your own user (the server does not name the user of this session).";
        assert!(lines.iter().any(|l| l == unknown), "{lines:?}");

        // Basic auth names the user, also when `users/?username=` gave nothing (403, or no such row).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pw"), "s3cret").unwrap();
        let secret = Some(format!("file:{}", dir.path().join("pw").display()));
        let auth = crate::config::AuthMethod::Basic;
        let p = crate::config::Profile { url: server.uri(), auth, username: Some("ann".into()), secret, ..Default::default() };
        app.conn.as_mut().unwrap().client = super::PulpClient::new("lab", &p, &app.cfg.settings).unwrap();
        super::user_action(&mut app, Act::Delete, item);
        assert!(app.modal.is_none() && app.jobs.is_empty(), "refused before any request");
        assert_eq!(notices(&app).last().unwrap().1, "refused: you cannot delete the account you are logged in with");
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

        // A start version: Pulp writes only the files that it lacks (and the metadata of a publication).
        let (mut f, mut submit) = open_form(&mut app, Act::Export, item.clone());
        for (k, v) in [("repo", REPO), ("number", "2"), ("what", "repository version"), ("start", "1")] {
            set(&mut f, k, v);
        }
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        let want = json!({ "repository_version": format!("{REPO}versions/2/"), "start_repository_version": format!("{REPO}versions/1/") });
        assert_eq!(sent(&server).await.pop().unwrap().2, want);
        // A server without the field: not offered.
        let mut caps = crate::pulp::Capabilities::default();
        caps.schemas.insert("FilesystemExport".into(), ["publication", "repository_version"].map(String::from).into());
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        let (f, _) = open_form(&mut app, Act::Export, item);
        assert_eq!(f.fields.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), ["repo", "number", "what"]);
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
        serve(&server, "GET", IMP, 200, item.clone()).await;
        serve(&server, "GET", "/pulp/api/v3/repositories/", 200, page(json!([]))).await; // no snapshot
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
        serve(&server, "GET", IMP, 200, item.clone()).await;
        serve(&server, "GET", "/pulp/api/v3/repositories/", 200, page(json!([]))).await; // no snapshot
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

    /// The check tests the mapping that the import uses: the importer's current one, not the one of a stale list row.
    #[tokio::test]
    async fn the_import_check_uses_the_current_mapping_of_the_importer() {
        use crate::tui::kinds::Act;
        let toc = "/var/lib/pulp/exports/dvd/export-9-20261102_1000-toc.json";
        let stale = json!({ "pulp_href": IMP, "name": "dvd", "repo_mapping": { "web": "web-old" } });
        let server = MockServer::start().await;
        serve(&server, "GET", IMP, 200, json!({ "pulp_href": IMP, "name": "dvd", "repo_mapping": { "web": "web-copy" } })).await;
        serve(&server, "GET", "/pulp/api/v3/repositories/", 200, page(json!([]))).await; // no snapshot
        let check_path = "/pulp/api/v3/importers/core/pulp/import-check/";
        let check = json!({ "toc": { "context": toc, "is_valid": true, "messages": [] },
            "repo_mapping": { "context": "{\"web\":\"web-copy\"}", "is_valid": true, "messages": [] } });
        serve(&server, "POST", check_path, 200, check).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        app.list.kind = Kind::Importers;
        let (mut f, mut submit) = open_form(&mut app, Act::Import, stale);
        set(&mut f, "toc", toc);
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, .. }) = app.modal.take() else { panic!("no confirm: {:?}", notices(&app)) };
        assert!(lines.contains(&"  web -> web-copy".to_string()) && !lines.iter().any(|l| l.contains("web-old")), "{lines:?}");
        assert_eq!(
            sent(&server).await,
            [("POST".into(), check_path.into(), json!({ "toc": toc, "repo_mapping": "{\"web\":\"web-copy\"}" }))]
        );
    }

    /// A `.tar` (an export without chunks) goes as `path`, in the check and in the import.
    #[tokio::test]
    async fn an_import_of_the_export_tar_sends_its_path() {
        use crate::tui::kinds::Act;
        const GROUP: &str = "/pulp/api/v3/task-groups/4/";
        let tar = "/var/lib/pulp/exports/dvd/export-9-20261102_1000.tar";
        let item = json!({ "pulp_href": IMP, "name": "dvd", "repo_mapping": {} });
        let server = MockServer::start().await;
        let check_path = "/pulp/api/v3/importers/core/pulp/import-check/";
        let check = json!({ "path": { "context": tar, "is_valid": true, "messages": [] },
            "repo_mapping": { "context": "{}", "is_valid": true, "messages": [] } });
        serve(&server, "GET", IMP, 200, item.clone()).await;
        serve(&server, "GET", "/pulp/api/v3/repositories/", 200, page(json!([]))).await; // no snapshot
        serve(&server, "POST", check_path, 200, check).await;
        serve(&server, "POST", &format!("{IMP}imports/"), 202, json!({ "task_group": GROUP })).await;
        let group = json!({ "pulp_href": GROUP, "completed": 1,
            "tasks": [{ "name": "pulpcore.app.tasks.importer.pulp_import", "state": "completed", "pulp_href": "/t/1/" }] });
        serve(&server, "GET", GROUP, 200, group).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        app.list.kind = Kind::Importers;
        let (mut f, mut submit) = open_form(&mut app, Act::Import, item.clone());
        set(&mut f, "toc", tar);
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm: {:?}", notices(&app)) };
        assert_eq!(lines[..2], ["path: valid", "repo_mapping: valid"]);
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        assert_eq!(
            sent(&server).await.into_iter().map(|(_, p, b)| (p, b)).collect::<Vec<_>>(),
            [(check_path.to_string(), json!({ "path": tar, "repo_mapping": "{}" })), (format!("{IMP}imports/"), json!({ "path": tar }))]
        );
        // A server without `path`: refused before any request.
        let mut caps = crate::pulp::Capabilities::default();
        caps.schemas.insert("PulpImport".into(), ["toc"].map(String::from).into());
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        let (mut f, mut submit) = open_form(&mut app, Act::Import, item);
        set(&mut f, "toc", tar);
        assert_eq!(submit(&mut app, &f).unwrap_err(), "not supported by this server: import from an export .tar (path)");
    }

    /// A Content View snapshot (`pulp_tui_cv`) is never a mapped destination. Pulp does not say which repositories an
    /// export has before the import, so the confirmation names each snapshot that a same-name import changes, and the
    /// operator types the importer name.
    #[tokio::test]
    async fn an_import_refuses_a_snapshot_destination_and_names_the_snapshots_of_the_same_name() {
        use crate::tui::kinds::Act;
        use wiremock::matchers::query_param;
        const IMP2: &str = "/pulp/api/v3/importers/core/pulp/2/";
        let toc = "/var/lib/pulp/exports/dvd/export-9-20261102_1000-toc.json";
        let labels = json!({ "pulp_tui_cv": "web", "pulp_tui_db": "b" });
        let snap = |n: &str| json!({ "name": n, "pulp_href": format!("/r/{n}/"), "pulp_labels": labels });
        let server = MockServer::start().await;
        let repos = || Mock::given(method("GET")).and(path("/pulp/api/v3/repositories/"));
        let answer = |v: Value| ResponseTemplate::new(200).set_body_json(page(v));
        let os = json!([snap("pulp-tui-cv-web-os")]);
        repos().and(query_param("name", "pulp-tui-cv-web-os")).respond_with(answer(os)).mount(&server).await;
        let both = json!([snap("pulp-tui-cv-web-os"), snap("pulp-tui-cv-web-db")]);
        repos().and(query_param("pulp_label_select", "pulp_tui_cv")).respond_with(answer(both)).mount(&server).await;
        serve(&server, "GET", IMP, 200, json!({ "pulp_href": IMP, "name": "dvd", "repo_mapping": { "os": "pulp-tui-cv-web-os" } })).await;
        serve(&server, "GET", IMP2, 200, json!({ "pulp_href": IMP2, "name": "db", "repo_mapping": { "pulp-tui-cv-web-db": "db" } })).await;
        let check = json!({ "toc": { "context": toc, "is_valid": true, "messages": [] },
            "repo_mapping": { "context": "{}", "is_valid": true, "messages": [] } });
        serve(&server, "POST", "/pulp/api/v3/importers/core/pulp/import-check/", 200, check).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        app.list.kind = Kind::Importers;
        let refused = "refused: pulp-tui-cv-web-os is a snapshot repository of Content View web (database b)";

        // Create (and Edit) with a snapshot destination: refused before the request.
        let (mut f, mut submit) = open_form(&mut app, Act::Create, Value::Null);
        set(&mut f, "name", "dvd");
        set(&mut f, "repo_mapping", "os=pulp-tui-cv-web-os");
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        let n = notices(&app);
        assert!(n.iter().any(|(l, t)| *l == Level::Error && t.contains(refused)), "{n:?}");

        // Import with a mapping made outside pulp-tui: refused before the check.
        let (mut f, mut submit) = open_form(&mut app, Act::Import, json!({ "pulp_href": IMP, "name": "dvd" }));
        set(&mut f, "toc", toc);
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        let n = notices(&app);
        assert!(app.modal.is_none() && n.last().is_some_and(|(l, t)| *l == Level::Error && t.contains(refused)), "{n:?}");
        assert_eq!(sent(&server).await, [], "no create, no check");

        // Unmapped names: the confirmation names the snapshot that a same-name import changes (not the mapped one).
        let (mut f, mut submit) = open_form(&mut app, Act::Import, json!({ "pulp_href": IMP2, "name": "db" }));
        set(&mut f, "toc", toc);
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, typed, .. }) = app.modal.take() else { panic!("no confirm: {:?}", notices(&app)) };
        assert_eq!(typed.as_deref(), Some("db"), "the typed importer name");
        assert!(lines.contains(&"  pulp-tui-cv-web-os (Content View web)".to_string()), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("pulp-tui-cv-web-db (")), "a mapped source does not go by name: {lines:?}");
    }

    const GUARDS: &str = "/pulp/api/v3/contentguards/";
    const RBAC: &str = "/pulp/api/v3/contentguards/core/rbac/1/";
    const HDR: &str = "/pulp/api/v3/contentguards/core/header/2/";
    const COMP: &str = "/pulp/api/v3/contentguards/core/composite/3/";

    /// `distributions/` of all types: docs and web use the RBAC guard, edge the composite, base none.
    async fn guarded(server: &MockServer) {
        let d = |n: &str, g: Option<&str>| json!({ "name": n, "content_guard": g });
        let all = json!({ "count": 4, "next": null, "results": [d("docs", Some(RBAC)), d("base", None), d("web", Some(RBAC)), d("edge", Some(COMP))] });
        serve(server, "GET", "/pulp/api/v3/distributions/", 200, all).await;
    }

    async fn on_guards(server: &MockServer) -> (App, UnboundedReceiver<Msg>) {
        let (mut app, rx) = connected(server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        app.list.kind = Kind::ContentGuards;
        (app, rx)
    }

    /// A copy of the open form (it stays open for `submit_open`).
    fn open_now(app: &App) -> super::Form {
        let Some(Modal::Form { form, .. }) = &app.modal else { panic!("no form: {:?}", notices(app)) };
        form.clone()
    }

    /// The form that the job of `act` opens.
    async fn loaded_form(app: &mut App, rx: &mut UnboundedReceiver<Msg>, act: crate::tui::kinds::Act, item: Value) -> super::Form {
        super::run(app, act, item);
        settle(app, rx).await;
        open_now(app)
    }

    fn submit_open(app: &mut App, f: &super::Form) -> Result<(), String> {
        let Some(Modal::Form { mut submit, .. }) = app.modal.take() else { panic!("no form") };
        submit(app, f)
    }

    #[tokio::test]
    async fn content_guards_list_the_distributions_that_use_each_guard() {
        use wiremock::matchers::query_param;
        let server = MockServer::start().await;
        guarded(&server).await;
        let rows =
            json!({ "count": 2, "next": null, "results": [{ "name": "staff", "pulp_href": RBAC }, { "name": "token", "pulp_href": HDR }] });
        Mock::given(method("GET"))
            .and(path(GUARDS))
            .and(query_param("ordering", "name"))
            .respond_with(ResponseTemplate::new(200).set_body_json(rows))
            .mount(&server)
            .await;
        let (mut app, mut rx) = on_guards(&server).await;
        app.open(crate::tui::app::Screen::List(Kind::ContentGuards));
        settle(&mut app, &mut rx).await;
        let cells: Vec<Vec<String>> =
            app.list.items.iter().map(|r| Kind::ContentGuards.columns().iter().map(|c| (c.get)(r)).collect()).collect();
        assert_eq!(cells, [["staff", "core/rbac", "docs, web"], ["token", "core/header", ""]], "{:?}", app.list.error);
        let reqs = server.received_requests().await.unwrap();
        let q = reqs.iter().find(|r| r.url.path() == "/pulp/api/v3/distributions/").unwrap().url.query().unwrap_or_default().to_string();
        assert!(q.contains("fields=name%2Ccontent_guard"), "only the two fields: {q}");
    }

    #[tokio::test]
    async fn a_content_guard_is_created_with_the_fields_of_its_type() {
        use crate::tui::kinds::Act;
        let server = MockServer::start().await;
        serve(&server, "POST", "/pulp/api/v3/contentguards/core/header/", 201, json!({ "pulp_href": HDR })).await;
        serve(&server, "POST", "/pulp/api/v3/contentguards/certguard/x509/", 201, json!({ "pulp_href": "/x/" })).await;
        let (mut app, mut rx) = on_guards(&server).await;
        let types = [
            "core/rbac",
            "core/header",
            "core/envvar_header",
            "core/content_redirect",
            "core/composite",
            "certguard/x509",
            "certguard/rhsm",
        ];
        let (mut f, mut submit) = open_form(&mut app, Act::Create, Value::Null);
        assert_eq!(f.fields[0].kind, super::FieldKind::Choice(types.map(String::from).to_vec()), "no OpenAPI: every type");
        set(&mut f, "type", "core/header");
        submit(&mut app, &f).unwrap();
        let mut f = open_now(&app);
        assert_eq!(f.title, "New core/header content guard");
        let keys: Vec<&str> = f.fields.iter().map(|x| x.key.as_str()).collect();
        assert_eq!(keys, ["name", "description", "header_name", "header_value", "jq_filter"]);
        assert_eq!(f.fields[3].kind, super::FieldKind::Secret, "the header value is a shared secret");
        set(&mut f, "name", "token");
        assert_eq!(f.validate(), Err("Header name is required".into()));
        set(&mut f, "header_name", "X-Token");
        assert_eq!(f.validate(), Err("Header value is required".into()));
        set(&mut f, "header_value", "s3cret");
        submit_open(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;

        // x509: Pulp takes the PEM text; the form takes the path of a local file.
        let dir = tempfile::tempdir().unwrap();
        let pem = dir.path().join("ca.pem");
        std::fs::write(&pem, "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n").unwrap();
        let (mut f, mut submit) = open_form(&mut app, Act::Create, Value::Null);
        set(&mut f, "type", "certguard/x509");
        submit(&mut app, &f).unwrap();
        let mut f = open_now(&app);
        assert_eq!(f.fields.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), ["name", "description", "ca_certificate"]);
        set(&mut f, "name", "certs");
        set(&mut f, "ca_certificate", pem.to_str().unwrap());
        submit_open(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        assert_eq!(
            sent(&server).await,
            [
                (
                    "POST".into(),
                    "/pulp/api/v3/contentguards/core/header/".into(),
                    json!({ "name": "token", "header_name": "X-Token", "header_value": "s3cret" })
                ),
                (
                    "POST".into(),
                    "/pulp/api/v3/contentguards/certguard/x509/".into(),
                    json!({ "name": "certs", "ca_certificate": "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n" })
                ),
            ],
            "empty optional fields are left out (Pulp refuses an empty string)"
        );

        // Only the types that the server has.
        let mut caps = crate::pulp::Capabilities::default();
        caps.paths.extend(["contentguards/".into(), "contentguards/core/rbac/".into(), "contentguards/certguard/rhsm/".into()]);
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        let (f, _) = open_form(&mut app, Act::Create, Value::Null);
        assert_eq!(f.fields[0].kind, super::FieldKind::Choice(vec!["core/rbac".into(), "certguard/rhsm".into()]));
    }

    #[tokio::test]
    async fn a_content_guard_edit_loads_the_guard_and_sends_only_what_changed() {
        use crate::tui::kinds::Act;
        let server = MockServer::start().await;
        let hdr = json!({ "pulp_href": HDR, "name": "token", "description": "old", "header_name": "X-Token", "header_value": "s3cret", "jq_filter": ".a" });
        serve(&server, "GET", HDR, 200, hdr).await;
        serve(&server, "PATCH", HDR, 200, json!({ "pulp_href": HDR })).await;
        serve(&server, "GET", COMP, 200, json!({ "pulp_href": COMP, "name": "any", "guards": [RBAC, HDR] })).await;
        serve(&server, "PATCH", COMP, 200, json!({ "pulp_href": COMP })).await;
        let (mut app, mut rx) = on_guards(&server).await;

        let mut f = loaded_form(&mut app, &mut rx, Act::Edit, json!({ "pulp_href": HDR, "name": "token" })).await;
        assert_eq!(f.title, "Edit content guard token");
        let shown = ["name", "description", "header_name", "header_value", "jq_filter"].map(|k| f.get(k).to_string());
        assert_eq!(shown, ["token", "old", "X-Token", "", ".a"], "the header value is never shown");
        assert!(f.validate().is_ok(), "an edit keeps the header value when the field stays empty");
        set(&mut f, "header_name", "X-Key");
        set(&mut f, "jq_filter", "");
        submit_open(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;

        // A composite never lists itself as one of its guards.
        let mut f = loaded_form(&mut app, &mut rx, Act::Edit, json!({ "pulp_href": COMP, "name": "any" })).await;
        let g = f.fields.iter().find(|x| x.key == "guards").unwrap();
        assert_eq!(
            (g.value.clone(), g.shown.as_str(), g.exclude.clone()),
            (format!("{RBAC}\n{HDR}"), "2 selected", vec![COMP.to_string()])
        );
        set(&mut f, "guards", RBAC);
        submit_open(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        assert_eq!(
            sent(&server).await,
            [
                ("PATCH".into(), HDR.into(), json!({ "header_name": "X-Key", "jq_filter": null })),
                ("PATCH".into(), COMP.into(), json!({ "guards": [RBAC] })),
            ]
        );
    }

    #[tokio::test]
    async fn deleting_a_content_guard_names_the_distributions_that_become_public() {
        use crate::tui::kinds::Act;
        let server = MockServer::start().await;
        guarded(&server).await;
        // `any` has only the RBAC guard; `both` keeps the header guard.
        let comps = json!({ "count": 2, "next": null, "results": [
            { "name": "any", "pulp_href": COMP, "guards": [RBAC] }, { "name": "both", "pulp_href": "/c/4/", "guards": [HDR, RBAC] }] });
        serve(&server, "GET", "/pulp/api/v3/contentguards/core/composite/", 200, comps).await;
        serve(&server, "DELETE", RBAC, 204, Value::Null).await;
        let (mut app, mut rx) = on_guards(&server).await;

        super::run(&mut app, Act::Delete, json!({ "pulp_href": HDR, "name": "token" }));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { lines, .. }) = app.modal.take() else { panic!("no confirm: {:?}", notices(&app)) };
        assert_eq!(
            lines,
            [
                "Delete content guard token (core/header).",
                "No distribution uses it.",
                "Composite guard both loses it and keeps 1 other guard(s)."
            ]
        );

        super::run(&mut app, Act::Delete, json!({ "pulp_href": RBAC, "name": "staff" }));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { title, lines, typed: Some(typed), on_yes: Some(yes), .. }) = app.modal.take() else {
            panic!("no confirm")
        };
        assert_eq!((title.as_str(), typed.as_str()), ("Delete content guard staff", "staff"));
        assert_eq!(
            lines,
            [
                "Delete content guard staff (core/rbac).",
                "Pulp removes the guard from 2 distribution(s): they become PUBLIC, anyone can download their content:",
                "  docs",
                "  web",
                "Composite guard any loses its only guard: a composite without guards allows everyone, also on edge",
                "Composite guard both loses it and keeps 1 other guard(s).",
            ]
        );
        assert!(sent(&server).await.is_empty(), "nothing deleted before the confirmation");
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        assert_eq!(sent(&server).await, [("DELETE".into(), RBAC.into(), Value::Null)]);
    }

    #[tokio::test]
    async fn rbac_guard_access_shows_who_may_download_and_sends_the_downloader_role() {
        use crate::tui::kinds::Act;
        let server = MockServer::start().await;
        let detail = json!({ "pulp_href": RBAC, "name": "staff", "users": [{ "username": "admin" }, { "username": "alice" }], "groups": [{ "name": "ops" }] });
        serve(&server, "GET", RBAC, 200, detail).await;
        serve(&server, "POST", &format!("{RBAC}add_role/"), 201, json!({})).await;
        serve(&server, "POST", &format!("{RBAC}remove_role/"), 201, json!({})).await;
        let (mut app, mut rx) = on_guards(&server).await;
        let item = json!({ "pulp_href": RBAC, "name": "staff" });

        let f = loaded_form(&mut app, &mut rx, Act::GuardAccess, item.clone()).await;
        assert_eq!(f.title, "Download access of content guard staff");
        assert_eq!(f.notes, ["May download now: users admin, alice; groups ops"]);
        assert_eq!(f.fields.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), ["change", "users", "groups"]);
        assert_eq!(submit_open(&mut app, &f), Err("pick a user or a group".into()));
        let mut f = loaded_form(&mut app, &mut rx, Act::GuardAccess, item.clone()).await;
        set(&mut f, "users", "bob\ncarol");
        submit_open(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        let mut f = loaded_form(&mut app, &mut rx, Act::GuardAccess, item.clone()).await;
        set(&mut f, "change", "remove");
        set(&mut f, "groups", "ops");
        submit_open(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        let role = "core.rbaccontentguard_downloader";
        assert_eq!(
            sent(&server).await,
            [
                ("POST".into(), format!("{RBAC}add_role/"), json!({ "role": role, "users": ["bob", "carol"], "groups": [] })),
                ("POST".into(), format!("{RBAC}remove_role/"), json!({ "role": role, "users": [], "groups": ["ops"] })),
            ]
        );

        let mut caps = crate::pulp::Capabilities::default();
        caps.paths.insert("contentguards/".into());
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        super::run(&mut app, Act::GuardAccess, item);
        assert!(app.modal.is_none() && app.jobs.is_empty());
        let n = notices(&app);
        assert_eq!(n.last().unwrap().1, "not supported by this server: RBAC guard roles ({r_b_a_c_content_guard_href}add_role/)");
    }

    #[tokio::test]
    async fn read_only_mode_offers_and_runs_no_change_on_the_admin_screens() {
        use crate::config::{AuthMethod, Config, Profile};
        use crate::tui::kinds::Act::*;
        let mut cfg = Config::default();
        let p = Profile { url: "https://pulp.example.com".into(), auth: AuthMethod::None, read_only: true, ..Default::default() };
        cfg.profiles.insert("lab".into(), p);
        let local = Arc::new(crate::store::Store::open_in_memory().await.unwrap());
        let mut app = App::new(cfg, "/dev/null".into(), local, Err(String::new()));
        app.profile = Some("lab".into());
        let rbac = json!({ "pulp_href": RBAC, "name": "staff" });
        app.list.marked.insert(U2.into());
        for (k, item, offered, refused) in [
            (Kind::ContentGuards, rbac, vec![ObjectRoles, Inspect], vec![GuardAccess, Create, Edit, Delete]),
            (Kind::Uploads, json!({ "pulp_href": U1 }), vec![ObjectRoles, Inspect], vec![Delete]),
            (Kind::TaskSchedules, json!({ "pulp_href": "/s/1/", "last_task": "/t/1/" }), vec![WatchTask, ObjectRoles, Inspect], vec![]),
            (Kind::Domains, json!({ "pulp_href": "/d/1/", "name": "edge" }), vec![Inspect], vec![MigrateStorage, Create, Edit, Delete]),
            (Kind::Artifacts, json!({ "pulp_href": "/a/1/", "sha256": "ab" }), vec![Inspect], vec![RepairServer]),
            (
                Kind::AccessPolicies,
                json!({ "pulp_href": "/p/1/", "viewset_name": "tasks", "customized": true }),
                vec![Inspect],
                vec![ResetPolicy],
            ),
        ] {
            app.list.kind = k.clone();
            assert_eq!(app.available_actions(&item), offered, "{k:?}");
            for a in refused {
                crate::tui::actions::run(&mut app, a, item.clone());
                assert!(app.modal.is_none() && app.jobs.is_empty(), "{k:?} {a:?}");
                assert_eq!(notices(&app).last().unwrap().1, "read-only mode: this action changes the server", "{k:?} {a:?}");
            }
        }
    }

    const U1: &str = "/pulp/api/v3/uploads/0190aaaa-0001/";
    const U2: &str = "/pulp/api/v3/uploads/0190aaaa-0002/";
    const U3: &str = "/pulp/api/v3/uploads/0190aaaa-0003/";

    #[tokio::test]
    async fn uploads_delete_the_selected_or_the_marked_ones_after_a_plain_confirmation() {
        use crate::tui::kinds::Act;
        let server = MockServer::start().await;
        for u in [U1, U2, U3] {
            serve(&server, "DELETE", u, 204, Value::Null).await;
        }
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        app.list.kind = Kind::Uploads;
        let item = json!({ "pulp_href": U1, "size": 2048, "pulp_created": "2026-11-02T10:00:00Z" });
        let tail = [
            "Pulp deletes the chunks that a client sent; no content, artifact or repository changes.",
            "A client that still sends such an upload fails and must start again.",
        ];
        super::run(&mut app, Act::Delete, item.clone());
        let Some(Modal::Confirm { lines, typed: None, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no plain confirm") };
        assert_eq!(lines, [&["Delete upload 0190aaaa-0001 (2048 bytes, created 2026-11-02T10:00:00Z)."][..], &tail].concat());
        assert!(sent(&server).await.is_empty(), "nothing deleted before the confirmation");
        yes(&mut app);
        settle(&mut app, &mut rx).await;

        // Marks can be on other pages: one confirmation for all of them, the selected row is not one of them.
        app.list.marked.extend([U2.to_string(), U3.to_string()]);
        super::run(&mut app, Act::Delete, item);
        let Some(Modal::Confirm { lines, typed: None, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no plain confirm") };
        assert_eq!(lines, [&["Delete 2 marked upload(s):", "  0190aaaa-0002", "  0190aaaa-0003"][..], &tail].concat());
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        let mut deleted: Vec<String> = sent(&server).await.into_iter().map(|(m, p, _)| format!("{m} {p}")).collect();
        deleted.sort();
        assert_eq!(deleted, [U1, U2, U3].map(|u| format!("DELETE {u}")));
        assert!(app.list.marked.is_empty());
        assert!(notices(&app).iter().any(|(l, t)| *l == Level::Ok && t == "delete upload 0190aaaa-0003: done"), "{:?}", notices(&app));
    }

    #[tokio::test]
    async fn a_failed_upload_delete_keeps_its_mark() {
        use crate::tui::kinds::Act;
        let server = MockServer::start().await;
        serve(&server, "DELETE", U1, 204, Value::Null).await;
        serve(&server, "DELETE", U2, 403, json!({ "detail": "You do not have permission to perform this action." })).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        app.list.kind = Kind::Uploads;
        app.list.marked.extend([U1.to_string(), U2.to_string()]);
        super::run(&mut app, Act::Delete, json!({ "pulp_href": U1 }));
        let Some(Modal::Confirm { on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm") };
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        assert_eq!(app.list.marked, [U2.to_string()].into(), "only the deleted upload is unmarked");
    }

    #[tokio::test]
    async fn watch_task_on_a_schedule_opens_its_last_task() {
        use crate::tui::kinds::Act;
        let server = MockServer::start().await;
        let (mut app, _rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        app.list.kind = Kind::TaskSchedules;
        super::run(&mut app, Act::WatchTask, json!({ "pulp_href": "/pulp/api/v3/task-schedules/2/", "name": "new", "last_task": null }));
        assert!(app.modal.is_none());
        assert_eq!(notices(&app).last().unwrap(), &(Level::Error, "schedule new has not run a task yet".into()));
        let task = "/pulp/api/v3/tasks/7/";
        super::run(
            &mut app,
            Act::WatchTask,
            json!({ "pulp_href": "/pulp/api/v3/task-schedules/1/", "name": "orphans", "last_task": task }),
        );
        let Some(Modal::Text { title, .. }) = &app.modal else { panic!("no live view: {:?}", notices(&app)) };
        assert_eq!(title, "Watch task orphans");
        assert!(app.tasks.items.iter().any(|t| t.href == task && t.label == "orphans"), "the last task, not the schedule");
    }

    const DOM: &str = "/pulp/api/v3/domains/0190dddd/";
    const FS_CLASS: &str = "pulpcore.app.models.storage.FileSystem";
    const S3_CLASS: &str = "storages.backends.s3.S3Storage";

    async fn on_domains(server: &MockServer) -> (App, UnboundedReceiver<Msg>) {
        let (mut app, rx) = connected(server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        app.list.kind = Kind::Domains;
        (app, rx)
    }

    #[tokio::test]
    async fn a_domain_is_created_with_its_storage_and_edited_without_it() {
        use crate::tui::kinds::Act;
        let server = MockServer::start().await;
        serve(&server, "POST", "/pulp/api/v3/domains/", 201, json!({ "pulp_href": DOM })).await;
        serve(&server, "PATCH", DOM, 202, json!({ "task": "/pulp/api/v3/tasks/1/" })).await;
        let (mut app, mut rx) = on_domains(&server).await;
        let (mut f, mut submit) = open_form(&mut app, Act::Create, Value::Null);
        assert_eq!(f.title, "New domain");
        let keys: Vec<&str> = f.fields.iter().map(|x| x.key.as_str()).collect();
        assert_eq!(
            keys,
            ["name", "description", "storage_class", "storage_settings", "redirect_to_object_storage", "hide_guarded_distributions"]
        );
        let classes = [FS_CLASS, S3_CLASS, "storages.backends.azure_storage.AzureStorage"].map(String::from).to_vec();
        assert_eq!(f.fields[2].kind, super::FieldKind::Choice(classes), "no OpenAPI: the classes that pulpcore offers");
        assert_eq!(f.fields[3].kind, super::FieldKind::Secret, "S3 and Azure settings hold keys");
        assert_eq!((f.get("redirect_to_object_storage"), f.get("hide_guarded_distributions")), ("true", "false"), "Pulp's defaults");
        set(&mut f, "name", "edge");
        set(&mut f, "storage_settings", "[1]");
        assert_eq!(submit(&mut app, &f), Err("Storage settings must be a JSON object".into()));
        set(&mut f, "storage_settings", r#"{"location": "/var/lib/pulp/edge"}"#);
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;

        // Edit takes the row; the storage changes only with a migration (M).
        let row = json!({ "pulp_href": DOM, "name": "edge", "description": "old", "storage_class": FS_CLASS,
            "redirect_to_object_storage": true, "hide_guarded_distributions": false });
        let (mut f, mut submit) = open_form(&mut app, Act::Edit, row);
        assert_eq!(f.title, "Edit domain edge");
        let shown: Vec<(&str, &str)> = f.fields.iter().map(|x| (x.key.as_str(), x.value.as_str())).collect();
        assert_eq!(shown, [("description", "old"), ("redirect_to_object_storage", "true"), ("hide_guarded_distributions", "false")]);
        assert_eq!(submit(&mut app, &f), Err("nothing changed".into()));
        set(&mut f, "description", "");
        set(&mut f, "hide_guarded_distributions", "true");
        submit(&mut app, &f).unwrap();
        settle(&mut app, &mut rx).await;
        let created = json!({ "name": "edge", "storage_class": FS_CLASS, "storage_settings": { "location": "/var/lib/pulp/edge" },
            "redirect_to_object_storage": true, "hide_guarded_distributions": false });
        assert_eq!(
            sent(&server).await,
            [
                ("POST".into(), "/pulp/api/v3/domains/".into(), created),
                ("PATCH".into(), DOM.into(), json!({ "description": null, "hide_guarded_distributions": true })),
            ]
        );
        assert!(app.tasks.items.iter().any(|t| t.href == "/pulp/api/v3/tasks/1/" && t.label == "update domain edge"));

        // Pulp refuses to change the default domain: say so before any request.
        for a in [Act::Edit, Act::Delete, Act::MigrateStorage] {
            super::run(&mut app, a, json!({ "pulp_href": "/pulp/api/v3/domains/0/", "name": "default" }));
            assert!(app.modal.is_none() && app.jobs.is_empty(), "{a:?}");
            assert_eq!(notices(&app).last().unwrap().1, "Pulp does not edit, delete or migrate the default domain", "{a:?}");
        }
    }

    #[tokio::test]
    async fn deleting_a_domain_counts_what_is_in_it_before_the_typed_name() {
        use crate::tui::kinds::Act;
        use wiremock::matchers::query_param;
        let server = MockServer::start().await;
        for (p, n) in [("repositories/", 2), ("remotes/", 1), ("distributions/", 0), ("content/", 5)] {
            Mock::given(method("GET"))
                .and(path(format!("/pulp/edge/api/v3/{p}")))
                .and(query_param("limit", "1"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "count": n, "next": null, "results": [] })))
                .mount(&server)
                .await;
        }
        // Pulp's default policy lets only an administrator list artifacts.
        serve(&server, "GET", "/pulp/edge/api/v3/artifacts/", 403, json!({ "detail": "denied" })).await;
        serve(&server, "DELETE", DOM, 202, json!({ "task": "/pulp/api/v3/tasks/2/" })).await;
        let (mut app, mut rx) = on_domains(&server).await;
        super::run(&mut app, Act::Delete, json!({ "pulp_href": DOM, "name": "edge", "storage_class": S3_CLASS }));
        settle(&mut app, &mut rx).await;
        let Some(Modal::Confirm { title, lines, typed: Some(typed), on_yes: Some(yes), .. }) = app.modal.take() else {
            panic!("no typed confirm: {:?}", notices(&app))
        };
        assert_eq!((title.as_str(), typed.as_str()), ("Delete domain edge", "edge"));
        assert_eq!(
            lines,
            [
                "Delete domain edge (storage S3Storage).",
                "In it, as far as you may see: 2 repositories, 1 remotes, 0 distributions, 5 content units, ? artifacts (you may not list them).",
                "Pulp refuses while a repository version holds content (PLP0007), or while a repository, remote,",
                "distribution or other object is in the domain: delete them first. The task then fails with Pulp's reason.",
                "Else Pulp deletes the orphan content and the artifacts of the domain with their files, and its task records.",
            ]
        );
        assert!(sent(&server).await.is_empty(), "nothing deleted before the confirmation");
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        assert_eq!(sent(&server).await, [("DELETE".into(), DOM.into(), Value::Null)]);
        assert!(app.tasks.items.iter().any(|t| t.href == "/pulp/api/v3/tasks/2/" && t.label == "delete domain edge"));
    }

    #[tokio::test]
    async fn a_storage_migration_posts_to_its_domain_after_the_typed_name() {
        use crate::tui::kinds::Act;
        let server = MockServer::start().await;
        serve(&server, "POST", "/pulp/edge/api/v3/domains/migrate/", 202, json!({ "task": "/pulp/api/v3/tasks/3/" })).await;
        let (mut app, mut rx) = on_domains(&server).await;
        let mut caps = crate::pulp::Capabilities::default();
        caps.load_openapi(&json!({ "paths": { "/pulp/api/v3/domains/": {}, "/pulp/api/v3/domains/migrate/": {} },
            "components": { "schemas": {
                "Domain": { "properties": { "storage_class": { "allOf": [{ "$ref": "#/components/schemas/StorageClassEnum" }] } } },
                "StorageClassEnum": { "enum": [FS_CLASS, S3_CLASS] } } } }));
        app.conn.as_mut().unwrap().caps = Arc::new(caps.clone());
        let row = json!({ "pulp_href": DOM, "name": "edge", "storage_class": FS_CLASS });
        let (mut f, mut submit) = open_form(&mut app, Act::MigrateStorage, row.clone());
        assert_eq!(f.title, "Migrate the storage of domain edge");
        assert_eq!(f.fields.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), ["storage_class", "storage_settings"]);
        assert_eq!(f.fields[0].kind, super::FieldKind::Choice(vec![FS_CLASS.into(), S3_CLASS.into()]), "the classes of the server");
        assert_eq!(f.fields[1].kind, super::FieldKind::Secret);
        set(&mut f, "storage_class", S3_CLASS);
        set(&mut f, "storage_settings", "x");
        assert_eq!(submit(&mut app, &f), Err("Storage settings must be a JSON object".into()));
        let settings = json!({ "bucket_name": "b", "access_key": "a", "secret_key": "s" });
        set(&mut f, "storage_settings", &settings.to_string());
        submit(&mut app, &f).unwrap();
        let Some(Modal::Confirm { title, lines, typed: Some(typed), on_yes: Some(yes), .. }) = app.modal.take() else {
            panic!("no typed confirm: {:?}", notices(&app))
        };
        assert_eq!((title.as_str(), typed.as_str()), ("Migrate the storage of domain edge", "edge"));
        assert_eq!(
            lines,
            [
                "Migrate the storage of domain edge from FileSystem to S3Storage.",
                "Pulp copies every artifact of the domain to the new storage, then the domain uses it.",
                "Until the task ends, the other tasks of the domain wait; content is served from the old storage.",
                "Pulp does not delete the files in the old storage: remove them after the task.",
            ]
        );
        assert!(sent(&server).await.is_empty(), "nothing sent before the confirmation");
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        let want = json!({ "storage_class": S3_CLASS, "storage_settings": settings });
        assert_eq!(sent(&server).await, [("POST".into(), "/pulp/edge/api/v3/domains/migrate/".into(), want)]);
        assert!(app.tasks.items.iter().any(|t| t.href == "/pulp/api/v3/tasks/3/" && t.label == "migrate the storage of domain edge"));

        caps.paths.remove("domains/migrate/");
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        super::run(&mut app, Act::MigrateStorage, row);
        assert!(app.modal.is_none() && app.jobs.is_empty());
        assert_eq!(notices(&app).last().unwrap().1, "not supported by this server: domain storage migration (domains/migrate/)");
    }

    /// The form that the shared action `ObjectRoles` opens for `item` on the current screen.
    async fn roles_form(app: &mut App, rx: &mut UnboundedReceiver<Msg>, item: Value) -> super::Form {
        crate::tui::actions::run(app, crate::tui::kinds::Act::ObjectRoles, item);
        settle(app, rx).await;
        open_now(app)
    }

    #[tokio::test]
    async fn object_roles_show_who_holds_what_and_add_or_remove_a_role() {
        use crate::tui::kinds::Act;
        let server = MockServer::start().await;
        let repo = "/pulp/api/v3/repositories/rpm/rpm/0190abcd/";
        let roles = json!({ "roles": [
            { "role": "rpm.rpmrepository_owner", "users": ["admin"], "groups": [] },
            { "role": "rpm.rpmrepository_viewer", "users": ["bob", "carol"], "groups": ["ops"] }] });
        serve(&server, "GET", &format!("{repo}list_roles/"), 200, roles).await;
        let mine = json!({ "permissions": ["rpm.view_rpmrepository", "rpm.sync_rpmrepository"] });
        serve(&server, "GET", &format!("{repo}my_permissions/"), 200, mine).await;
        for p in ["add_role/", "remove_role/"] {
            serve(&server, "POST", &format!("{repo}{p}"), 201, json!({})).await;
        }
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        app.list.kind = Kind::Repos(RepoType::Rpm, false);
        let item = json!({ "pulp_href": repo, "name": "os" });

        let mut f = roles_form(&mut app, &mut rx, item.clone()).await;
        assert_eq!(f.title, "Roles on os");
        assert_eq!(
            f.notes,
            [
                "rpm.rpmrepository_owner: users admin; groups none",
                "rpm.rpmrepository_viewer: users bob, carol; groups ops",
                "My permissions: rpm.view_rpmrepository, rpm.sync_rpmrepository",
            ]
        );
        assert_eq!(f.fields.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), ["change", "role", "users", "groups"]);
        set(&mut f, "role", "rpm.rpmrepository_viewer");
        assert_eq!(submit_open(&mut app, &f), Err("pick a user or a group".into()));
        let mut f = roles_form(&mut app, &mut rx, item.clone()).await;
        set(&mut f, "role", "rpm.rpmrepository_viewer");
        set(&mut f, "users", "dave");
        submit_open(&mut app, &f).unwrap();
        assert!(app.modal.is_none(), "an ordinary role goes at once");
        settle(&mut app, &mut rx).await;

        // An owner role asks first.
        let mut f = roles_form(&mut app, &mut rx, item.clone()).await;
        set(&mut f, "role", "rpm.rpmrepository_owner");
        set(&mut f, "groups", "ops");
        submit_open(&mut app, &f).unwrap();
        let Some(Modal::Confirm { title, lines, typed: None, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm") };
        assert_eq!(title, "Confirm role change");
        assert_eq!(lines, ["rpm.rpmrepository_owner grants owner/admin level permissions on os."]);
        yes(&mut app);
        settle(&mut app, &mut rx).await;

        // So does a removal from one's own account.
        let conn = app.conn.as_mut().unwrap();
        let mut caps = (*conn.caps).clone();
        caps.user = Some(crate::pulp::caps::CurrentUser { href: "/u/".into(), username: "bob".into(), is_staff: None });
        conn.caps = Arc::new(caps);
        let mut f = roles_form(&mut app, &mut rx, item.clone()).await;
        set(&mut f, "role", "rpm.rpmrepository_viewer");
        set(&mut f, "users", "bob");
        submit_open(&mut app, &f).unwrap();
        assert!(app.modal.is_none(), "a role added to one's own account needs no warning");
        settle(&mut app, &mut rx).await;
        let mut f = roles_form(&mut app, &mut rx, item.clone()).await;
        set(&mut f, "change", "remove");
        set(&mut f, "role", "rpm.rpmrepository_viewer");
        set(&mut f, "users", "bob\ncarol");
        submit_open(&mut app, &f).unwrap();
        let Some(Modal::Confirm { lines, on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirm") };
        assert_eq!(lines, ["You remove a role from YOUR OWN account (bob): you may lose access to os."]);
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        let (add, remove) = (format!("{repo}add_role/"), format!("{repo}remove_role/"));
        assert_eq!(
            sent(&server).await,
            [
                ("POST".into(), add.clone(), json!({ "role": "rpm.rpmrepository_viewer", "users": ["dave"], "groups": [] })),
                ("POST".into(), add.clone(), json!({ "role": "rpm.rpmrepository_owner", "users": [], "groups": ["ops"] })),
                ("POST".into(), add, json!({ "role": "rpm.rpmrepository_viewer", "users": ["bob"], "groups": [] })),
                ("POST".into(), remove, json!({ "role": "rpm.rpmrepository_viewer", "users": ["bob", "carol"], "groups": [] })),
            ]
        );
        assert!(notices(&app).contains(&(Level::Ok, "remove role rpm.rpmrepository_viewer on os: done".into())), "{:?}", notices(&app));

        // Per object type: the server's operation ids.
        let mut caps = crate::pulp::Capabilities::default();
        caps.paths.insert("repositories/rpm/rpm/".into());
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        crate::tui::actions::run(&mut app, Act::ObjectRoles, item);
        assert!(app.modal.is_none() && app.jobs.is_empty());
        assert_eq!(notices(&app).last().unwrap().1, "not supported by this server: object roles of repositories/rpm/rpm/");
    }

    #[tokio::test]
    async fn resetting_an_access_policy_shows_its_statements_and_needs_the_viewset_name() {
        use crate::tui::kinds::Act;
        let server = MockServer::start().await;
        let ap = "/pulp/api/v3/access_policies/0190eeee/";
        serve(&server, "POST", &format!("{ap}reset/"), 200, json!({ "pulp_href": ap, "customized": false })).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        app.list.kind = Kind::AccessPolicies;
        super::run(&mut app, Act::ResetPolicy, json!({ "pulp_href": ap, "viewset_name": "tasks", "customized": false }));
        assert!(app.modal.is_none() && app.jobs.is_empty());
        assert_eq!(notices(&app).last().unwrap().1, "access policy tasks is not customized: it is the default already");

        let statements = json!([{ "action": ["list"], "principal": "authenticated", "effect": "allow" }]);
        let item = json!({ "pulp_href": ap, "viewset_name": "tasks", "customized": true, "statements": statements });
        super::run(&mut app, Act::ResetPolicy, item.clone());
        let Some(Modal::Confirm { title, lines, typed: Some(typed), on_yes: Some(yes), .. }) = app.modal.take() else {
            panic!("no typed confirm: {:?}", notices(&app))
        };
        assert_eq!((title.as_str(), typed.as_str()), ("Reset access policy tasks", "tasks"));
        let head = [
            "Reset the access policy of tasks to the default of its plugin.",
            "Pulp replaces its statements, creation hooks and queryset scoping; the changes are lost.",
            "Users may get or lose access to these objects at once.",
            "Statements now:",
        ];
        assert_eq!(lines, [head.map(String::from).to_vec(), crate::tui::actions::pretty(&statements)].concat());
        assert!(sent(&server).await.is_empty(), "nothing sent before the confirmation");
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        assert_eq!(sent(&server).await, [("POST".into(), format!("{ap}reset/"), json!({}))]);
        assert!(notices(&app).contains(&(Level::Ok, "reset access policy tasks: done".into())), "{:?}", notices(&app));

        let mut caps = crate::pulp::Capabilities::default();
        caps.paths.insert("access_policies/".into());
        app.conn.as_mut().unwrap().caps = Arc::new(caps);
        super::run(&mut app, Act::ResetPolicy, item);
        assert!(app.modal.is_none() && app.jobs.is_empty());
        assert_eq!(notices(&app).last().unwrap().1, "not supported by this server: access policy reset ({access_policy_href}reset/)");
    }

    #[tokio::test]
    async fn read_only_mode_shows_object_roles_and_my_permissions_without_the_change_form() {
        use crate::config::{AuthMethod, Config, Profile};
        use crate::tui::kinds::Act;
        let server = MockServer::start().await;
        serve(&server, "GET", "/pulp/api/v3/status/", 200, json!({ "versions": [] })).await;
        let up = "/pulp/api/v3/uploads/0190aaaa/";
        let roles = json!({ "roles": [{ "role": "core.upload_owner", "users": ["admin"], "groups": [] }] });
        serve(&server, "GET", &format!("{up}list_roles/"), 200, roles).await;
        serve(&server, "GET", &format!("{up}my_permissions/"), 200, json!({ "permissions": ["core.view_upload"] })).await;
        let mut cfg = Config::default();
        let p = Profile { url: server.uri(), auth: AuthMethod::None, read_only: true, ..Default::default() };
        cfg.profiles.insert("lab".into(), p);
        let store = Arc::new(crate::store::Store::open_in_memory().await.unwrap());
        let mut app = App::new(cfg, "/dev/null".into(), store.clone(), Ok(store));
        let mut rx = app.rx.take().unwrap();
        app.connect("lab");
        settle(&mut app, &mut rx).await;
        assert!(app.read_only());
        app.list.kind = Kind::Uploads;
        let item = json!({ "pulp_href": up });
        assert!(app.available_actions(&item).contains(&Act::ObjectRoles), "the view is read-only");
        crate::tui::actions::run(&mut app, Act::ObjectRoles, item);
        settle(&mut app, &mut rx).await;
        let Some(Modal::Text { title, lines, .. }) = &app.modal else { panic!("no view: {:?}", notices(&app)) };
        assert_eq!(title, "Roles on 0190aaaa");
        assert_eq!(
            lines,
            &[
                "core.upload_owner: users admin; groups none",
                "My permissions: core.view_upload",
                "Read-only mode: no role is added or removed here.",
            ]
        );
        assert!(sent(&server).await.is_empty());
    }

    #[tokio::test]
    async fn remove_member_deletes_the_membership_not_the_user() {
        let server = MockServer::start().await;
        let (group, user) = ("/pulp/api/v3/groups/3/", "/pulp/api/v3/users/6/");
        Mock::given(method("DELETE"))
            .and(path(format!("{group}users/6/")))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE")).and(path(user)).respond_with(ResponseTemplate::new(204)).expect(0).mount(&server).await;
        let (mut app, mut rx) = connected(&server, Arc::new(crate::store::Store::open_in_memory().await.unwrap())).await;
        let item = json!({ "pulp_href": user, "username": "ann" });
        super::member_action(&mut app, crate::tui::kinds::Act::Delete, item, group.into(), "ops".into());
        let Some(Modal::Confirm { on_yes: Some(yes), .. }) = app.modal.take() else { panic!("no confirmation") };
        yes(&mut app);
        settle(&mut app, &mut rx).await;
        assert!(notices(&app).iter().any(|(l, t)| *l == Level::Ok && t.contains("remove ann from ops")), "{:?}", notices(&app));
        server.verify().await;
    }
}
