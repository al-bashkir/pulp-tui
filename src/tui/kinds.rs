//! Screen definitions: what each list shows, where it loads from, and which actions it offers.

use super::app::{Ctx, PAGE};
use crate::config::Config;
use crate::filters::format_rules;
use crate::pulp::{Capabilities, Feature, RepoType};
use crate::store::{LIBRARY, Store};
use anyhow::Result;
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Profiles,
    Repos(RepoType),
    Remotes(RepoType),
    /// Versions of one repository (href, name) or of all repositories.
    Versions(Option<(String, String)>),
    /// Content of a repository version; `ctype` indexes `content_types`.
    Content {
        version: String,
        ctype: usize,
    },
    Publications(RepoType),
    Distributions(RepoType),
    ContentViews,
    CvVersions {
        cv: i64,
        name: String,
    },
    Environments,
    UpstreamPulps,
    Users,
    UserRoles {
        user: String,
        name: String,
    },
    Groups,
    GroupUsers {
        group: String,
        name: String,
    },
    GroupRoles {
        group: String,
        name: String,
    },
    Roles,
    Tasks,
    ServerTasks,
}

pub const NAV: &[(&str, Kind)] = &[
    ("Servers", Kind::Profiles),
    ("RPM Repositories", Kind::Repos(RepoType::Rpm)),
    ("File Repositories", Kind::Repos(RepoType::File)),
    ("Remotes", Kind::Remotes(RepoType::Rpm)),
    ("Repository Versions", Kind::Versions(None)),
    ("Publications", Kind::Publications(RepoType::Rpm)),
    ("Distributions", Kind::Distributions(RepoType::Rpm)),
    ("Content Views", Kind::ContentViews),
    ("Lifecycle Envs", Kind::Environments),
    ("Replication", Kind::UpstreamPulps),
    ("Users", Kind::Users),
    ("Groups", Kind::Groups),
    ("Roles", Kind::Roles),
    ("Tasks", Kind::Tasks),
];

/// API collection path, extra query and search parameter of a screen.
type Source = (String, Vec<(&'static str, String)>, Option<&'static str>);

/// (label, API path) of the content types in the content browser.
pub fn content_types(t: RepoType) -> &'static [(&'static str, &'static str)] {
    match t {
        RepoType::Rpm => &[
            ("packages", "content/rpm/packages/"),
            ("advisories", "content/rpm/advisories/"),
            ("modulemds", "content/rpm/modulemds/"),
            ("package groups", "content/rpm/packagegroups/"),
            ("distribution trees", "content/rpm/distribution_trees/"),
            ("repo metadata files", "content/rpm/repo_metadata_files/"),
        ],
        RepoType::File => &[("files", "content/file/files/")],
    }
}

pub struct Col {
    pub title: &'static str,
    pub width: u16,
    pub get: fn(&Value) -> String,
}

fn col(title: &'static str, width: u16, get: fn(&Value) -> String) -> Col {
    Col { title, width, get }
}

pub fn s(v: &Value, k: &str) -> String {
    match v.get(k) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Null) | None => String::new(),
        Some(Value::Bool(b)) => {
            if *b {
                "yes".into()
            } else {
                "no".into()
            }
        }
        Some(x) => x.to_string(),
    }
}

/// Last path segment of an href (short id for display).
pub fn short(href: &str) -> String {
    let t = href.trim_end_matches('/');
    match t.rsplit_once("/versions/") {
        Some((repo, n)) => format!("{}@v{n}", repo.rsplit('/').next().unwrap_or_default().chars().take(8).collect::<String>()),
        None => t.rsplit('/').next().unwrap_or_default().chars().take(13).collect(),
    }
}

