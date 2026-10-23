//! Actions behind the menus: forms, previews, confirmations and background jobs.
//! Content View and administration actions live in `actions_cv` / `actions_admin`.

use super::app::{App, Apply, Ctx, Level, item_key};
use super::form::{Field, FieldKind, Form, Source, format_labels, parse_labels};
use super::kinds::{Act, Kind, s, short};
use crate::config::{AuthMethod, Profile};
use crate::filters::{Selector, parse_rules};
use crate::pulp::client::Page;
use crate::pulp::models::Task;
use crate::pulp::ops;
use crate::pulp::{Capabilities, Mutation, PulpClient, RepoType};
use anyhow::{Context, anyhow, bail};
use serde_json::{Value, json};
use std::future::Future;

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
        ap(move |app| match m {
            Mutation::Task(h) => {
                app.track_task(&h, &l2);
                app.notify(Level::Info, format!("submitted: {l2} (see Tasks)"));
            }
            Mutation::TaskGroup(g) => app.ok(format!("{l2}: task group {g}")),
            _ => {
                app.ok(format!("{l2}: done"));
                app.reload();
            }
        })
    });
}

pub async fn href_by_name(c: &PulpClient, collection: &str, name: &str) -> anyhow::Result<String> {
    ops::find_by_name::<Value>(c, &c.api(collection), name)
        .await?
        .and_then(|v| v["pulp_href"].as_str().map(str::to_string))
        .ok_or_else(|| anyhow!("no object named {name:?} in {collection}"))
}

/// `/…/repositories/rpm/rpm/<id>/versions/3/` -> repository href.
pub fn repo_of_version(v: &str) -> String {
    v.split("versions/").next().unwrap_or(v).to_string()
}

fn version_of(repo: &str, number: i64) -> String {
    format!("{repo}versions/{number}/")
}

fn caps(app: &App) -> Option<std::sync::Arc<Capabilities>> {
    app.conn.as_ref().map(|c| c.caps.clone())
}

pub fn pretty(v: &Value) -> Vec<String> {
    serde_json::to_string_pretty(v).unwrap_or_default().lines().map(str::to_string).collect()
}

/// OSC 52 clipboard copy: works over SSH and inside containers, no dependency needed.
pub fn copy_to_clipboard(s: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", crate::pulp::client::base64_encode(s.as_bytes()));
    let _ = out.flush();
}

pub fn run(app: &mut App, act: Act, item: Value) {
    let kind = app.list.kind.clone();
    if act.mutates(&kind) && app.read_only() {
        app.error("read-only mode: this action changes the server");
        return;
    }
    match (&kind, act) {
        (_, Act::Inspect) => inspect(app, &kind, item),
        (Kind::Profiles, _) => profile_action(app, act, item),
        (Kind::Repos(t), _) => repo_action(app, *t, act, item),
        (Kind::Remotes(t), _) => remote_action(app, *t, act, item),
        (Kind::Versions(_), _) => version_action(app, act, item),
        (Kind::Content { version, .. }, _) => content_action(app, version.clone(), act),
        (Kind::Publications(t), _) => publication_action(app, *t, act, item),
        (Kind::Distributions(t), _) => distribution_action(app, *t, act, item),
        (Kind::Tasks | Kind::ServerTasks, _) => task_action(app, act, item),
        (Kind::ContentViews | Kind::CvVersions { .. } | Kind::Environments, _) => super::actions_cv::run(app, act, item),
        _ => super::actions_admin::run(app, act, item),
    }
}

fn inspect(app: &mut App, kind: &Kind, item: Value) {
    let href = s(&item, "pulp_href");
    let title = s(&item, "name");
    let title = if title.is_empty() { short(&href) } else { title };
    // Lists may exclude heavy fields; fetch the complete object for inspection.
    if !href.is_empty() && kind.needs_connection() && !matches!(kind, Kind::Tasks) {
        app.job(format!("inspect {title}"), move |ctx| async move {
            let v: Value = ctx.client.get(&href, &[]).await?;
            ap(move |app| app.text(format!("Inspect {title}"), pretty(&v)))
        });
    } else {
        let mut lines = pretty(&item);
        if let Kind::Tasks = kind {
            lines = task_lines(&item);
        }
        app.text(format!("Inspect {title}"), lines);
    }
}

