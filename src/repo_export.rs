//! Export and import of repository and remote definitions (`export-repos`, `import-repos`).
//!
//! The file holds the writable fields of each object (the plugin's request schema) and no
//! secret: write-only fields are not readable, and readable fields that can hold a secret
//! (`sles_auth_token`, `headers`, credentials in URLs) are left out. A repository names its
//! remote; other references (signing services) are hrefs of one server and are left out.
//! Objects that Content Views or replication manage are skipped: their owner creates them.
//! Import creates what the server does not have and keeps objects with the same name, so a
//! rerun (also after a crash halfway) ends in the same state. ULN and Git remotes go too; an
//! object without a field that the server requires (a ULN remote's credentials) is not created.

use crate::cv::{LABEL_CV, LABEL_DB, LABEL_ENV};
use crate::pulp::ops::{self, no_progress};
use crate::pulp::{Capabilities, PulpClient, RepoType};
use crate::replication::LABEL_REPLICA;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// Labels of objects that pulp-tui or native replication (`UpstreamPulp`) manage.
const OWNER_LABELS: [&str; 5] = [LABEL_CV, LABEL_ENV, LABEL_DB, LABEL_REPLICA, "UpstreamPulp"];
/// Readable fields that can hold a secret.
const SECRET_FIELDS: [&str; 2] = ["sles_auth_token", "headers"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerExport {
    pub format: u32,
    pub remotes: Vec<Entry>,
    /// The plugin's second remote type (`RepoType::alt_remote`: ULN, Git). An older pulp-tui ignores them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alt_remotes: Vec<Entry>,
    pub repositories: Vec<Entry>,
}

/// One remote or repository. A repository's `remote` field holds the remote's name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    #[serde(rename = "type")]
    pub repo_type: RepoType,
    pub name: String,
    /// Write-only fields that are set on the source (credentials): set them again by hand after an import.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    pub fields: Map<String, Value>,
}

/// The file form of a server object. `writable` tells the fields of the plugin's request schema and
/// `remotes` maps remote hrefs to names. Also returns notes on what was left out.
fn entry(t: RepoType, obj: &Value, writable: &dyn Fn(&str) -> bool, remotes: &BTreeMap<String, String>, api: &str) -> (Entry, Vec<String>) {
    let mut notes = vec![];
    let mut fields = Map::new();
    for (k, v) in obj.as_object().into_iter().flatten().filter(|(k, _)| *k != "name" && writable(k)) {
        let (v, note) = field(k, v, remotes, api);
        notes.extend(note);
        if let Some(v) = v {
            fields.insert(k.clone(), v);
        }
    }
    let secrets = obj["hidden_fields"].as_array().into_iter().flatten().filter(|h| h["is_set"] == true);
    let secrets = secrets.filter_map(|h| h["name"].as_str().map(str::to_string)).collect();
    (Entry { repo_type: t, name: text(obj, "name"), secrets, fields }, notes)
}

/// The file value of one writable field (None = left out), and a note when something was left out.
fn field(k: &str, v: &Value, remotes: &BTreeMap<String, String>, api: &str) -> (Option<Value>, Option<String>) {
    match (k, v) {
        _ if SECRET_FIELDS.contains(&k) => (None, (!v.is_null()).then(|| format!("{k} left out (it can hold a secret)"))),
        ("remote", Value::String(h)) => match remotes.get(h) {
            Some(name) => (Some(name.clone().into()), None),
            None => (None, Some(format!("remote {h} left out (not found)"))),
        },
        ("url" | "proxy_url", Value::String(u)) => match without_credentials(u) {
            Some(clean) => (Some(clean.into()), Some(format!("credentials in {k} left out"))),
            None => (Some(v.clone()), None),
        },
        _ if v.to_string().contains(&format!("\"{api}")) => (None, Some(format!("{k} left out (a reference to an object of this server)"))),
        _ => (Some(v.clone()), None),
    }
}