fn ts(v: &Value, k: &str) -> String {
    s(v, k).chars().take(19).collect::<String>().replace('T', " ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Create,
    Edit,
    Delete,
    Inspect,
    Sync,
    SyncHistory,
    Versions,
    Content,
    Upload,
    Publish,
    Distribute,
    CopyContent,
    RemoveContent,
    AddToRepo,
    Compare,
    Rollback,
    CopyUrl,
    Verify,
    CancelTask,
    DismissTasks,
    TrackTask,
    Connect,
    TestProfile,
    StoreSecret,
    ExportCv,
    ImportCv,
    CvAddRepo,
    CvFilters,
    CvRemoveRepo,
    CvPreview,
    CvPublish,
    CvVersions,
    Promote,
    CvDrift,
    Resume,
    ToggleProtected,
    Replicate,
    FallbackReplicate,
    ReplicationHistory,
    Password,
    ToggleActive,
    Roles,
    Members,
}

impl Act {
    pub fn key(self) -> char {
        use Act::*;
        match self {
            Create => 'c',
            Edit => 'e',
            Delete => 'd',
            Inspect => 'i',
            Sync => 'f',
            SyncHistory | ReplicationHistory => 'H',
            Versions | CvVersions => 'v',
            Content | Connect | Members => 'o',
            Upload => 'u',
            Publish | CvPublish => 'P',
            Distribute | CvDrift => 'D',
            CopyContent => 'C',
            RemoveContent | CvRemoveRepo => 'x',
            AddToRepo | CvAddRepo | TrackTask => 'a',
            Compare => 'm',
            Rollback => 'b',
            CopyUrl => 'y',
            Verify => 'V',
            CancelTask => 'X',
            DismissTasks => 'z',
            TestProfile => 'T',
            StoreSecret => 'K',
            ExportCv => 'E',
            ImportCv => 'I',
            CvFilters | FallbackReplicate => 'F',
            CvPreview | Password => 'w',
            Promote => 'M',
            Resume | Replicate | Roles => 'R',
            ToggleProtected => 'L',
            ToggleActive => 'A',
        }
    }

    pub fn label(self) -> &'static str {
        use Act::*;
        match self {
            Create => "Create",
            Edit => "Edit",
            Delete => "Delete",
            Inspect => "Inspect (full JSON)",
            Sync => "Sync from remote",
            SyncHistory => "Sync history",
            Versions => "Versions",
            Content => "Browse content",
            Upload => "Upload local file",
            Publish => "Publish",
            Distribute => "Distribute (create/re-point distribution)",
            CopyContent => "Copy content from another repository",
            RemoveContent => "Remove from repository (new version)",
            AddToRepo => "Add to another repository",
            Compare => "Compare with marked version",
            Rollback => "Roll back a distribution to this",
            CopyUrl => "Copy content URL",
            Verify => "Verify served content",
            CancelTask => "Cancel task on server",
            DismissTasks => "Dismiss finished tasks",
            TrackTask => "Track in Task Center",
            Connect => "Connect (use this server)",
            TestProfile => "Test connection",
            StoreSecret => "Store secret in system keyring",
            ExportCv => "Export Content View definitions",
            ImportCv => "Import Content View definitions",
            CvAddRepo => "Add repository",
            CvFilters => "Edit repository filters",
            CvRemoveRepo => "Remove repository",
            CvPreview => "Preview draft vs latest version",
            CvPublish => "Publish new version",
            CvVersions => "Versions",
            Promote => "Promote version to environment",
            CvDrift => "Verify / drift check (all environments)",
            Resume => "Resume / retry publish",
            ToggleProtected => "Toggle protected",
            Replicate => "Replicate now",
            FallbackReplicate => "Fallback: sync from another profile",
            ReplicationHistory => "Last replication task group",
            Password => "Set password",
            ToggleActive => "Enable / disable",
            Roles => "Roles",
            Members => "Members",
        }
    }

    /// Whether the action changes state on the Pulp server (blocked in read-only mode).
    pub fn mutates(self, k: &Kind) -> bool {
        use Act::*;
        match k {
            Kind::Profiles | Kind::Environments => false,
            Kind::ContentViews => matches!(self, Delete | CvPublish | Promote | Rollback),
            Kind::CvVersions { .. } => matches!(self, Delete | Promote | Rollback | Resume),
            _ => !matches!(
                self,
                Inspect
                    | SyncHistory
                    | Versions
                    | Content
                    | Compare
                    | CopyUrl
                    | Verify
                    | DismissTasks
                    | TrackTask
                    | Roles
                    | Members
                    | ReplicationHistory
                    | CvPreview
                    | CvDrift
            ),
        }
    }

    pub fn needs_item(self) -> bool {
        !matches!(self, Act::Create | Act::DismissTasks | Act::ImportCv | Act::ExportCv | Act::FallbackReplicate)
    }
}

