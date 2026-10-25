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
    /// `true` also shows the snapshot repositories of Content Views (label `pulp_tui_cv`).
    Repos(RepoType, bool),
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
    /// This session's requests, actions and notices; `polls` also shows task polling.
    Logs {
        polls: bool,
    },
}

pub const NAV: &[(&str, Kind)] = &[
    ("Servers", Kind::Profiles),
    ("RPM Repositories", Kind::Repos(RepoType::Rpm, false)),
    ("File Repositories", Kind::Repos(RepoType::File, false)),
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
    ("Logs", Kind::Logs { polls: false }),
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
    WatchTask,
    Connect,
    TestProfile,
    StoreSecret,
    ExportCv,
    ImportCv,
    CvAddRepo,
    CvFilters,
    CvRemoveRepo,
    CvAddComponent,
    CvRemoveComponent,
    CvPreview,
    CvPublish,
    CvVersions,
    CvSyncRepos,
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
    BreakLock,
    /// In no `actions()` list: offered on Connect and save; here so that `mutates` classifies them.
    MigrateDb,
    CopyDb,
}

impl Act {
    pub fn key(self) -> char {
        use Act::*;
        match self {
            Create => 'c',
            Edit => 'e',
            Delete => 'd',
            Inspect => 'i',
            Sync | CvSyncRepos => 'f',
            SyncHistory | ReplicationHistory => 'H',
            Versions | CvVersions => 'v',
            Content | Connect | Members => 'o',
            Upload => 'u',
            Publish | CvPublish => 'P',
            Distribute | CvDrift => 'D',
            CopyContent => 'C',
            RemoveContent | CvRemoveRepo | CvRemoveComponent => 'x',
            AddToRepo | CvAddRepo | CvAddComponent | TrackTask => 'a',
            Compare => 'm',
            Rollback => 'b',
            CopyUrl => 'y',
            Verify => 'V',
            CancelTask => 'X',
            DismissTasks => 'z',
            WatchTask => 'W',
            TestProfile => 'T',
            StoreSecret => 'K',
            ExportCv => 'E',
            ImportCv => 'I',
            CvFilters | FallbackReplicate => 'F',
            CvPreview | Password => 'w',
            Promote | MigrateDb => 'M',
            CopyDb => 'Y',
            Resume | Replicate | Roles => 'R',
            ToggleProtected => 'L',
            ToggleActive => 'A',
            BreakLock => 'U',
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
            WatchTask => "Watch task (live)",
            Connect => "Connect (use this server)",
            TestProfile => "Test connection",
            StoreSecret => "Store secret in system keyring",
            ExportCv => "Export Content View definitions",
            ImportCv => "Import Content View definitions",
            CvAddRepo => "Add repositories",
            CvFilters => "Edit repository filters",
            CvRemoveRepo => "Remove repository",
            CvAddComponent => "Add components",
            CvRemoveComponent => "Remove component",
            CvPreview => "Preview draft vs latest version",
            CvPublish => "Publish new version",
            CvVersions => "Versions",
            CvSyncRepos => "Sync repositories",
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
            BreakLock => "Break lock",
            MigrateDb => "Migrate database",
            CopyDb => "Copy Content View data",
        }
    }

    /// Whether the action changes state on the Pulp server or in the shared database (blocked in read-only mode).
    /// `shared`: Content View definitions and environments live in a `[database]` other operators use.
    pub fn mutates(self, k: &Kind, shared: bool) -> bool {
        use Act::*;
        match k {
            // Migrate and copy change a database; import creates Content Views and environments in the shared one.
            Kind::Profiles => matches!(self, MigrateDb | CopyDb) || shared && matches!(self, ImportCv),
            // Edit changes the label, which moves distributions on the server.
            Kind::Environments => shared || matches!(self, Edit),
            Kind::ContentViews => {
                matches!(self, Delete | CvPublish | CvSyncRepos | Promote | Rollback | BreakLock)
                    || shared && matches!(self, Create | Edit | CvAddRepo | CvFilters | CvRemoveRepo | CvAddComponent | CvRemoveComponent)
            }
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
                    | WatchTask
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
            Kind::Repos(t, snapshots) => format!(
                "{} Repositories  [t: show/hide Content View snapshots]{}",
                t.label(),
                if *snapshots { " — snapshots shown" } else { "" }
            ),
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
            Kind::Logs { polls } => {
                format!("Logs (this session, UTC)  [t: show/hide task polling]{}", if *polls { " — polling shown" } else { "" })
            }
        }
    }