fn task_lines(item: &Value) -> Vec<String> {
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

fn profile_form(name: Option<&str>, p: &Profile) -> Form {
    let auth = ["basic", "bearer", "none"].map(String::from).to_vec();
    let mut fields = vec![];
    if name.is_none() {
        fields.push(Field::text("name", "Profile name").required());
    }
    let opt = |o: &Option<std::path::PathBuf>| o.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
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
        Field::new("timeout_secs", "Timeout (s)", FieldKind::Number).value(p.timeout_secs.to_string()),
        Field::new("read_only", "Read-only", FieldKind::Bool).value(p.read_only.to_string()),
        Field::new("production", "Production (red banner)", FieldKind::Bool).value(p.production.to_string()),
    ]);
    Form::new(if name.is_some() { "Edit server profile" } else { "New server profile" }, fields)
        .note("TLS certificates are always verified. Secrets are referenced, never stored in the config file.")
}

fn profile_from_form(f: &Form) -> Result<Profile, String> {
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
    };
    p.validate().map_err(|e| format!("{e:#}"))?;
    Ok(p)
}

fn save_config(app: &mut App) -> Result<(), String> {
    app.cfg.save(&app.cfg_path).map_err(|e| format!("cannot save {}: {e:#}", app.cfg_path.display()))
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
        Act::Create => app.form(profile_form(None, &Profile::default()), |app, f| {
            let name = f.get("name").to_string();
            crate::store::validate_name(&name).map_err(|e| e.to_string())?;
            if app.cfg.profiles.contains_key(&name) {
                return Err(format!("profile {name} exists"));
            }
            app.cfg.profiles.insert(name.clone(), profile_from_form(f)?);
            if app.cfg.default_profile.is_none() {
                app.cfg.default_profile = Some(name.clone());
            }
            save_config(app)?;
            app.ok(format!("profile {name} saved"));
            app.reload();
            Ok(())
        }),
        Act::Edit => {
            let Some(p) = app.cfg.profiles.get(&name).cloned() else { return };
            app.form(profile_form(Some(&name), &p), move |app, f| {
                app.cfg.profiles.insert(name.clone(), profile_from_form(f)?);
                save_config(app)?;
                app.ok(format!("profile {name} saved (reconnect to apply)"));
                app.reload();
                Ok(())
            });
        }
        Act::Delete => {
            let lines = vec![
                format!("Remove profile {name} from the config file."),
                "Pulp is not changed. Content View definitions stored for this profile stay in the local database.".into(),
            ];
            let n = name.clone();
            app.confirm_danger(format!("Delete profile {name}"), lines, &name, move |app| {
                app.cfg.profiles.remove(&n);
                if app.cfg.default_profile.as_deref() == Some(n.as_str()) {
                    app.cfg.default_profile = None;
                }
                if let Err(e) = save_config(app) {
                    app.error(e);
                }
                app.reload();
            });
        }
        Act::StoreSecret => {
            let form = Form::new(
                format!("Store secret for {name} in the system keyring"),
                vec![Field::new("secret", "Password / token", FieldKind::Secret).required()],
            )
            .note("The profile's secret reference must be `keyring` to use it.");
            app.form(form, move |app, f| {
                let (n, secret) = (name.clone(), f.get("secret").to_string());
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
            let store = app.store.clone();
            app.input(
                format!("Export Content View definitions of {profile} to file"),
                &format!("pulp-tui-{profile}-cv.json"),
                move |app, path| {
                    let fut = async move {
                        let ex = store.export(&profile)?;
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
            let store = app.store.clone();
            app.input(format!("Import Content View definitions into {profile} from file"), "", move |app, path| {
                let fut = async move {
                    let ex: crate::store::Export =
                        serde_json::from_slice(&tokio::fs::read(&path).await.with_context(|| format!("read {path}"))?)?;
                    let (cvs, envs) = store.import(&profile, &ex)?;
                    ap(move |app| app.ok(format!("imported {cvs} content view(s), {envs} environment(s); existing names were kept")))
                };
                app.spawn("import".into(), false, fut);
            });
        }
        _ => {}
    }
}

// ---- repositories --------------------------------------------------------------------

fn repo_form(t: RepoType, caps: Option<&Capabilities>, item: Option<&Value>) -> Form {
    let v = item.cloned().unwrap_or(Value::Null);
    let schema = match t {
        RepoType::Rpm => "rpm.RpmRepository",
        RepoType::File => "file.FileRepository",
    };
    let has = |f: &str| caps.is_none_or(|c| c.has_field(schema, f));
    let mut fields = vec![
        Field::text("name", "Name").required().value(s(&v, "name")),
        Field::text("description", "Description").value(s(&v, "description")),
        Field::pick("remote", "Remote", Source::Remotes(t)).help("default remote for sync; empty = none").value(""),
        Field::new("retain_repo_versions", "Retain versions", FieldKind::Number)
            .value(s(&v, "retain_repo_versions"))
            .help("empty = keep all versions"),
        Field::new("autopublish", "Autopublish", FieldKind::Bool).value(if v["autopublish"] == true { "true" } else { "false" }),
        Field::text("labels", "Labels (k=v,...)").value(format_labels(&v["pulp_labels"])),
    ];
    if t == RepoType::Rpm {
        if has("retain_package_versions") {
            fields.push(
                Field::new("retain_package_versions", "Retain package versions", FieldKind::Number)
                    .value(s(&v, "retain_package_versions"))
                    .help("0 = keep all"),
            );
        }
        if has("checksum_type") {
            let mut c: Vec<String> = caps.and_then(|c| c.enum_values("PackageChecksumTypeEnum")).map(|e| e.to_vec()).unwrap_or_default();
            c.retain(|x| !matches!(x.as_str(), "unknown" | "md5" | "sha1"));
            c.insert(0, String::new());
            fields.push(Field::new("checksum_type", "Checksum type", FieldKind::Choice(c)).value(s(&v, "checksum_type")));
        }
        if has("metadata_signing_service") {
            fields.push(
                Field::pick("metadata_signing_service", "Metadata signing service", Source::SigningServices)
                    .value(s(&v, "metadata_signing_service")),
            );
        }
    }
    if t == RepoType::File && has("manifest") {
        fields.push(Field::text("manifest", "Manifest file name").value(if v.is_null() {
            "PULP_MANIFEST".into()
        } else {
            s(&v, "manifest")
        }));
    }
    let mut form =
        Form::new(if item.is_some() { format!("Edit {} repository", t.label()) } else { format!("New {} repository", t.label()) }, fields);
    if let Some(r) = v["remote"].as_str() {
        form = form.note(format!("current remote: {r} (leave Remote empty to keep it)"));
    }
    if v["pulp_labels"].get(crate::cv::LABEL_CV).is_some() {
        form = form.note("This repository is owned by a Content View: retention must stay empty (snapshots would be pruned).");
    }
    form
}

/// JSON body from a repository form; on edit only changed fields are sent.
fn repo_body(f: &Form, edit: bool) -> Result<Value, String> {
    let mut b = json!({});
    for fld in &f.fields {
        if edit && !fld.changed() || fld.key == "remote" {
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

fn repo_action(app: &mut App, t: RepoType, act: Act, item: Value) {
    let href = s(&item, "pulp_href");
    let name = s(&item, "name");
    if let Some(cv) = item["pulp_labels"].get(crate::cv::LABEL_CV).and_then(Value::as_str)
        && matches!(act, Act::Sync | Act::Upload | Act::CopyContent)
    {
        app.error(format!(
            "refused: {name} is a snapshot repository of Content View {cv}; change its source repositories and publish a new version"
        ));
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
        Act::Create | Act::Edit => {
            let edit = act == Act::Edit;
            let form = repo_form(t, caps(app).as_deref(), edit.then_some(&item));
            let owned_by_cv = item["pulp_labels"].get(crate::cv::LABEL_CV).is_some();
            app.form(form, move |app, f| {
                let mut body = repo_body(f, edit)?;
                if owned_by_cv && !body["retain_repo_versions"].is_null() && body.get("retain_repo_versions").is_some() {
                    return Err("refused: retention on a Content View snapshot repository would prune referenced versions".into());
                }
                let remote = f.opt("remote").map(str::to_string);
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
        }
        Act::Delete => {
            app.job(format!("impact of deleting {name}"), move |ctx| async move {
                let c = &ctx.client;
                let pubs: Page<Value> = c.list(&c.api(t.publications()), &[("repository", href.clone()), ("limit", "1".into())]).await?;
                let dists: Page<Value> =
                    c.list(&c.api(t.distributions()), &[("repository", href.clone()), ("limit", "100".into())]).await?;
                let versions: Page<Value> = c.list(&format!("{href}versions/"), &[("limit", "1".into())]).await?;
                let protected = ctx.store.protected_versions(c.profile())?.iter().filter(|v| v.starts_with(&href)).count();
                let mut lines = vec![
                    format!("Deletes repository {name} with all {} versions and {} publications.", versions.count, pubs.count),
                    "Distributions pinned to its publications stop serving content.".into(),
                    format!(
                        "{} distribution(s) follow this repository directly: {}",
                        dists.count,
                        dists.results.iter().map(|d| s(d, "name")).collect::<Vec<_>>().join(", ")
                    ),
                ];
                if protected > 0 {
                    lines.push(format!("WARNING: {protected} version(s) are Content View snapshots (published/promoted views break)."));
                }
                ap(move |app| {
                    app.confirm_danger(format!("Delete repository {name}"), lines, &name.clone(), move |app| {
                        submit(app, format!("delete repository {name}"), move |c| async move { Ok(c.delete(&href).await?) })
                    });
                })
            });
        }
        Act::Sync => {
            let policies: Vec<String> = match t {
                RepoType::Rpm => caps(app)
                    .and_then(|c| c.enum_values("SyncPolicyEnum").map(|e| e.to_vec()))
                    .unwrap_or_else(|| vec!["additive".into(), "mirror_complete".into(), "mirror_content_only".into()]),
                RepoType::File => vec!["additive".into(), "mirror".into()],
            };
            let form = Form::new(
                format!("Sync {name}"),
                vec![
                    Field::pick("remote", "Remote", Source::Remotes(t)).help("empty = the repository's default remote"),
                    Field::new("policy", "Sync policy", FieldKind::Choice(policies)),
                ],
            )
            .note(if item["remote"].is_string() {
                "repository has a default remote"
            } else {
                "repository has NO default remote: pick one"
            });
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
                if f.get("policy").contains("mirror") {
                    let mut lines = vec![
                        "Mirror sync makes the NEW repository version match the upstream exactly:".into(),
                        "content that is not upstream is removed from the new version.".into(),
                        "Older versions are not changed and can still be published (rollback).".into(),
                    ];
                    if f.get("policy") == "mirror_complete" {
                        lines.push("mirror_complete also publishes the upstream metadata unchanged (a publication is created).".into());
                    }
                    app.confirm("Confirm mirror sync", lines, go);
                } else {
                    go(app);
                }
                Ok(())
            });
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
            if t == RepoType::File {
                fields.push(Field::text("relative_path", "Relative path in repository").help("default: file name"));
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
                app.job(format!("publish {name} {}", short(&v)), move |ctx| async move {
                    let p = ops::ensure_publication(&ctx.client, t, &v, &ctx.on_task()).await?;
                    ap(move |app| app.ok(format!("publication {} for {}", short(&p), short(&v))))
                });
            }
        },
        Act::Distribute => distribute_form(app, t, name.clone(), item["latest_version_href"].as_str().map(str::to_string), Some(href)),
        Act::CopyContent => {
            let mut fields = vec![
                Field::pick("source", "Source repository", Source::Repos(Some(t))).required(),
                Field::text("filters", "Filter rules").help(if t == RepoType::Rpm {
                    "e.g. include name=kernel* arch=x86_64; exclude name=*-debuginfo (empty = all)"
                } else {
                    "e.g. include path=docs/* (empty = all)"
                }),
            ];
            if t == RepoType::Rpm {
                fields.push(Field::new("deps", "Copy dependencies", FieldKind::Bool).value("true"));
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
                                Some(ops::packages_in(c, &sv).await?.into_iter().filter(|p| sel.package(p)).map(|p| p.pulp_href).collect())
                            }
                            (RepoType::File, _) => {
                                Some(ops::files_in(c, &sv).await?.into_iter().filter(|x| sel.file(x)).map(|x| x.pulp_href).collect())
                            }
                        };
                        let what = units.as_ref().map(|u| format!("{} unit(s)", u.len())).unwrap_or("all content".into());
                        let lines = vec![
                            format!("Copy {what} from {} into {name}.", short(&sv)),
                            if deps { "RPM dependencies are copied too.".into() } else { String::new() },
                        ];
                        ap(move |app| {
                            app.confirm("Confirm copy", lines, move |app| {
                                submit(app, format!("copy {source} -> {name}"), move |c| async move {
                                    Ok(match t {
                                        RepoType::Rpm => {
                                            let mut cfg = json!({ "source_repo_version": sv, "dest_repo": href });
                                            if let Some(u) = units {
                                                cfg["content"] = u.into();
                                            }
                                            c.post(&c.api("rpm/copy/"), &json!({ "config": [cfg], "dependency_solving": deps })).await?
                                        }
                                        RepoType::File => {
                                            c.post(&format!("{href}modify/"), &json!({ "add_content_units": units.unwrap_or_default() }))
                                                .await?
                                        }
                                    })
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

/// Create or re-point a distribution to the publication of `version` (pinned), or bind it to
/// the repository (follows the latest publication).
fn distribute_form(app: &mut App, t: RepoType, name: String, version: Option<String>, repo: Option<String>) {
    let modes = if repo.is_some() {
        vec!["pin this version".to_string(), "follow repository latest".to_string()]
    } else {
        vec!["pin this version".to_string()]
    };
    let form = Form::new(
        format!("Distribute {name}"),
        vec![
            Field::text("dist", "Distribution name").required().value(name.clone()),
            Field::text("base_path", "Base path").required().value(name.clone()),
            Field::new("mode", "Binding", FieldKind::Choice(modes)),
        ],
    )
    .note("Pinned: serves exactly one immutable publication. Following latest: serves the newest publication (RPM: use autopublish).")
    .note("An existing distribution with this name is re-pointed; base paths of other distributions are never taken over.");
    app.form(form, move |app, f| {
        let (dist, bp, follow) =
            (f.get("dist").to_string(), f.get("base_path").trim_matches('/').to_string(), f.get("mode").starts_with("follow"));
        let (version, repo) = (version.clone(), repo.clone());
        app.job(format!("distribute {dist}"), move |ctx| async move {
            let c = &ctx.client;
            let on = ctx.on_task();
            let href = if follow {
                let repo = repo.ok_or_else(|| anyhow!("no repository"))?;
                match ops::find_by_name::<Value>(c, &c.api(t.distributions()), &dist).await? {
                    Some(d) => {
                        let h = s(&d, "pulp_href");
                        ops::settle(c, c.patch(&h, &json!({ "repository": repo, "publication": null })).await?, &on).await?;
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
                let v = version.ok_or_else(|| anyhow!("repository has no versions"))?;
                let p = ops::ensure_publication(c, t, &v, &on).await?;
                ops::ensure_distribution(c, t, &dist, &bp, Some(&p), &json!({}), &on).await?
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

fn remote_action(app: &mut App, t: RepoType, act: Act, item: Value) {
    let href = s(&item, "pulp_href");
    let name = s(&item, "name");
    match act {
        Act::Create | Act::Edit => {
            let edit = act == Act::Edit;
            let v = if edit { item.clone() } else { Value::Null };
            let caps = caps(app);
            let schema = if t == RepoType::Rpm { "rpm.RpmRemote" } else { "file.FileRemote" };
            let policies = caps
                .as_ref()
                .and_then(|c| c.enum_values("Policy692Enum").map(|e| e.to_vec()))
                .unwrap_or_else(|| vec!["immediate".into(), "on_demand".into(), "streamed".into()]);
            let mut fields = vec![
                Field::text("name", "Name").required().value(s(&v, "name")),
                Field::text("url", "URL").required().value(s(&v, "url")).help(if t == RepoType::File {
                    "URL of the PULP_MANIFEST"
                } else {
                    "yum repository base URL"
                }),
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
                Field::text("ca_cert", "CA cert (PEM file path)").help(if edit { "empty = unchanged" } else { "" }),
                Field::text("client_cert", "Client cert (PEM file path)"),
                Field::text("client_key", "Client key (PEM file path)"),
                Field::text("username", "Username").help("write-only; empty = unchanged"),
                Field::new("password", "Password", FieldKind::Secret),
                Field::text("proxy_url", "Proxy URL").value(s(&v, "proxy_url")),
                Field::text("proxy_username", "Proxy username"),
                Field::new("proxy_password", "Proxy password", FieldKind::Secret),
                Field::new("download_concurrency", "Download concurrency", FieldKind::Number).value(s(&v, "download_concurrency")),
                Field::new("rate_limit", "Rate limit (req/s)", FieldKind::Number).value(s(&v, "rate_limit")),
                Field::text("labels", "Labels (k=v,...)").value(format_labels(&v["pulp_labels"])),
            ];
            if t == RepoType::Rpm && caps.as_ref().is_none_or(|c| c.has_field(schema, "sles_auth_token")) {
                fields.push(Field::new("sles_auth_token", "SLES auth token", FieldKind::Secret));
            }
            let form = Form::new(if edit { format!("Edit remote {name}") } else { format!("New {} remote", t.label()) }, fields);
            app.form(form, move |app, f| {
                let mut b = json!({});
                let mut files = vec![];
                for fld in &f.fields {
                    let val = fld.value.trim().to_string();
                    let secretish = matches!(fld.kind, FieldKind::Secret) || matches!(fld.key.as_str(), "username" | "proxy_username");
                    if (!edit || secretish || fld.key.ends_with("_cert") || fld.key.ends_with("_key")) && val.is_empty() {
                        continue; // write-only: empty keeps the server value
                    }
                    if edit && !fld.changed() && !secretish && !fld.key.ends_with("_cert") && !fld.key.ends_with("_key") {
                        continue;
                    }
                    match (fld.key.as_str(), &fld.kind) {
                        ("labels", _) => b["pulp_labels"] = parse_labels(&val)?,
                        ("ca_cert" | "client_cert" | "client_key", _) => files.push((fld.key.clone(), val)),
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
        app.error("only RPM and File repository versions are managed here");
        return;
    };
    let number = s(&item, "number");
    let repo = repo_of_version(&href);
    match act {
        Act::Content => app.push(Kind::Content { version: href, ctype: 0 }),
        Act::Publish => app.job(format!("publish {}", short(&href)), move |ctx| async move {
            let p = ops::ensure_publication(&ctx.client, t, &href, &ctx.on_task()).await?;
            ap(move |app| app.ok(format!("publication {} for version {number}", short(&p))))
        }),
        Act::Distribute => {
            let name = app.last_repo.as_ref().filter(|r| r.1 == repo).map(|r| r.2.clone()).unwrap_or_else(|| short(&repo));
            distribute_form(app, t, name, Some(href), None);
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
            let other = other.clone();
            app.job("compare versions", move |ctx| async move {
                // Always show older -> newer.
                let num = |h: &str| h.trim_end_matches('/').rsplit('/').next().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
                let (a, b) = if num(&other) <= num(&href) { (other, href) } else { (href, other) };
                let ka = ops::content_keys(&ctx.client, t, Some(&a)).await?;
                let kb = ops::content_keys(&ctx.client, t, Some(&b)).await?;
                let lines = ops::diff(&ka, &kb).lines(&short(&a), &short(&b));
                ap(move |app| app.text("Version comparison", lines))
            });
        }
        Act::Delete => {
            let protected = app.profile.as_ref().and_then(|p| app.store.protected_versions(p).ok()).unwrap_or_default();
            if protected.contains(&href) {
                app.error("refused: this version is a snapshot of a Content View version (delete the CV version instead)");
                return;
            }
            let lines = vec![
                format!("Delete repository version {number} ({}).", short(&href)),
                "Its publications are deleted too; distributions pinned to them stop serving.".into(),
                "Deleting versions is never needed for rollback: re-point a distribution instead.".into(),
            ];
            app.confirm_danger(format!("Delete version {number}"), lines, &format!("v{number}"), move |app| {
                submit(app, format!("delete version {number}"), move |c| async move { Ok(c.delete(&href).await?) })
            });
        }
        _ => {}
    }
}

/// Show what a distribution serves now and what it will serve, then re-point on confirmation.
fn rollback_preview(app: &mut App, t: RepoType, dist: String, version: String) {
    app.job(format!("rollback preview {dist}"), move |ctx| async move {
        let c = &ctx.client;
        let d: Value =
            ops::find_by_name(c, &c.api(t.distributions()), &dist).await?.ok_or_else(|| anyhow!("no {} distribution {dist}", t.label()))?;
        let current = match d["publication"].as_str() {
            Some(p) => {
                let pv: Value = c.get(p, &[]).await?;
                format!("publication {} of {}", short(p), short(&s(&pv, "repository_version")))
            }
            None if d["repository"].is_string() => format!("latest of repository {}", short(&s(&d, "repository"))),
            None => "nothing".into(),
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
        ap(move |app| {
            app.confirm(format!("Roll back {dist}"), lines, move |app| {
                app.job(format!("rollback {dist}"), move |ctx| async move {
                    let c = &ctx.client;
                    let on = ctx.on_task();
                    let p = ops::ensure_publication(c, t, &version, &on).await?;
                    let base = s(&d, "base_path");
                    ops::ensure_distribution(c, t, &dist, &base, Some(&p), &json!({}), &on).await?;
                    let check = ops::verify_served(c, t, &s(&d, "base_url"), &version, "").await;
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
                app.list.marked.clear();
                submit(app, format!("remove {} unit(s)", units.len()), move |c| async move {
                    Ok(c.post(&format!("{repo}modify/"), &json!({ "remove_content_units": units })).await?)
                })
            });
        }
        Act::AddToRepo => {
            let form = Form::new(
                format!("Add {} unit(s) to another {} repository", units.len(), t.label()),
                vec![Field::pick("target", "Target repository", Source::Repos(Some(t))).required()],
            );
            app.form(form, move |app, f| {
                let (target, units) = (f.get("target").to_string(), units.clone());
                app.list.marked.clear();
                submit(app, format!("add {} unit(s) to {target}", units.len()), move |c| async move {
                    let h = href_by_name(&c, t.repositories(), &target).await?;
                    Ok(c.post(&format!("{h}modify/"), &json!({ "add_content_units": units })).await?)
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
            let form = Form::new(
                format!("New {} publication", t.label()),
                vec![
                    Field::pick("repo", "Repository", Source::Repos(Some(t))).required(),
                    Field::new("number", "Version number", FieldKind::Number).help("empty = latest"),
                ],
            );
            app.form(form, move |app, f| {
                let (repo, number) = (f.get("repo").to_string(), f.number("number"));
                app.job(format!("publish {repo}"), move |ctx| async move {
                    let c = &ctx.client;
                    let r: Value =
                        ops::find_by_name(c, &c.api(t.repositories()), &repo).await?.ok_or_else(|| anyhow!("no repository {repo}"))?;
                    let v = match number {
                        Some(n) => version_of(&s(&r, "pulp_href"), n),
                        None => r["latest_version_href"].as_str().ok_or_else(|| anyhow!("no versions"))?.to_string(),
                    };
                    let p = ops::ensure_publication(c, t, &v, &ctx.on_task()).await?;
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
            distribute_form(app, t, short(&repo_of_version(&v)), Some(v), None);
        }
        Act::Delete => {
            let dists = item["distributions"].as_array().map(|a| a.len()).unwrap_or(0);
            let mut lines = vec![format!("Delete publication {}. The repository version is not affected.", short(&href))];
            if dists > 0 {
                lines.push(format!("WARNING: {dists} distribution(s) serve this publication and will stop serving."));
            }
            app.confirm_danger("Delete publication", lines, "delete", move |app| {
                submit(app, "delete publication", move |c| async move { Ok(c.delete(&href).await?) })
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
            let (version, layout, how) = if let Some(p) = d["publication"].as_str() {
                let pv: Value = c.get(p, &[]).await?;
                (s(&pv, "repository_version"), s(&pv, "layout"), format!("pinned to publication {}", short(p)))
            } else if let Some(v) = d["repository_version"].as_str() {
                (v.to_string(), String::new(), "pinned to repository version".into())
            } else if let Some(r) = d["repository"].as_str() {
                let rv: Value = c.get(r, &[]).await?;
                (s(&rv, "latest_version_href"), String::new(), "follows latest of repository".into())
            } else {
                bail!("distribution is not bound to any content");
            };
            let res = ops::verify_served(c, t, &s(&d, "base_url"), &version, &layout).await;
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
            let form = Form::new(
                if edit { format!("Edit distribution {name}") } else { format!("New {} distribution", t.label()) },
                vec![
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
                    Field::pick("content_guard", "Content guard", Source::ContentGuards).value(s(&v, "content_guard")),
                    Field::text("labels", "Labels (k=v,...)").value(format_labels(&v["pulp_labels"])),
                ],
            )
            .note("Changing the base path or binding changes what consumers receive at this URL.");
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
                if repo.is_none() && mode != "none" && !edit {
                    return Err("repository is required".into());
                }
                let href = href.clone();
                let label = format!("{} distribution {}", if edit { "update" } else { "create" }, f.get("name"));
                app.job(label.clone(), move |ctx| async move {
                    let c = &ctx.client;
                    let on = ctx.on_task();
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
                            b["publication"] = ops::ensure_publication(c, t, &v, &on).await?.into();
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
            let lines = vec![format!("Delete distribution {name}."), format!("{} will stop serving content.", s(&item, "base_url"))];
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
    let profile = app.profile.clone().unwrap_or_default();
    app.job("load dashboard", move |ctx: Ctx| async move {
        let c = &ctx.client;
        let caps = crate::pulp::Capabilities::detect(c).await?;
        let count = |path: String, q: Vec<(&'static str, String)>| {
            let c = c.clone();
            async move {
                let mut q = q;
                q.push(("limit", "1".into()));
                q.push(("fields", "pulp_href".into()));
                c.list::<Value>(&c.api(&path), &q)
                    .await
                    .map(|p| p.count.to_string())
                    .unwrap_or_else(|e| format!("n/a ({})", e.status().map(|s| s.to_string()).unwrap_or_default()))
            }
        };
        let mut lines = vec![];
        let st = caps.status.clone();
        if let Some(st) = &st {
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
            (RpmCopy, "RPM copy API"),
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
        lines.push(format!(
            "  RPM repositories   {:>8}   File repositories {:>8}",
            count("repositories/rpm/rpm/".into(), vec![]).await,
            count("repositories/file/file/".into(), vec![]).await
        ));
        lines.push(format!(
            "  RPM distributions  {:>8}   File distributions{:>8}",
            count("distributions/rpm/rpm/".into(), vec![]).await,
            count("distributions/file/file/".into(), vec![]).await
        ));
        lines.push(format!(
            "  RPM packages       {:>8}   File content      {:>8}",
            count("content/rpm/packages/".into(), vec![]).await,
            count("content/file/files/".into(), vec![]).await
        ));
        lines.push(format!(
            "  Tasks running {}  waiting {}  failed (all time) {}",
            count("tasks/".into(), vec![("state", "running".into())]).await,
            count("tasks/".into(), vec![("state", "waiting".into())]).await,
            count("tasks/".into(), vec![("state", "failed".into())]).await
        ));
        let cvs = ctx.store.content_views(&profile)?.len();
        let envs = ctx.store.environments(&profile)?.len();
        lines.push(format!("  Content Views {cvs}   lifecycle environments {envs}   (local to this pulp-tui database)"));
        let caps = std::sync::Arc::new(caps);
        ap(move |app| {
            if let Some(conn) = app.conn.as_mut() {
                conn.caps = caps;
            }
            app.dashboard = lines;
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_body_sends_only_changes_on_edit() {
        let item = json!({ "name": "os", "description": "d", "retain_repo_versions": 5, "autopublish": false, "pulp_labels": {"a": "b"} });
        let mut f = repo_form(RepoType::File, None, Some(&item));
        assert_eq!(repo_body(&f, true).unwrap(), json!({}));
        f.fields.iter_mut().find(|x| x.key == "retain_repo_versions").unwrap().value.clear();
        f.fields.iter_mut().find(|x| x.key == "autopublish").unwrap().value = "true".into();
        assert_eq!(repo_body(&f, true).unwrap(), json!({ "retain_repo_versions": null, "autopublish": true }));
        let new = repo_form(RepoType::Rpm, None, None);
        let mut new = new;
        new.fields[0].value = "x".into();
        let b = repo_body(&new, false).unwrap();
        assert_eq!(b["name"], "x");
        assert!(b.get("description").is_none() && b["retain_repo_versions"].is_null());
    }

    #[test]
    fn version_hrefs() {
        assert_eq!(repo_of_version("/p/repositories/rpm/rpm/1/versions/3/"), "/p/repositories/rpm/rpm/1/");
        assert_eq!(version_of("/p/r/1/", 2), "/p/r/1/versions/2/");
    }
}