impl Kind {
    pub fn title(&self) -> String {
        match self {
            Kind::Profiles => "Servers (profiles)".into(),
            Kind::Repos(t) => format!("{} Repositories", t.label()),
            Kind::Remotes(t) => format!("{} Remotes  [t: toggle type]", t.label()),
            Kind::Versions(Some((_, n))) => format!("Versions of {n}"),
            Kind::Versions(None) => "Repository Versions (all repositories)".into(),
            Kind::Content { version, ctype } => {
                let t = RepoType::from_href(version).unwrap_or(RepoType::File);
                format!("Content {} of {}  [t: type]", content_types(t).get(*ctype).map(|c| c.0).unwrap_or("?"), short(version))
            }
            Kind::Publications(t) => format!("{} Publications  [t: toggle type]", t.label()),
            Kind::Distributions(t) => format!("{} Distributions  [t: toggle type]", t.label()),
            Kind::ContentViews => "Content Views (managed by pulp-tui, not a Pulp API)".into(),
            Kind::CvVersions { name, .. } => format!("Content View {name}: versions"),
            Kind::Environments => "Lifecycle Environments (pulp-tui)".into(),
            Kind::UpstreamPulps => "Replication: Upstream Pulps (native, tech preview)".into(),
            Kind::Users => "Users".into(),
            Kind::UserRoles { name, .. } => format!("Roles of user {name}"),
            Kind::Groups => "Groups".into(),
            Kind::GroupUsers { name, .. } => format!("Members of group {name}"),
            Kind::GroupRoles { name, .. } => format!("Roles of group {name}"),
            Kind::Roles => "Roles".into(),
            Kind::Tasks => "Task Center (tracked tasks)  [t: all server tasks]".into(),
            Kind::ServerTasks => "Server tasks  [t: tracked tasks]".into(),
        }
    }

    pub fn needs_connection(&self) -> bool {
        !matches!(self, Kind::Profiles | Kind::Environments | Kind::ContentViews | Kind::CvVersions { .. } | Kind::Tasks)
    }

    /// Precise message when the server lacks what this screen needs.
    pub fn unsupported(&self, caps: &Capabilities) -> Option<String> {
        let f = match self {
            Kind::Repos(RepoType::Rpm)
            | Kind::Remotes(RepoType::Rpm)
            | Kind::Publications(RepoType::Rpm)
            | Kind::Distributions(RepoType::Rpm) => Feature::Rpm,
            Kind::Repos(RepoType::File)
            | Kind::Remotes(RepoType::File)
            | Kind::Publications(RepoType::File)
            | Kind::Distributions(RepoType::File) => Feature::File,
            Kind::UpstreamPulps => Feature::NativeReplication,
            Kind::Users | Kind::UserRoles { .. } | Kind::Groups | Kind::GroupUsers { .. } | Kind::GroupRoles { .. } => Feature::Users,
            Kind::Roles => Feature::Roles,
            _ => return None,
        };
        caps.require(f).err().map(|e| {
            if f == Feature::NativeReplication {
                format!("{e}. Use the fallback (F) to sync distributions from another server profile.")
            } else {
                e.to_string()
            }
        })
    }

    pub fn toggle(&self) -> Option<Kind> {
        let flip = |t: &RepoType| if *t == RepoType::Rpm { RepoType::File } else { RepoType::Rpm };
        Some(match self {
            Kind::Remotes(t) => Kind::Remotes(flip(t)),
            Kind::Publications(t) => Kind::Publications(flip(t)),
            Kind::Distributions(t) => Kind::Distributions(flip(t)),
            Kind::Content { version, ctype } => {
                let n = content_types(RepoType::from_href(version)?).len();
                Kind::Content { version: version.clone(), ctype: (ctype + 1) % n }
            }
            Kind::Tasks => Kind::ServerTasks,
            Kind::ServerTasks => Kind::Tasks,
            _ => return None,
        })
    }