    pub fn needs_connection(&self) -> bool {
        !matches!(
            self,
            Kind::Profiles | Kind::Environments | Kind::ContentViews | Kind::CvVersions { .. } | Kind::Tasks | Kind::Logs { .. }
        )
    }

    /// Precise message when the server lacks what this screen needs.
    pub fn unsupported(&self, caps: &Capabilities) -> Option<String> {
        let f = match self {
            Kind::Repos(RepoType::Rpm, _)
            | Kind::Remotes(RepoType::Rpm)
            | Kind::Publications(RepoType::Rpm)
            | Kind::Distributions(RepoType::Rpm) => Feature::Rpm,
            Kind::Repos(RepoType::File, _)
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
            Kind::Repos(t, snapshots) => Kind::Repos(*t, !snapshots),
            Kind::Remotes(t) => Kind::Remotes(flip(t)),
            Kind::Publications(t) => Kind::Publications(flip(t)),
            Kind::Distributions(t) => Kind::Distributions(flip(t)),
            Kind::Content { version, ctype } => {
                let n = content_types(RepoType::from_href(version)?).len();
                Kind::Content { version: version.clone(), ctype: (ctype + 1) % n }
            }
            Kind::Tasks => Kind::ServerTasks,
            Kind::ServerTasks => Kind::Tasks,
            Kind::Logs { polls } => Kind::Logs { polls: !polls },
            _ => return None,
        })
    }

    /// API collection path and extra query for API-backed screens.
    fn source(&self) -> Option<Source> {
        let name = Some("name__icontains");
        Some(match self {
            Kind::Repos(t, snapshots) => {
                let mut q = vec![("ordering", "name".into())];
                if !snapshots {
                    q.push(("pulp_label_select", format!("!{}", crate::cv::LABEL_CV)));
                }
                (t.repositories().into(), q, name)
            }
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
            Kind::Repos(..) => vec![
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
            Kind::Publications(t) => {
                let mut c = vec![
                    col("id", 13, |v| short(&s(v, "pulp_href"))),
                    col("version", 14, |v| short(&s(v, "repository_version"))),
                    col("created", 19, |v| ts(v, "pulp_created")),
                ];
                // Only File publications have the field; Delete looks up the distributions of both types.
                if *t == RepoType::File {
                    c.push(col("distributions", 6, |v| v["distributions"].as_array().map(|a| a.len().to_string()).unwrap_or_default()));
                }
                c.push(col("checkpoint", 5, |v| s(v, "checkpoint")));
                c
            }
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
                col("composite", 9, |v| if v["composite"] == true { "yes".into() } else { String::new() }),
                col("repos", 5, |v| v["repos"].as_array().map(|a| a.len().to_string()).unwrap_or_default()),
                col("latest", 7, |v| s(v, "latest")),
                col("environments", 40, |v| s(v, "bound")),
            ],
            Kind::CvVersions { .. } => vec![
                col("version", 7, |v| s(v, "number")),
                col("state", 11, |v| s(v, "state")),
                col("created", 19, |v| ts(v, "created_at")),
                col("environments", 20, |v| s(v, "environments")),
                col("notes", 30, |v| s(v, "notes")),
            ],
            Kind::Environments => vec![
                col("name", 18, |v| s(v, "name")),
                col("label", 14, |v| s(v, "label")),
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
            Kind::Logs { .. } => vec![
                col("time", 8, |v| s(v, "at")),
                col("kind", 7, |v| s(v, "kind")),
                col("server", 10, |v| s(v, "server")),
                col("summary", 80, |v| s(v, "summary")),
            ],
        }
    }

    pub fn actions(&self) -> Vec<Act> {
        use Act::*;
        match self {
            Kind::Profiles => vec![Connect, TestProfile, Create, Edit, Delete, StoreSecret, ExportCv, ImportCv],
            Kind::Repos(..) => {
                vec![Versions, Content, Sync, SyncHistory, Upload, Publish, Distribute, CopyContent, Create, Edit, Delete, Inspect]
            }
            Kind::Remotes(_) | Kind::Roles => vec![Create, Edit, Delete, Inspect],
            Kind::Versions(_) => vec![Content, Publish, Distribute, Rollback, Compare, Delete, Inspect],
            Kind::Content { .. } => vec![Inspect, RemoveContent, AddToRepo],
            Kind::Publications(_) => vec![Create, Distribute, Delete, Inspect],
            Kind::Distributions(_) => vec![CopyUrl, Verify, Rollback, Create, Edit, Delete, Inspect],
            Kind::ContentViews => vec![
                CvVersions,
                CvSyncRepos,
                CvPreview,
                CvPublish,
                Promote,
                Rollback,
                CvDrift,
                CvAddRepo,
                CvFilters,
                CvRemoveRepo,
                CvAddComponent,
                CvRemoveComponent,
                BreakLock,
                Create,
                Edit,
                Delete,
                Inspect,
            ],
            Kind::CvVersions { .. } => vec![Promote, Rollback, Compare, Resume, WatchTask, Delete, Inspect],
            Kind::Environments => vec![Create, Edit, ToggleProtected, Delete],
            Kind::UpstreamPulps => vec![Replicate, ReplicationHistory, FallbackReplicate, Create, Edit, Delete, Inspect],
            Kind::Users => vec![Roles, Create, Edit, Password, ToggleActive, Delete, Inspect],
            Kind::UserRoles { .. } | Kind::GroupRoles { .. } | Kind::GroupUsers { .. } => vec![Create, Delete],
            Kind::Groups => vec![Members, Roles, Create, Delete, Inspect],
            Kind::Tasks => vec![WatchTask, Inspect, CancelTask, DismissTasks],
            Kind::ServerTasks => vec![WatchTask, Inspect, TrackTask, CancelTask],
            Kind::Logs { .. } => vec![Inspect],
        }
    }

    /// The actions that fit `item`: a composite view has components instead of repositories, a normal view the opposite.
    pub fn item_actions(&self, item: &Value) -> Vec<Act> {
        use Act::*;
        let mut actions = self.actions();
        if *self == Kind::ContentViews {
            let composite = item["composite"] == true;
            actions.retain(|a| match a {
                CvAddRepo | CvFilters | CvRemoveRepo | CvSyncRepos => !composite,
                CvAddComponent | CvRemoveComponent => composite,
                _ => true,
            });
        }
        actions
    }
}

fn sum_counts(v: &Value) -> String {
    v.as_object().map(|m| m.values().filter_map(|x| x["count"].as_u64()).sum::<u64>().to_string()).unwrap_or_default()
}

/// Load one page of a screen. API screens query Pulp; Content View screens read the CV store
/// (`cv`: the store or why it is unavailable) under `scope`; `profile` marks the active server.
#[allow(clippy::too_many_arguments)]
pub async fn load(
    k: &Kind,
    ctx: Option<&Ctx>,
    cv: Result<&Store, &str>,
    scope: &str,
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
    let store = cv.map_err(|e| super::app::unavailable(e.into()));
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
            let store = store?;
            let envs = store.environments(scope).await?;
            let audit = store.audit_log(scope, 200).await?;
            let mut out = vec![];
            for cv in store.content_views(scope).await? {
                // `cv.*` entries start with the view name; `os` must not get the entries of `os-base`.
                let about = |a: &&crate::store::AuditEntry| {
                    a.action.starts_with("cv.")
                        && a.detail.strip_prefix(cv.name.as_str()).is_some_and(|r| r.is_empty() || r.starts_with([' ', ':']))
                };
                let versions = store.versions(cv.id).await?;
                let numbers: std::collections::HashMap<i64, i64> = versions.iter().map(|v| (v.id, v.number)).collect();
                let bound: Vec<String> = store
                    .bindings_for_cv(cv.id)
                    .await?
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
                    .cv_repos(cv.id)
                    .await?
                    .into_iter()
                    .map(|r| json!({ "name": r.repo_name, "type": r.repo_type, "href": r.repo_href, "filters": format_rules(&r.filters) }))
                    .collect();
                let mut components = vec![];
                for c in store.components(cv.id).await? {
                    let pin = match c.pinned {
                        Some(id) => format!("pinned v{}", store.version(id).await?.number),
                        None => match store.versions(c.cv_id).await?.iter().find(|v| v.state == "published") {
                            Some(v) => format!("latest (v{})", v.number),
                            None => "latest (none published)".into(),
                        },
                    };
                    components.push(json!({ "id": c.cv_id, "name": c.name, "pin": pin }));
                }
                out.push(json!({
                    "id": cv.id, "name": cv.name, "description": cv.description, "created_at": cv.created_at,
                    "composite": cv.composite, "auto_publish": cv.auto_publish, "components": components,
                    "repos": repos, "bound": bound.join("  "),
                    "latest": versions.iter().find(|v| v.state == "published").map(|v| format!("v{}", v.number)),
                    "versions": versions.iter().filter(|v| v.state != "deleted").map(|v| json!({
                        "number": v.number, "state": v.state, "notes": v.notes, "created_at": v.created_at, "error": v.error
                    })).collect::<Vec<_>>(),
                    "audit": audit.iter().filter(about).take(10)
                        .map(|a| format!("{} {} {}{}", a.at, a.action, a.detail, if a.actor.is_empty() { String::new() } else { format!(" by {}", a.actor) }))
                        .collect::<Vec<_>>(),
                }));
            }
            out
        }
        Kind::CvVersions { cv, .. } => {
            let store = store?;
            let envs = store.environments(scope).await?;
            let bindings = store.bindings_for_cv(*cv).await?;
            // No live lock: the client that ran a `publishing` version ended; Resume continues it.
            let running = store.lock_live(*cv, crate::cv::unix_now()).await?;
            let mut out = vec![];
            for v in store.versions(*cv).await? {
                let state = if v.state == "publishing" && !running { "interrupted" } else { v.state.as_str() };
                let in_envs: Vec<String> = bindings
                    .iter()
                    .filter(|b| b.version_id == v.id)
                    .filter_map(|b| envs.iter().find(|e| e.id == b.env_id).map(|e| e.name.clone()))
                    .collect();
                let repos: Vec<Value> = store
                    .version_repos(v.id)
                    .await
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
                let components: Vec<String> =
                    store.version_components(v.id).await?.into_iter().map(|u| format!("{} v{}", u.name, u.number)).collect();
                out.push(json!({
                    "id": v.id, "cv_id": v.cv_id, "number": v.number, "state": state, "notes": v.notes,
                    "created_at": v.created_at, "finished_at": v.finished_at, "error": v.error,
                    "environments": in_envs.join(", "), "repositories": repos, "components": components,
                }));
            }
            out
        }
        Kind::Environments => {
            let store = store?;
            store.ensure_library(scope).await?;
            let envs = store.environments(scope).await?;
            let cvs = store.content_views(scope).await?;
            let mut out = vec![];
            for e in &envs {
                let mut bound = vec![];
                for cv in &cvs {
                    if let Ok(Some(b)) = store.binding(e.id, cv.id).await
                        && let Ok(v) = store.version(b.version_id).await
                    {
                        bound.push(format!("{}:v{}", cv.name, v.number));
                    }
                }
                out.push(json!({
                    "id": e.id, "name": e.name, "label": e.label, "library": e.name == LIBRARY && e.prior_id.is_none(),
                    "prior": e.prior_id.and_then(|p| envs.iter().find(|x| x.id == p)).map(|x| x.name.clone()).unwrap_or_default(),
                    "protected": e.protected, "bound": bound.join(", "),
                }));
            }
            out
        }
        Kind::Logs { polls } => crate::session_log::rows(*polls),
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
            // A Content View row is a normal or a composite view: the actions differ, so check both.
            for item in [json!({}), json!({ "composite": true })] {
                let keys: Vec<char> = k.item_actions(&item).iter().map(|a| a.key()).collect();
                let mut dedup = keys.clone();
                dedup.sort();
                dedup.dedup();
                assert_eq!(keys.len(), dedup.len(), "duplicate action key in {k:?} for {item}: {keys:?}");
                assert!(keys.iter().all(|c| !reserved.contains(*c)), "{k:?} uses a reserved key: {keys:?}");
            }
            assert!(!k.columns().is_empty());
        }
    }

    #[test]
    fn composite_rows_offer_component_actions_only() {
        let (normal, composite) =
            (Kind::ContentViews.item_actions(&json!({})), Kind::ContentViews.item_actions(&json!({ "composite": true })));
        for a in [Act::CvAddRepo, Act::CvFilters, Act::CvRemoveRepo, Act::CvSyncRepos] {
            assert!(normal.contains(&a) && !composite.contains(&a), "{a:?}");
        }
        for a in [Act::CvAddComponent, Act::CvRemoveComponent] {
            assert!(composite.contains(&a) && !normal.contains(&a), "{a:?}");
        }
        assert!(Act::CvAddComponent.mutates(&Kind::ContentViews, true) && !Act::CvAddComponent.mutates(&Kind::ContentViews, false));
        assert!(Act::CvRemoveComponent.mutates(&Kind::ContentViews, true) && !Act::CvRemoveComponent.mutates(&Kind::ContentViews, false));
        assert_eq!((Act::CvAddComponent.key(), Act::CvRemoveComponent.key()), ('a', 'x'));
        assert_eq!(
            Kind::Repos(RepoType::Rpm, false).item_actions(&json!({ "composite": true })),
            Kind::Repos(RepoType::Rpm, false).actions()
        );
    }

    #[tokio::test]
    async fn content_view_rows_describe_composites_and_their_versions() {
        let (st, cfg) = (Store::open_in_memory().await.unwrap(), Config::default());
        let os = st.create_cv("lab", "os", "").await.unwrap();
        let extras = st.create_cv("lab", "extras", "").await.unwrap();
        let none = st.create_cv("lab", "none", "").await.unwrap();
        let v1 = st.begin_version(os, "", &[]).await.unwrap();
        st.finish_version(v1.id, "published", None).await.unwrap();
        let v2 = st.begin_version(os, "", &[]).await.unwrap();
        st.finish_version(v2.id, "published", None).await.unwrap();
        let e1 = st.begin_version(extras, "", &[]).await.unwrap();
        st.finish_version(e1.id, "published", None).await.unwrap();
        let c = st.create_composite("lab", "rocky9", "").await.unwrap();
        st.set_auto_publish(c, true).await.unwrap();
        st.set_component(c, os, Some(v1.id)).await.unwrap();
        st.set_component(c, extras, None).await.unwrap();
        st.set_component(c, none, None).await.unwrap();
        let cv1 = st.begin_composite_version(c, "", &[v1.id, e1.id], &[]).await.unwrap();

        let (rows, _) = load(&Kind::ContentViews, None, Ok(&st), "lab", "lab", &cfg, 0, "").await.unwrap();
        let get = |name: &str| rows.iter().find(|r| r["name"] == name).unwrap().clone();
        let cols = Kind::ContentViews.columns();
        assert_eq!(cols[1].title, "composite");
        let shown = |name: &str| (cols[1].get)(&get(name));
        assert_eq!((shown("rocky9"), shown("os")), ("yes".to_string(), String::new()));
        let rocky = get("rocky9");
        assert_eq!((rocky["composite"].clone(), rocky["auto_publish"].clone()), (json!(true), json!(true)));
        let pins: Vec<(i64, String, String)> =
            rocky["components"].as_array().unwrap().iter().map(|c| (c["id"].as_i64().unwrap(), s(c, "name"), s(c, "pin"))).collect();
        assert_eq!(
            pins,
            [
                (extras, "extras".to_string(), "latest (v1)".to_string()),
                (none, "none".to_string(), "latest (none published)".to_string()),
                (os, "os".to_string(), "pinned v1".to_string()),
            ]
        );
        assert_eq!(get("os")["components"], json!([]), "a normal view has no components");
        assert_eq!(get("os")["composite"], json!(false));

        let k = Kind::CvVersions { cv: c, name: "rocky9".into() };
        let (versions, _) = load(&k, None, Ok(&st), "lab", "lab", &cfg, 0, "").await.unwrap();
        assert_eq!((versions[0]["id"].as_i64(), versions[0]["components"].clone()), (Some(cv1.id), json!(["extras v1", "os v1"])));
        let k = Kind::CvVersions { cv: os, name: "os".into() };
        let (versions, _) = load(&k, None, Ok(&st), "lab", "lab", &cfg, 0, "").await.unwrap();
        assert!(versions.iter().all(|v| v["components"] == json!([])));
    }

    #[tokio::test]
    async fn a_publishing_version_without_a_live_lock_shows_interrupted() {
        let (st, cfg) = (Store::open_in_memory().await.unwrap(), Config::default());
        let cv = st.create_cv("lab", "os", "").await.unwrap();
        st.begin_version(cv, "", &[]).await.unwrap();
        let k = Kind::CvVersions { cv, name: "os".into() };
        let state = async || load(&k, None, Ok(&st), "lab", "lab", &cfg, 0, "").await.unwrap().0[0]["state"].clone();
        assert_eq!(state().await, "interrupted", "no lock: the client that ran it ended");
        let now = crate::cv::unix_now();
        st.try_lock(cv, "a@h", "t1", "publish", now).await.unwrap().unwrap();
        assert_eq!(state().await, "publishing");
        st.renew_lock(cv, "t1", now - crate::store::LOCK_TTL_SECS - 1).await.unwrap();
        assert_eq!(state().await, "interrupted", "lock expired");
    }

    #[test]
    fn repos_screen_hides_content_view_snapshots_until_toggled() {
        let k = Kind::Repos(RepoType::Rpm, false);
        let label = |k: &Kind| k.source().unwrap().1.into_iter().find(|(q, _)| *q == "pulp_label_select").map(|(_, v)| v);
        assert_eq!(label(&k).as_deref(), Some("!pulp_tui_cv"));
        assert_eq!(k.toggle(), Some(Kind::Repos(RepoType::Rpm, true)));
        assert_eq!(label(&Kind::Repos(RepoType::Rpm, true)), None);
        assert!(Kind::Repos(RepoType::File, true).title().ends_with(" — snapshots shown"));
    }

    #[test]
    fn logs_screen_is_local_and_toggles_polling() {
        let k = Kind::Logs { polls: false };
        assert!(!k.needs_connection());
        assert_eq!(k.toggle(), Some(Kind::Logs { polls: true }));
        assert_eq!(k.actions(), vec![Act::Inspect]);
        assert!(NAV.iter().any(|(l, k)| *l == "Logs" && *k == Kind::Logs { polls: false }));
        assert!(k.title().ends_with("[t: show/hide task polling]"));
        assert!(Kind::Logs { polls: true }.title().ends_with(" — polling shown"));
        assert!(!Act::Inspect.mutates(&k, false));
    }

    #[tokio::test]
    async fn logs_load_reads_the_session_log_and_applies_the_filter() {
        crate::session_log::record("notice", "lab", "[ok] logs-load-needle".into(), vec![]);
        let (store, cfg, k) = (Store::open_in_memory().await.unwrap(), Config::default(), Kind::Logs { polls: false });
        let (all, n) = load(&k, None, Ok(&store), "lab", "lab", &cfg, 0, "").await.unwrap();
        assert_eq!(n as usize, all.len());
        let (hit, _) = load(&k, None, Ok(&store), "lab", "lab", &cfg, 0, "LOGS-LOAD-NEEDLE").await.unwrap();
        assert_eq!(hit.len(), 1);
        let cols: Vec<String> = k.columns().iter().map(|c| (c.get)(&hit[0])).collect();
        assert_eq!(cols[1..], ["notice", "lab", "[ok] logs-load-needle"]);
    }

    #[tokio::test]
    async fn cv_screens_report_unavailable_store() {
        let e = load(&Kind::ContentViews, None, Err("timeout"), "s", "p", &Config::default(), 0, "").await.unwrap_err();
        assert_eq!(e.to_string(), "shared database unavailable: timeout");
        // Screens that do not use the store still load.
        assert!(load(&Kind::Profiles, None, Err("timeout"), "s", "p", &Config::default(), 0, "").await.is_ok());
    }

    #[test]
    fn read_only_classification() {
        assert!(Act::Delete.mutates(&Kind::Repos(RepoType::Rpm, false), false));
        assert!(!Act::Inspect.mutates(&Kind::Repos(RepoType::Rpm, false), false));
        assert!(!Act::Create.mutates(&Kind::ContentViews, false), "local CV definitions");
        assert!(Act::Create.mutates(&Kind::ContentViews, true), "shared CV definitions");
        assert!(Act::ToggleProtected.mutates(&Kind::Environments, true));
        assert!(!Act::ToggleProtected.mutates(&Kind::Environments, false));
        assert!(Act::Edit.mutates(&Kind::Environments, false), "a relabel moves distributions");
        assert!(Kind::Environments.actions().contains(&Act::Edit));
        assert!(Act::CvPublish.mutates(&Kind::ContentViews, false));
        assert!(Act::BreakLock.mutates(&Kind::ContentViews, false), "removes another operator's lock");
        assert!(Kind::ContentViews.actions().contains(&Act::BreakLock) && Act::BreakLock.needs_item());
        assert!(Act::CvSyncRepos.mutates(&Kind::ContentViews, false), "changes Pulp, whatever the database");
        assert!(Kind::ContentViews.actions().contains(&Act::CvSyncRepos) && Act::CvSyncRepos.key() == 'f');
        assert!(!Act::Delete.mutates(&Kind::Profiles, true));
        assert!(Act::ImportCv.mutates(&Kind::Profiles, true), "bulk create in the shared database");
        assert!(!Act::ImportCv.mutates(&Kind::Profiles, false));
        assert!(Act::MigrateDb.mutates(&Kind::Profiles, false) && Act::CopyDb.mutates(&Kind::Profiles, false));
        assert!(!Kind::Profiles.actions().iter().any(|a| matches!(a, Act::MigrateDb | Act::CopyDb)), "offered on Connect and save only");
    }

    #[test]
    fn short_ids() {
        assert_eq!(short("/pulp/api/v3/repositories/rpm/rpm/0190abcd-1234/versions/3/"), "0190abcd@v3");
        assert_eq!(short("/pulp/api/v3/publications/file/file/0190abcd-1234-5678/"), "0190abcd-1234");
    }
}