/// `u` without user and password; None when it has none.
fn without_credentials(u: &str) -> Option<String> {
    let mut url = url::Url::parse(u).ok()?;
    if url.username().is_empty() && url.password().is_none() {
        return None;
    }
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    Some(url.into())
}

fn text(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

/// The create body for `e` on a server whose request schema has the fields `writable`; `remotes` maps the
/// server's remote hrefs to names. Also returns notes on what was left out.
fn create_body(e: &Entry, writable: &dyn Fn(&str) -> bool, remotes: &BTreeMap<String, String>) -> (Value, Vec<String>) {
    let mut notes = vec![];
    let mut body = Map::from_iter([("name".to_string(), Value::from(e.name.clone()))]);
    for (k, v) in &e.fields {
        let v = match (k.as_str(), v) {
            _ if !writable(k) => {
                notes.push(format!("{k} left out (this server does not have it)"));
                continue;
            }
            // "" is "not set" (older servers export it); newer ones refuse it (`package_signing_fingerprint`, minLength 1).
            (_, Value::String(s)) if s.is_empty() => continue,
            ("remote", Value::String(name)) => match remotes.iter().find(|(_, n)| *n == name) {
                Some((h, _)) => h.clone().into(),
                None => {
                    notes.push(format!("remote {name} left out (this server does not have it)"));
                    continue;
                }
            },
            _ => v.clone(),
        };
        body.insert(k.clone(), v);
    }
    // The operator may have added one to the file's fields.
    let unset: Vec<&str> = e.secrets.iter().map(String::as_str).filter(|s| !e.fields.contains_key(*s)).collect();
    if !unset.is_empty() {
        notes.push(format!("set {} again by hand (not in the export)", unset.join(", ")));
    }
    (body.into(), notes)
}

/// The note for an object the server already has: the fields of the file whose value on the server is different.
/// A secret that the operator added to the file is write-only on the server, so it is not compared.
fn kept_note(e: &Entry, server: &Map<String, Value>) -> Option<String> {
    let differs = e.fields.iter().filter(|(k, v)| **v != "" && !e.secrets.contains(k) && server.get(*k) != Some(v));
    let differs: Vec<&str> = differs.map(|(k, _)| k.as_str()).collect();
    (!differs.is_empty()).then(|| format!("exists; kept (differs: {})", differs.join(", ")))
}

/// Refuse a file that the import would stop on halfway: other format, no name, a name twice, a plugin the server lacks.
fn check(caps: &Capabilities, ex: &ServerExport) -> Result<()> {
    if ex.format != 1 {
        bail!("unsupported export format {}", ex.format);
    }
    for (what, entries) in [("remote", &ex.remotes), ("remote", &ex.alt_remotes), ("repository", &ex.repositories)] {
        let mut seen = HashSet::new();
        for e in entries {
            let t = e.repo_type.label();
            if e.name.is_empty() {
                bail!("a {what} in the file has no name");
            }
            if !seen.insert((e.repo_type, &e.name)) {
                bail!("{t} {what} {} is in the file twice", e.name);
            }
            if !caps.supports(e.repo_type.feature()) {
                bail!("{t} {what} {}: this server has no {t} plugin", e.name);
            }
        }
    }
    for e in &ex.alt_remotes {
        let t = e.repo_type.label();
        let Some((label, _, schema)) = e.repo_type.alt_remote() else {
            bail!("{t} remote {} is in alt_remotes, but {t} has no second remote type", e.name)
        };
        if !caps.schemas.contains_key(schema) {
            bail!("{t} {label} remote {}: this server has no {label} remotes", e.name);
        }
    }
    Ok(())
}

async fn list(c: &PulpClient, collection: &str) -> Result<Vec<Value>> {
    Ok(c.list_all(&c.api(collection), &[("ordering", "name".into())], 100_000).await?)
}

/// Export the repositories and remotes of the plugins the server has. Also returns notes on what was left out.
pub async fn export(c: &PulpClient, caps: &Capabilities) -> Result<(ServerExport, Vec<String>)> {
    let types: Vec<RepoType> = RepoType::ALL.into_iter().filter(|t| caps.supports(t.feature())).collect();
    // Without the request schema every field would pass as writable (`has_field` is optimistic).
    if let Some(s) = types.iter().flat_map(|t| [t.remote_schema(), t.repository_schema()]).find(|s| !caps.schemas.contains_key(*s)) {
        bail!("the server's OpenAPI schema has no {s}; the export needs it to find the writable fields");
    }
    let mut ex = ServerExport { format: 1, remotes: vec![], alt_remotes: vec![], repositories: vec![] };
    let (mut notes, mut managed) = (vec![], 0);
    for t in types {
        // The second remote type (ULN, Git), where the server has it.
        let alt = t.alt_remote().filter(|(_, _, schema)| caps.schemas.contains_key(*schema));
        let remotes = list(c, t.remotes()).await?;
        let alts = match alt {
            Some((_, collection, _)) => list(c, collection).await?,
            None => vec![],
        };
        let names = remotes.iter().chain(&alts).map(|r| (text(r, "pulp_href"), text(r, "name"))).collect();
        let repos = list(c, t.repositories()).await?;
        let (alt_what, alt_schema) = alt.map_or((String::new(), ""), |(label, _, schema)| (format!("{label} remote"), schema));
        for (what, schema, objs, out) in [
            ("remote", t.remote_schema(), &remotes, &mut ex.remotes),
            (alt_what.as_str(), alt_schema, &alts, &mut ex.alt_remotes),
            ("repository", t.repository_schema(), &repos, &mut ex.repositories),
        ] {
            for o in objs {
                if OWNER_LABELS.iter().any(|l| o["pulp_labels"].get(l).is_some()) {
                    managed += 1;
                    continue;
                }
                let (e, left_out) = entry(t, o, &|f| caps.has_field(schema, f), &names, c.api_prefix());
                notes.extend(left_out.into_iter().map(|n| format!("{} {what} {}: {n}", t.label(), e.name)));
                out.push(e);
            }
        }
    }
    if managed > 0 {
        notes.push(format!("{managed} object(s) that Content Views or replication manage left out"));
    }
    Ok((ex, notes))
}

/// Create the remotes, then the repositories, of the file that the server does not have. Returns the
/// objects created, the objects kept (same name on the server) and notes.
pub async fn import(c: &PulpClient, caps: &Capabilities, ex: &ServerExport) -> Result<(usize, usize, Vec<String>)> {
    run(c, caps, ex, true).await
}

/// What `import` would do, without a change: the same counts and notes, and `<object>: create` for each object it creates.
pub async fn plan(c: &PulpClient, caps: &Capabilities, ex: &ServerExport) -> Result<(usize, usize, Vec<String>)> {
    run(c, caps, ex, false).await
}

async fn run(c: &PulpClient, caps: &Capabilities, ex: &ServerExport, apply: bool) -> Result<(usize, usize, Vec<String>)> {
    check(caps, ex)?;
    let (mut created, mut kept, mut notes) = (0, 0, vec![]);
    for t in ex.remotes.iter().chain(&ex.alt_remotes).chain(&ex.repositories).map(|e| e.repo_type).collect::<BTreeSet<_>>() {
        let (cr, kp, n) = import_type(c, caps, t, ex, apply).await?;
        (created, kept) = (created + cr, kept + kp);
        notes.extend(n);
    }
    Ok((created, kept, notes))
}

/// `import` (or with `apply` false, `plan`) for the entries of one plugin type.
async fn import_type(
    c: &PulpClient,
    caps: &Capabilities,
    t: RepoType,
    ex: &ServerExport,
    apply: bool,
) -> Result<(usize, usize, Vec<String>)> {
    let (mut created, mut kept, mut notes) = (0, 0, vec![]);
    // Remote href -> name: the server's remotes (both types) and the ones this import creates.
    let mut remotes = BTreeMap::new();
    let alt = t.alt_remote().filter(|(_, _, schema)| caps.schemas.contains_key(*schema));
    let alt_what = alt.map(|(label, ..)| format!("{label} remote")).unwrap_or_default();
    let mut kinds = vec![("remote", t.remotes(), t.remote_schema(), &ex.remotes)];
    kinds.extend(alt.map(|(_, collection, schema)| (alt_what.as_str(), collection, schema, &ex.alt_remotes)));
    kinds.push(("repository", t.repositories(), t.repository_schema(), &ex.repositories));
    for (what, collection, schema, entries) in kinds {
        let writable = |f: &str| caps.has_field(schema, f);
        let server = list(c, collection).await?;
        if what != "repository" {
            remotes.extend(server.iter().map(|r| (text(r, "pulp_href"), text(r, "name"))));
        }
        for e in entries.iter().filter(|e| e.repo_type == t) {
            let label = format!("{} {what} {}", t.label(), e.name);
            if let Some(o) = server.iter().find(|o| o["name"] == e.name.as_str()) {
                kept += 1;
                notes.extend(kept_note(e, &entry(t, o, &writable, &remotes, c.api_prefix()).0.fields).map(|n| format!("{label} {n}")));
                continue;
            }
            let (body, left_out) = create_body(e, &writable, &remotes);
            // A ULN remote needs its username and password, which no export has.
            let required = caps.required.get(schema).into_iter().flatten().map(String::as_str);
            let missing: Vec<&str> = required.filter(|f| body.get(*f).is_none()).collect();
            if !missing.is_empty() {
                let needs = missing.join(", ");
                notes.push(format!(
                    "{label}: not created: it needs {needs} (not in the export); add them to its fields in the file, or create it by hand"
                ));
                continue;
            }
            let href = if apply {
                ops::create(c, &c.api(collection), &body, collection, &no_progress).await?
            } else {
                notes.push(format!("{label}: create"));
                format!("new:{}", e.name) // a repository of the file finds this remote by name
            };
            if what != "repository" {
                remotes.insert(href, e.name.clone());
            }
            created += 1;
            notes.extend(left_out.into_iter().map(|n| format!("{label}: {n}")));
        }
    }
    Ok((created, kept, notes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const API: &str = "/pulp/api/v3/";

    fn caps() -> Capabilities {
        let mut c = Capabilities::default();
        c.versions.insert("rpm".into(), "3.29.0".into());
        c.load_openapi(&json!({ "paths": {}, "components": { "schemas": {
            "rpm.RpmRemote": { "properties": { "name": {}, "url": {}, "policy": {}, "proxy_url": {}, "headers": {},
                "sles_auth_token": {}, "password": {}, "pulp_labels": {} } },
            "rpm.RpmRepository": { "properties": { "name": {}, "description": {}, "remote": {},
                "metadata_signing_service": {}, "retain_repo_versions": {}, "pulp_labels": {} } },
        } } }));
        c
    }

    fn client(server: &MockServer) -> PulpClient {
        let p = crate::config::Profile { url: server.uri(), auth: crate::config::AuthMethod::None, ..Default::default() };
        PulpClient::new("t", &p, &crate::config::Settings::default()).unwrap()
    }

    async fn serve(server: &MockServer, collection: &str, results: Value) {
        Mock::given(method("GET"))
            .and(path(format!("{API}{collection}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "count": 0, "next": null, "results": results })))
            .mount(server)
            .await;
    }

    fn remote_href(n: u32) -> String {
        format!("{API}remotes/rpm/rpm/{n}/")
    }

    fn rpm(name: &str, fields: Value) -> Entry {
        Entry { repo_type: RepoType::Rpm, name: name.into(), secrets: vec![], fields: fields.as_object().unwrap().clone() }
    }

    #[tokio::test]
    async fn export_holds_writable_fields_without_secrets_and_names_the_remote() {
        let server = MockServer::start().await;
        serve(
            &server,
            "remotes/rpm/rpm/",
            json!([
                { "pulp_href": remote_href(1), "pulp_created": "2026-01-01", "name": "os", "url": "https://u:pw@mirror.example/os/",
                  "policy": "on_demand", "proxy_url": null, "headers": [{ "Authorization": "Bearer x" }], "sles_auth_token": "tok",
                  "pulp_labels": {}, "hidden_fields": [{ "name": "password", "is_set": true }, { "name": "username", "is_set": false }] },
                { "pulp_href": remote_href(2), "name": "nat-os", "url": "http://up/", "pulp_labels": { "UpstreamPulp": "1" } },
            ]),
        )
        .await;
        serve(&server, "repositories/rpm/rpm/", json!([
            { "pulp_href": format!("{API}repositories/rpm/rpm/1/"), "name": "app", "description": "d", "remote": remote_href(1),
              "metadata_signing_service": format!("{API}signing-services/9/"), "retain_repo_versions": null,
              "latest_version_href": "v", "pulp_labels": { "team": "a" } },
            { "pulp_href": format!("{API}repositories/rpm/rpm/2/"), "name": "cv-os", "remote": null, "pulp_labels": { "pulp_tui_cv": "x" } },
        ]))
        .await;
        let (ex, notes) = export(&client(&server), &caps()).await.unwrap();
        assert_eq!(
            serde_json::to_value(&ex).unwrap(),
            json!({ "format": 1,
                "remotes": [{ "type": "rpm", "name": "os", "secrets": ["password"],
                    "fields": { "url": "https://mirror.example/os/", "policy": "on_demand", "proxy_url": null, "pulp_labels": {} } }],
                "repositories": [{ "type": "rpm", "name": "app",
                    "fields": { "description": "d", "remote": "os", "retain_repo_versions": null, "pulp_labels": { "team": "a" } } }] })
        );
        for n in [
            "RPM remote os: credentials in url left out",
            "RPM remote os: headers left out (it can hold a secret)",
            "RPM remote os: sles_auth_token left out (it can hold a secret)",
            "RPM repository app: metadata_signing_service left out (a reference to an object of this server)",
            "2 object(s) that Content Views or replication manage left out",
        ] {
            assert!(notes.iter().any(|x| x == n), "missing note {n:?} in {notes:#?}");
        }
    }

    /// ULN and Git remotes (the second remote type of pulp_rpm and pulp_file) go in `alt_remotes`, without their
    /// credentials; a repository names one. Import creates such a remote only with every field that the server
    /// requires: a ULN remote needs `username` and `password`, which the export never has.
    #[tokio::test]
    async fn uln_and_git_remotes_are_exported_and_imported_when_their_required_fields_are_there() {
        let mut caps = caps();
        caps.versions.insert("file".into(), "1.0.0".into());
        caps.load_openapi(&json!({ "paths": {}, "components": { "schemas": {
            "rpm.UlnRemote": { "required": ["name", "password", "url", "username"], "properties": { "name": {}, "url": {},
                "uln_server_base_url": {}, "username": {}, "password": {}, "pulp_labels": {} } },
            "file.FileRemote": { "required": ["name", "url"], "properties": { "name": {}, "url": {}, "pulp_labels": {} } },
            "file.FileRepository": { "properties": { "name": {}, "remote": {}, "pulp_labels": {} } },
            "file.FileGitRemote": { "required": ["name", "url"], "properties": { "name": {}, "url": {}, "git_ref": {}, "username": {},
                "password": {}, "pulp_labels": {} } },
        } } }));
        let (uln, git) = (format!("{API}remotes/rpm/uln/1/"), format!("{API}remotes/file/git/1/"));
        let up = MockServer::start().await;
        for coll in ["remotes/rpm/rpm/", "remotes/file/file/", "repositories/file/file/"] {
            serve(&up, coll, json!([])).await;
        }
        serve(
            &up,
            "remotes/rpm/uln/",
            json!([{ "pulp_href": uln, "name": "uln-os", "url": "uln://ol8_x86_64_baseos_latest",
            "uln_server_base_url": "https://linux-update.oracle.com/", "pulp_labels": {},
            "hidden_fields": [{ "name": "username", "is_set": true }, { "name": "password", "is_set": true }] }]),
        )
        .await;
        serve(
            &up,
            "remotes/file/git/",
            json!([{ "pulp_href": git, "name": "docs-git", "url": "https://git.example/docs.git",
            "git_ref": "main", "pulp_labels": {}, "hidden_fields": [] }]),
        )
        .await;
        serve(
            &up,
            "repositories/rpm/rpm/",
            json!([{ "pulp_href": format!("{API}repositories/rpm/rpm/1/"), "name": "ol8", "remote": uln,
            "pulp_labels": {} }]),
        )
        .await;
        let (mut ex, _) = export(&client(&up), &caps).await.unwrap();
        assert_eq!(
            serde_json::to_value(&ex).unwrap(),
            json!({ "format": 1, "remotes": [],
                "alt_remotes": [
                    { "type": "rpm", "name": "uln-os", "secrets": ["username", "password"],
                      "fields": { "url": "uln://ol8_x86_64_baseos_latest", "uln_server_base_url": "https://linux-update.oracle.com/",
                                  "pulp_labels": {} } },
                    { "type": "file", "name": "docs-git", "fields": { "url": "https://git.example/docs.git", "git_ref": "main", "pulp_labels": {} } },
                ],
                "repositories": [{ "type": "rpm", "name": "ol8", "fields": { "remote": "uln-os", "pulp_labels": {} } }] })
        );

        let down = MockServer::start().await;
        for coll in [
            "remotes/rpm/rpm/",
            "remotes/rpm/uln/",
            "repositories/rpm/rpm/",
            "remotes/file/file/",
            "remotes/file/git/",
            "repositories/file/file/",
        ] {
            serve(&down, coll, json!([])).await;
        }
        let (create, keep, notes) = plan(&client(&down), &caps, &ex).await.unwrap();
        assert_eq!((create, keep), (2, 0));
        assert_eq!(
            notes,
            [
                "RPM ULN remote uln-os: not created: it needs password, username (not in the export); add them to its fields in the \
                 file, or create it by hand",
                "RPM repository ol8: create",
                "RPM repository ol8: remote uln-os left out (this server does not have it)",
                "File Git remote docs-git: create",
            ]
        );
        // The operator adds the credentials to the file: the import creates the ULN remote, and the repository gets it.
        ex.alt_remotes[0].fields.extend([("username".into(), json!("u")), ("password".into(), json!("p"))]);
        for (coll, href) in [("remotes/rpm/uln/", uln.as_str()), ("repositories/rpm/rpm/", "/r/1/"), ("remotes/file/git/", git.as_str())] {
            Mock::given(method("POST"))
                .and(path(format!("{API}{coll}")))
                .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "pulp_href": href })))
                .mount(&down)
                .await;
        }
        let (created, _, notes) = import(&client(&down), &caps, &ex).await.unwrap();
        assert_eq!((created, notes), (3, vec![]));
        let posts: Vec<(String, Value)> = down
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "POST")
            .map(|r| (r.url.path().to_string(), r.body_json().unwrap()))
            .collect();
        assert_eq!(posts[0].1["username"], "u");
        assert_eq!(posts[1], (format!("{API}repositories/rpm/rpm/"), json!({ "name": "ol8", "remote": uln, "pulp_labels": {} })));
        assert_eq!(posts[2].0, format!("{API}remotes/file/git/"));

        // A target that has the ULN remote already: it is kept (its credentials are not compared), and the repository
        // gets it.
        let has_uln = MockServer::start().await;
        for coll in ["remotes/rpm/rpm/", "repositories/rpm/rpm/", "remotes/file/file/", "remotes/file/git/", "repositories/file/file/"] {
            serve(&has_uln, coll, json!([])).await;
        }
        let existing = json!({ "pulp_href": uln, "name": "uln-os", "url": "uln://ol8_x86_64_baseos_latest",
            "uln_server_base_url": "https://linux-update.oracle.com/", "pulp_labels": {} });
        serve(&has_uln, "remotes/rpm/uln/", json!([existing])).await;
        let (create, keep, notes) = plan(&client(&has_uln), &caps, &ex).await.unwrap();
        assert_eq!(
            (create, keep, notes),
            (2, 1, vec!["RPM repository ol8: create".to_string(), "File Git remote docs-git: create".into()])
        );
    }

    #[tokio::test]
    async fn export_refuses_a_server_without_the_request_schema() {
        let server = MockServer::start().await;
        let mut c = caps();
        c.schemas.remove("rpm.RpmRemote");
        let e = export(&client(&server), &c).await.unwrap_err();
        assert!(e.to_string().contains("rpm.RpmRemote"), "{e}");
    }

    #[tokio::test]
    async fn import_creates_what_is_missing_maps_the_remote_and_keeps_what_exists() {
        let server = MockServer::start().await;
        serve(&server, "remotes/rpm/rpm/", json!([])).await;
        serve(&server, "repositories/rpm/rpm/", json!([
            { "pulp_href": format!("{API}repositories/rpm/rpm/7/"), "name": "kept", "description": "server", "remote": null, "pulp_labels": {} },
        ]))
        .await;
        Mock::given(method("POST"))
            .and(path(format!("{API}remotes/rpm/rpm/")))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "pulp_href": remote_href(5) })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{API}repositories/rpm/rpm/")))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "pulp_href": format!("{API}repositories/rpm/rpm/8/") })))
            .expect(1)
            .mount(&server)
            .await;
        let mut os = rpm("os", json!({ "url": "https://mirror.example/os/", "policy": "on_demand", "pulp_labels": {} }));
        os.secrets = vec!["password".into()];
        let ex = ServerExport {
            format: 1,
            remotes: vec![os],
            alt_remotes: vec![],
            repositories: vec![
                rpm("app", json!({ "description": "d", "remote": "os", "pulp_labels": {} })),
                rpm("kept", json!({ "description": "file", "remote": null, "pulp_labels": {} })),
            ],
        };
        let (created, kept, notes) = import(&client(&server), &caps(), &ex).await.unwrap();
        assert_eq!((created, kept), (2, 1));
        assert_eq!(
            notes,
            ["RPM remote os: set password again by hand (not in the export)", "RPM repository kept exists; kept (differs: description)"]
        );
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
                json!({ "name": "os", "url": "https://mirror.example/os/", "policy": "on_demand", "pulp_labels": {} }),
                json!({ "name": "app", "description": "d", "remote": remote_href(5), "pulp_labels": {} }),
            ]
        );
    }

    #[tokio::test]
    async fn plan_says_what_import_does_without_a_change() {
        let server = MockServer::start().await;
        serve(&server, "remotes/rpm/rpm/", json!([])).await;
        serve(&server, "repositories/rpm/rpm/", json!([
            { "pulp_href": format!("{API}repositories/rpm/rpm/7/"), "name": "kept", "description": "server", "remote": null, "pulp_labels": {} },
        ]))
        .await;
        let mut os = rpm("os", json!({ "url": "https://mirror.example/os/", "pulp_labels": {} }));
        os.secrets = vec!["password".into()];
        let ex = ServerExport {
            format: 1,
            remotes: vec![os],
            alt_remotes: vec![],
            repositories: vec![
                rpm("app", json!({ "description": "d", "remote": "os", "pulp_labels": {} })),
                rpm("kept", json!({ "description": "file", "remote": null, "pulp_labels": {} })),
            ],
        };
        let (create, keep, notes) = plan(&client(&server), &caps(), &ex).await.unwrap();
        assert_eq!((create, keep), (2, 1));
        assert_eq!(
            notes,
            [
                "RPM remote os: create",
                "RPM remote os: set password again by hand (not in the export)",
                "RPM repository app: create",
                "RPM repository kept exists; kept (differs: description)",
            ],
            "the repository finds the remote that the import creates first"
        );
        assert!(server.received_requests().await.unwrap().iter().all(|r| r.method.as_str() == "GET"), "nothing is sent");
        let refused = ServerExport { format: 2, ..ex };
        assert_eq!(plan(&client(&server), &caps(), &refused).await.unwrap_err().to_string(), "unsupported export format 2");
    }

    #[test]
    fn field_leaves_out_secrets_unknown_remotes_and_nested_references() {
        let f = |k: &str, v: Value| field(k, &v, &BTreeMap::new(), API);
        assert_eq!(f("sles_auth_token", Value::Null), (None, None), "an unset secret needs no note");
        assert_eq!(f("remote", json!(remote_href(9))), (None, Some(format!("remote {} left out (not found)", remote_href(9)))));
        assert_eq!(f("url", json!("https://mirror.example/")), (Some(json!("https://mirror.example/")), None));
        let overrides = json!({ "bookworm": format!("{API}signing-services/1/") });
        assert_eq!(f("signing_service_release_overrides", overrides).0, None);
        assert_eq!(f("description", json!("see /pulp/api/v3/ docs")).0, Some(json!("see /pulp/api/v3/ docs")), "text is not an href");
    }

    #[test]
    fn create_body_leaves_out_fields_the_server_lacks_and_a_remote_it_does_not_have() {
        let e = rpm("app", json!({ "description": "d", "layout": "flat", "remote": "gone", "package_signing_fingerprint": "" }));
        let writable = |f: &str| f != "layout";
        let (body, notes) = create_body(&e, &writable, &BTreeMap::new());
        assert_eq!(body, json!({ "name": "app", "description": "d" }), "\"\" is not set: pulp_rpm 3.39 refuses it");
        let server = json!({ "description": "d", "package_signing_fingerprint": null });
        assert_eq!(kept_note(&e, server.as_object().unwrap()), Some("exists; kept (differs: layout, remote)".into()));
        let mut uln = rpm("uln", json!({ "password": "p" }));
        assert_eq!(kept_note(&uln, &Map::new()), Some("exists; kept (differs: password)".into()));
        uln.secrets = vec!["password".into()];
        assert_eq!(kept_note(&uln, &Map::new()), None, "a secret added to the file is not compared");
        assert_eq!(notes, ["layout left out (this server does not have it)", "remote gone left out (this server does not have it)"]);
        let remotes = BTreeMap::from([(remote_href(3), "gone".to_string())]);
        assert_eq!(create_body(&e, &writable, &remotes).0["remote"], json!(remote_href(3)));
    }

    #[test]
    fn check_refuses_a_file_the_import_would_stop_on_halfway() {
        let ok =
            ServerExport { format: 1, remotes: vec![rpm("os", json!({}))], alt_remotes: vec![], repositories: vec![rpm("os", json!({}))] };
        assert!(check(&caps(), &ok).is_ok(), "a remote and a repository can share a name");
        let refused = |ex: ServerExport| check(&caps(), &ex).unwrap_err().to_string();
        assert_eq!(refused(ServerExport { format: 2, ..ok.clone() }), "unsupported export format 2");
        assert_eq!(refused(ServerExport { remotes: vec![rpm("", json!({}))], ..ok.clone() }), "a remote in the file has no name");
        assert_eq!(
            refused(ServerExport { repositories: vec![rpm("a", json!({})), rpm("a", json!({}))], ..ok.clone() }),
            "RPM repository a is in the file twice"
        );
        let deb = Entry { repo_type: RepoType::Deb, ..rpm("d", json!({})) };
        assert_eq!(refused(ServerExport { remotes: vec![deb.clone()], ..ok.clone() }), "Deb remote d: this server has no Deb plugin");
        // The second remote type: only where the plugin has one and the server's OpenAPI has its schema.
        let uln = ServerExport { alt_remotes: vec![rpm("uln", json!({}))], ..ok.clone() };
        assert_eq!(refused(uln), "RPM ULN remote uln: this server has no ULN remotes");
        let mut with_deb = caps();
        with_deb.versions.insert("deb".into(), "3.0.0".into());
        let e = check(&with_deb, &ServerExport { alt_remotes: vec![deb], ..ok }).unwrap_err().to_string();
        assert_eq!(e, "Deb remote d is in alt_remotes, but Deb has no second remote type");
    }
}