    /// API collection path and extra query for API-backed screens.
    fn source(&self) -> Option<Source> {
        let name = Some("name__icontains");
        Some(match self {
            Kind::Repos(t) => (t.repositories().into(), vec![("ordering", "name".into())], name),
            Kind::Remotes(t) => (t.remotes().into(), vec![("ordering", "name".into())], name),
            Kind::Versions(Some((repo, _))) => (format!("{repo}versions/"), vec![("ordering", "-number".into())], None),
            Kind::Versions(None) => ("repository_versions/".into(), vec![("ordering", "-pulp_created".into())], None),
            Kind::Content { version, ctype } => {
                let t = RepoType::from_href(version)?;
                let (_, path) = content_types(t)[*ctype];
                let search = match (t, ctype) {
                    (RepoType::Rpm, 0) => Some("name__contains"),
                    (RepoType::File, _) => Some("relative_path__icontains"),
                    _ => None,
                };
                let mut q = vec![("repository_version", version.clone())];
                if (t, *ctype) == (RepoType::Rpm, 0) {
                    q.push(("ordering", "name".into()));
                    q.push((
                        "exclude_fields",
                        "changelogs,files,requires,provides,conflicts,obsoletes,suggests,enhances,recommends,supplements".into(),
                    ));
                }
                (path.into(), q, search)
            }
            Kind::Publications(t) => (t.publications().into(), vec![("ordering", "-pulp_created".into())], None),
            Kind::Distributions(t) => (t.distributions().into(), vec![("ordering", "name".into())], name),
            Kind::UpstreamPulps => ("upstream-pulps/".into(), vec![], name),
            Kind::Users => ("users/".into(), vec![("ordering", "username".into())], Some("username__icontains")),
            Kind::UserRoles { user, .. } => (format!("{user}roles/"), vec![], None),
            Kind::Groups => ("groups/".into(), vec![("ordering", "name".into())], name),
            Kind::GroupUsers { group, .. } => (format!("{group}users/"), vec![], None),
            Kind::GroupRoles { group, .. } => (format!("{group}roles/"), vec![], None),
            Kind::Roles => ("roles/".into(), vec![("ordering", "name".into())], name),
            Kind::ServerTasks => (
                "tasks/".into(),
                vec![("ordering", "-pulp_created".into()), ("exclude_fields", "progress_reports".into())],
                Some("name__contains"),
            ),
            _ => return None,
        })
    }

    pub fn columns(&self) -> Vec<Col> {
        match self {
            Kind::Profiles => vec![
                col("", 2, |v| if v["active"] == true { "*".into() } else { String::new() }),
                col("name", 16, |v| s(v, "name")),
                col("url", 32, |v| s(v, "url")),
                col("user", 10, |v| s(v, "username")),
                col("flags", 16, |v| s(v, "flags")),
            ],
            Kind::Repos(_) => vec![
                col("name", 30, |v| s(v, "name")),
                col("latest", 7, |v| {
                    v["latest_version_href"].as_str().and_then(|h| h.trim_end_matches('/').rsplit('/').next()).unwrap_or("").into()
                }),
                col("remote", 6, |v| if v["remote"].is_string() { "yes".into() } else { String::new() }),
                col("retain", 6, |v| s(v, "retain_repo_versions")),
                col("description", 30, |v| s(v, "description")),
            ],
            Kind::Remotes(_) => {
                vec![col("name", 28, |v| s(v, "name")), col("policy", 10, |v| s(v, "policy")), col("url", 50, |v| s(v, "url"))]
            }
            Kind::Versions(_) => vec![
                col("repo", 10, |v| short(&s(v, "repository"))),
                col("number", 7, |v| s(v, "number")),
                col("created", 19, |v| ts(v, "pulp_created")),
                col("present", 8, |v| sum_counts(&v["content_summary"]["present"])),
                col("+added", 7, |v| sum_counts(&v["content_summary"]["added"])),
                col("-removed", 8, |v| sum_counts(&v["content_summary"]["removed"])),
            ],
            Kind::Content { version, ctype } => match (RepoType::from_href(version), ctype) {
                (Some(RepoType::Rpm), 0) => vec![
                    col("name", 30, |v| s(v, "name")),
                    col("epoch:version-release", 24, |v| {
                        let e = s(v, "epoch");
                        format!(
                            "{}{}-{}",
                            if e.is_empty() || e == "0" { String::new() } else { format!("{e}:") },
                            s(v, "version"),
                            s(v, "release")
                        )
                    }),
                    col("arch", 8, |v| s(v, "arch")),
                    col("sha256", 14, |v| s(v, "sha256").chars().take(12).collect()),
                ],
                (Some(RepoType::Rpm), 1) => vec![
                    col("id", 22, |v| s(v, "id")),
                    col("type", 10, |v| s(v, "type")),
                    col("severity", 9, |v| s(v, "severity")),
                    col("title", 40, |v| s(v, "title")),
                ],
                (Some(RepoType::Rpm), 2) => vec![
                    col("name", 20, |v| s(v, "name")),
                    col("stream", 10, |v| s(v, "stream")),
                    col("version", 20, |v| s(v, "version")),
                    col("context", 10, |v| s(v, "context")),
                    col("arch", 8, |v| s(v, "arch")),
                ],
                (Some(RepoType::Rpm), 3) => vec![col("id", 30, |v| s(v, "id")), col("name", 40, |v| s(v, "name"))],
                (Some(RepoType::File), _) => vec![
                    col("relative_path", 50, |v| s(v, "relative_path")),
                    col("sha256", 20, |v| s(v, "sha256").chars().take(16).collect()),
                ],
                _ => vec![col("href", 70, |v| s(v, "pulp_href")), col("created", 19, |v| ts(v, "pulp_created"))],
            },
            Kind::Publications(_) => vec![
                col("id", 13, |v| short(&s(v, "pulp_href"))),
                col("version", 14, |v| short(&s(v, "repository_version"))),
                col("created", 19, |v| ts(v, "pulp_created")),
                col("distributions", 6, |v| v["distributions"].as_array().map(|a| a.len().to_string()).unwrap_or_default()),
                col("checkpoint", 5, |v| s(v, "checkpoint")),
            ],
            Kind::Distributions(_) => vec![
                col("name", 26, |v| s(v, "name")),
                col("base_path", 26, |v| s(v, "base_path")),
                col("binding", 8, |v| {
                    match (v["publication"].is_string(), v["repository_version"].is_string(), v["repository"].is_string()) {
                        (true, _, _) | (_, true, _) => "pinned".into(),
                        (_, _, true) => "latest".into(),
                        _ => "unbound".into(),
                    }
                }),
                col("target", 14, |v| {
                    short(v["publication"].as_str().or(v["repository_version"].as_str()).or(v["repository"].as_str()).unwrap_or_default())
                }),
            ],
            Kind::ContentViews => vec![
                col("name", 20, |v| s(v, "name")),
                col("repos", 5, |v| v["repos"].as_array().map(|a| a.len().to_string()).unwrap_or_default()),
                col("latest", 7, |v| s(v, "latest")),
                col("environments", 40, |v| s(v, "bound")),
            ],
            Kind::CvVersions { .. } => vec![
                col("version", 7, |v| s(v, "number")),
                col("state", 10, |v| s(v, "state")),
                col("created", 19, |v| ts(v, "created_at")),
                col("environments", 20, |v| s(v, "environments")),
                col("notes", 30, |v| s(v, "notes")),
            ],
            Kind::Environments => vec![
                col("name", 18, |v| s(v, "name")),
                col("after", 14, |v| s(v, "prior")),
                col("protected", 9, |v| s(v, "protected")),
                col("bound views", 40, |v| s(v, "bound")),
            ],
            Kind::UpstreamPulps => vec![
                col("name", 20, |v| s(v, "name")),
                col("base_url", 30, |v| s(v, "base_url")),
                col("policy", 9, |v| s(v, "policy")),
                col("last replication", 19, |v| ts(v, "last_replication")),
            ],
            Kind::Users => vec![
                col("username", 20, |v| s(v, "username")),
                col("staff", 5, |v| s(v, "is_staff")),
                col("active", 6, |v| s(v, "is_active")),
                col("email", 28, |v| s(v, "email")),
            ],
            Kind::UserRoles { .. } | Kind::GroupRoles { .. } => vec![
                col("role", 36, |v| s(v, "role")),
                col("scope", 40, |v| match (v["content_object"].as_str(), v["domain"].as_str()) {
                    (Some(o), _) => format!("object {o}"),
                    (None, Some(d)) => format!("domain {d}"),
                    _ => "global (model-level)".into(),
                }),
            ],
            Kind::Groups => vec![col("name", 30, |v| s(v, "name")), col("id", 8, |v| s(v, "id"))],
            Kind::GroupUsers { .. } => vec![col("username", 30, |v| s(v, "username"))],
            Kind::Roles => vec![
                col("name", 40, |v| s(v, "name")),
                col("locked", 6, |v| s(v, "locked")),
                col("perms", 5, |v| v["permissions"].as_array().map(|a| a.len().to_string()).unwrap_or_default()),
                col("description", 30, |v| s(v, "description")),
            ],
            Kind::Tasks => vec![
                col("state", 10, |v| s(v, "state")),
                col("label", 32, |v| s(v, "label")),
                col("progress", 26, |v| s(v, "progress")),
                col("started", 19, |v| ts(v, "started")),
            ],
            Kind::ServerTasks => vec![
                col("state", 10, |v| s(v, "state")),
                col("name", 44, |v| s(v, "name").trim_start_matches("pulpcore.app.tasks.").to_string()),
                col("created", 19, |v| ts(v, "pulp_created")),
            ],
        }
    }

    pub fn actions(&self) -> Vec<Act> {
        use Act::*;
        match self {
            Kind::Profiles => vec![Connect, TestProfile, Create, Edit, Delete, StoreSecret, ExportCv, ImportCv],
            Kind::Repos(_) => {
                vec![Versions, Content, Sync, SyncHistory, Upload, Publish, Distribute, CopyContent, Create, Edit, Delete, Inspect]
            }
            Kind::Remotes(_) | Kind::Roles => vec![Create, Edit, Delete, Inspect],
            Kind::Versions(_) => vec![Content, Publish, Distribute, Rollback, Compare, Delete, Inspect],
            Kind::Content { .. } => vec![Inspect, RemoveContent, AddToRepo],
            Kind::Publications(_) => vec![Create, Distribute, Delete, Inspect],
            Kind::Distributions(_) => vec![CopyUrl, Verify, Rollback, Create, Edit, Delete, Inspect],
            Kind::ContentViews => vec![
                CvVersions,
                CvPreview,
                CvPublish,
                Promote,
                Rollback,
                CvDrift,
                CvAddRepo,
                CvFilters,
                CvRemoveRepo,
                Create,
                Edit,
                Delete,
                Inspect,
            ],
            Kind::CvVersions { .. } => vec![Promote, Rollback, Compare, Resume, Delete, Inspect],
            Kind::Environments => vec![Create, ToggleProtected, Delete],
            Kind::UpstreamPulps => vec![Replicate, ReplicationHistory, FallbackReplicate, Create, Edit, Delete, Inspect],
            Kind::Users => vec![Roles, Create, Edit, Password, ToggleActive, Delete, Inspect],
            Kind::UserRoles { .. } | Kind::GroupRoles { .. } | Kind::GroupUsers { .. } => vec![Create, Delete],
            Kind::Groups => vec![Members, Roles, Create, Delete, Inspect],
            Kind::Tasks => vec![Inspect, CancelTask, DismissTasks],
            Kind::ServerTasks => vec![Inspect, TrackTask, CancelTask],
        }
    }
}

fn sum_counts(v: &Value) -> String {
    v.as_object().map(|m| m.values().filter_map(|x| x["count"].as_u64()).sum::<u64>().to_string()).unwrap_or_default()
}

/// Load one page of a screen. API screens query Pulp; local screens read the store.
pub async fn load(
    k: &Kind,
    ctx: Option<&Ctx>,
    store: &Store,
    profile: &str,
    cfg: &Config,
    offset: u64,
    filter: &str,
) -> Result<(Vec<Value>, u64)> {
    if let Some((path, mut q, search)) = k.source() {
        let c = &ctx.ok_or_else(|| anyhow::anyhow!("not connected"))?.client;
        q.push(("limit", PAGE.to_string()));
        q.push(("offset", offset.to_string()));
        if let (Some(p), false) = (search, filter.is_empty()) {
            q.push((p, filter.to_string()));
        }
        let href = if path.starts_with('/') { path } else { c.api(&path) };
        let page: crate::pulp::client::Page<Value> = c.list(&href, &q).await?;
        return Ok((page.results, page.count));
    }
    let f = filter.to_lowercase();
    let keep = |v: &Value| f.is_empty() || v.to_string().to_lowercase().contains(&f);
    let items: Vec<Value> = match k {
        Kind::Profiles => cfg
            .profiles
            .iter()
            .map(|(name, p)| {
                let mut flags = vec![];
                if p.production {
                    flags.push("PRODUCTION");
                }
                if p.read_only {
                    flags.push("read-only");
                }
                json!({
                    "id": name, "name": name, "active": name == profile, "url": p.url, "api_root": p.api_root,
                    "domain": p.domain, "auth": p.auth, "username": p.username, "secret_ref": p.secret,
                    "ca_cert": p.ca_cert, "client_cert": p.client_cert, "proxy": p.proxy, "timeout_secs": p.timeout_secs,
                    "read_only": p.read_only, "production": p.production, "flags": flags.join(" "),
                })
            })
            .collect(),
        Kind::ContentViews => {
            let envs = store.environments(profile)?;
            let mut out = vec![];
            for cv in store.content_views(profile)? {
                let versions = store.versions(cv.id)?;
                let numbers: std::collections::HashMap<i64, i64> = versions.iter().map(|v| (v.id, v.number)).collect();
                let bound: Vec<String> = store
                    .bindings_for_cv(cv.id)?
                    .iter()
                    .filter_map(|b| {
                        let e = envs.iter().find(|e| e.id == b.env_id)?;
                        let st = if b.state == "active" { String::new() } else { format!(" ({})", b.state) };
                        Some((
                            envs.iter().position(|x| x.id == e.id),
                            format!("{}:v{}{st}", e.name, numbers.get(&b.version_id).copied().unwrap_or(0)),
                        ))
                    })
                    .collect::<std::collections::BTreeMap<_, _>>()
                    .into_values()
                    .collect();
                let repos: Vec<Value> = store
                    .cv_repos(cv.id)?
                    .into_iter()
                    .map(|r| json!({ "name": r.repo_name, "type": r.repo_type, "href": r.repo_href, "filters": format_rules(&r.filters) }))
                    .collect();
                out.push(json!({
                    "id": cv.id, "name": cv.name, "description": cv.description, "created_at": cv.created_at,
                    "repos": repos, "bound": bound.join("  "),
                    "latest": versions.iter().find(|v| v.state == "published").map(|v| format!("v{}", v.number)),
                    "versions": versions.iter().filter(|v| v.state != "deleted").map(|v| json!({
                        "number": v.number, "state": v.state, "notes": v.notes, "created_at": v.created_at, "error": v.error
                    })).collect::<Vec<_>>(),
                    "audit": store.audit_log(profile, 200)?.into_iter().filter(|a| a.detail.starts_with(&cv.name)).take(10)
                        .map(|a| format!("{} {} {}", a.at, a.action, a.detail)).collect::<Vec<_>>(),
                }));
            }
            out
        }
        Kind::CvVersions { cv, .. } => {
            let envs = store.environments(profile)?;
            let bindings = store.bindings_for_cv(*cv)?;
            store
                .versions(*cv)?
                .into_iter()
                .map(|v| {
                    let in_envs: Vec<String> = bindings
                        .iter()
                        .filter(|b| b.version_id == v.id)
                        .filter_map(|b| envs.iter().find(|e| e.id == b.env_id).map(|e| e.name.clone()))
                        .collect();
                    let repos: Vec<Value> = store
                        .version_repos(v.id)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|r| {
                            json!({
                                "repository": r.repo().repo_name, "type": r.repo().repo_type, "filters": format_rules(&r.repo().filters),
                                "source_version": r.source_version, "snapshot_version": r.derived_version,
                                "publication": r.publication, "units": r.content_count, "pending_task": r.pending_task,
                            })
                        })
                        .collect();
                    json!({
                        "id": v.id, "cv_id": v.cv_id, "number": v.number, "state": v.state, "notes": v.notes,
                        "created_at": v.created_at, "finished_at": v.finished_at, "error": v.error,
                        "environments": in_envs.join(", "), "repositories": repos,
                    })
                })
                .collect()
        }
        Kind::Environments => {
            store.ensure_library(profile)?;
            let envs = store.environments(profile)?;
            let cvs = store.content_views(profile)?;
            envs.iter()
                .map(|e| {
                    let bound: Vec<String> = cvs
                        .iter()
                        .filter_map(|cv| {
                            let b = store.binding(e.id, cv.id).ok()??;
                            let n = store.version(b.version_id).ok()?.number;
                            Some(format!("{}:v{n}", cv.name))
                        })
                        .collect();
                    json!({
                        "id": e.id, "name": e.name, "library": e.name == LIBRARY && e.prior_id.is_none(),
                        "prior": e.prior_id.and_then(|p| envs.iter().find(|x| x.id == p)).map(|x| x.name.clone()).unwrap_or_default(),
                        "protected": e.protected, "bound": bound.join(", "),
                    })
                })
                .collect()
        }
        _ => vec![],
    };
    let items: Vec<Value> = items.into_iter().filter(keep).collect();
    let n = items.len() as u64;
    Ok((items, n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_keys_are_unique_per_screen_and_avoid_list_keys() {
        let reserved = "jkgGhnprsSt q?:N/";
        let kinds = NAV.iter().map(|(_, k)| k.clone()).chain([
            Kind::Versions(Some(("/r/".into(), "r".into()))),
            Kind::Content { version: "/x/rpm/rpm/1/versions/1/".into(), ctype: 0 },
            Kind::CvVersions { cv: 1, name: "x".into() },
            Kind::UserRoles { user: "u".into(), name: "u".into() },
            Kind::GroupUsers { group: "g".into(), name: "g".into() },
            Kind::GroupRoles { group: "g".into(), name: "g".into() },
            Kind::ServerTasks,
        ]);
        for k in kinds {
            let keys: Vec<char> = k.actions().iter().map(|a| a.key()).collect();
            let mut dedup = keys.clone();
            dedup.sort();
            dedup.dedup();
            assert_eq!(keys.len(), dedup.len(), "duplicate action key in {k:?}: {keys:?}");
            assert!(keys.iter().all(|c| !reserved.contains(*c)), "{k:?} uses a reserved key: {keys:?}");
            assert!(!k.columns().is_empty());
        }
    }

    #[test]
    fn read_only_classification() {
        assert!(Act::Delete.mutates(&Kind::Repos(RepoType::Rpm)));
        assert!(!Act::Inspect.mutates(&Kind::Repos(RepoType::Rpm)));
        assert!(!Act::Create.mutates(&Kind::ContentViews), "CV definitions are local");
        assert!(Act::CvPublish.mutates(&Kind::ContentViews));
        assert!(!Act::Delete.mutates(&Kind::Profiles));
    }

    #[test]
    fn short_ids() {
        assert_eq!(short("/pulp/api/v3/repositories/rpm/rpm/0190abcd-1234/versions/3/"), "0190abcd@v3");
        assert_eq!(short("/pulp/api/v3/publications/file/file/0190abcd-1234-5678/"), "0190abcd-1234");
    }
}
