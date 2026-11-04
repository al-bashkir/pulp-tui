//! Screen definitions: what each list shows, where it loads from, and which actions it offers.

use super::app::{Ctx, PAGE};
use super::form::format_labels;
use crate::config::Config;
use crate::filters::format_rules;
use crate::pulp::{Capabilities, Feature, RepoType, deb};
use crate::store::{LIBRARY, Store};
use anyhow::Result;
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Profiles,
    /// `true` also shows the snapshot repositories of Content Views (label `pulp_tui_cv`).
    Repos(RepoType, bool),
    /// `true`: the plugin's second remote type (`RepoType::alt_remote`: pulp_rpm ULN, pulp_file Git).
    Remotes(RepoType, bool),
    /// Alternate content sources.
    Acs(RepoType),
    /// Versions of one repository (href, name) or of all repositories.
    Versions(Option<(String, String)>),
    /// Content of a repository version; `ctype` indexes `RepoType::content_types`.
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
    PulpExporters,
    FsExporters,
    /// Exports of one exporter (href, name).
    Exports {
        exporter: String,
        name: String,
    },
    Importers,
    /// Imports of one importer (href, name).
    Imports {
        importer: String,
        name: String,
    },
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
    /// All tasks of the server, with the query filters of the filter form (**F**); `/` searches the name.
    ServerTasks(Vec<(&'static str, String)>),
    TaskGroups,
    Workers,
    /// This session's requests, actions and notices; `polls` also shows task polling.
    Logs {
        polls: bool,
    },
}

pub const NAV: &[(&str, Kind)] = &[
    ("Servers", Kind::Profiles),
    ("RPM Repositories", Kind::Repos(RepoType::Rpm, false)),
    ("File Repositories", Kind::Repos(RepoType::File, false)),
    ("Deb Repositories", Kind::Repos(RepoType::Deb, false)),
    ("Remotes", Kind::Remotes(RepoType::Rpm, false)),
    ("Alternate Sources", Kind::Acs(RepoType::Rpm)),
    ("Repository Versions", Kind::Versions(None)),
    ("Publications", Kind::Publications(RepoType::Rpm)),
    ("Distributions", Kind::Distributions(RepoType::Rpm)),
    ("Content Views", Kind::ContentViews),
    ("Lifecycle Envs", Kind::Environments),
    ("Replication", Kind::UpstreamPulps),
    ("Exporters", Kind::PulpExporters),
    ("Importers", Kind::Importers),
    ("Users", Kind::Users),
    ("Groups", Kind::Groups),
    ("Roles", Kind::Roles),
    ("Tasks", Kind::Tasks),
    ("Logs", Kind::Logs { polls: false }),
];

/// API collection path, extra query and search parameter of a screen.
type Source = (String, Vec<(&'static str, String)>, Option<&'static str>);

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

/// Label, collection and OpenAPI schema of the remotes on `Kind::Remotes(t, alt)`.
pub fn remote_type(t: RepoType, alt: bool) -> (String, &'static str, &'static str) {
    match t.alt_remote().filter(|_| alt) {
        Some((label, path, schema)) => (format!("{} {label}", t.label()), path, schema),
        None => (t.label().into(), t.remotes(), t.remote_schema()),
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
    /// RPM: a comps.xml (`rpm/comps/`) or one advisory as JSON (POST `content/rpm/advisories/`) into the repository.
    UploadComps,
    UploadAdvisory,
    /// RPM: a modulemd, modulemd defaults or modulemd obsoletes from its fields (`content/rpm/modulemds/`, ...).
    CreateModule,
    Publish,
    Distribute,
    CopyContent,
    /// Delete the stored files of a repository's synced content (`repositories/reclaim_space/`).
    Reclaim,
    /// RPM: remove older versions of packages from the latest version (`rpm/prune/`), after a dry run.
    PrunePackages,
    RemoveContent,
    AddToRepo,
    Compare,
    Rollback,
    /// A new latest version with the content of the selected version (`modify/` with `base_version`).
    Restore,
    /// Download missing or corrupt files of a repository version again (`{version}repair/`).
    Repair,
    /// RPM: scan a repository version for known vulnerabilities (`{version}scan/`, osv.dev).
    Scan,
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
    /// Repository and remote definitions of a server (`repo_export`, as `export-repos` / `import-repos`).
    ExportRepos,
    ImportRepos,
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
    Unpromote,
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
    PruneOrphans,
    Export,
    Exports,
    /// Import check, then the import.
    Import,
    Imports,
    /// Alternate content source: index what the source has (`refresh/`, a task group).
    Refresh,
    /// Query filters of a server list (Server tasks).
    Filter,
    /// `tasks/purge/`: delete the records of finished tasks.
    PurgeTasks,
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
            Content | Connect | Members | Exports | Imports => 'o',
            Upload | Unpromote => 'u',
            Publish | CvPublish | PurgeTasks => 'P',
            Distribute | CvDrift => 'D',
            CopyContent => 'C',
            RemoveContent | CvRemoveRepo | CvRemoveComponent => 'x',
            AddToRepo | CvAddRepo | CvAddComponent | TrackTask => 'a',
            Compare => 'm',
            Rollback => 'b',
            CopyUrl => 'y',
            Verify | Scan => 'V',
            CancelTask => 'X',
            DismissTasks => 'z',
            WatchTask => 'W',
            TestProfile => 'T',
            StoreSecret => 'K',
            ExportCv | Export => 'E',
            ImportCv | Import => 'I',
            ExportRepos => 'X',
            ImportRepos => 'J',
            CvFilters | FallbackReplicate | Repair | Filter => 'F',
            CvPreview | Password => 'w',
            Promote | MigrateDb | CreateModule => 'M',
            CopyDb => 'Y',
            Resume | Replicate | Roles | Restore | Refresh | Reclaim => 'R',
            ToggleProtected => 'L',
            ToggleActive | UploadAdvisory => 'A',
            BreakLock | UploadComps => 'U',
            PruneOrphans | PrunePackages => 'O',
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
            UploadComps => "Upload comps.xml (package groups)",
            UploadAdvisory => "Upload advisory (JSON)",
            CreateModule => "Create module content (modulemd, defaults, obsoletes)",
            Publish => "Publish",
            Distribute => "Distribute (create/re-point distribution)",
            CopyContent => "Copy content from another repository",
            Reclaim => "Reclaim disk space (files of synced content)",
            PrunePackages => "Prune old package versions (new version)",
            RemoveContent => "Remove from repository (new version)",
            AddToRepo => "Add to another repository",
            Compare => "Compare with marked version",
            Rollback => "Roll back a distribution to this",
            Restore => "Restore as latest version (new version)",
            Repair => "Repair (download missing or corrupt files again)",
            CopyUrl => "Copy content URL",
            Verify => "Verify served content",
            Scan => "Scan for vulnerabilities (osv.dev)",
            CancelTask => "Cancel task on server",
            DismissTasks => "Dismiss finished tasks",
            TrackTask => "Track in Task Center",
            WatchTask => "Watch task (live)",
            Connect => "Connect (use this server)",
            TestProfile => "Test connection",
            StoreSecret => "Store secret in system keyring",
            ExportCv => "Export Content View definitions",
            ImportCv => "Import Content View definitions",
            ExportRepos => "Export repository definitions",
            ImportRepos => "Import repository definitions",
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
            Unpromote => "Unpromote from environment",
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
            PruneOrphans => "Prune orphan content (whole server)",
            Export => "Export now",
            Exports => "Exports",
            Import => "Import (check first)",
            Imports => "Imports",
            Refresh => "Refresh (index the source)",
            Filter => "Filter (state, name, resource, start time)",
            PurgeTasks => "Purge finished tasks (whole server)",
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
            Kind::Profiles => matches!(self, MigrateDb | CopyDb | ImportRepos) || shared && matches!(self, ImportCv),
            // Edit changes the label, which moves distributions on the server.
            Kind::Environments => shared || matches!(self, Edit),
            Kind::ContentViews => {
                matches!(self, Delete | CvPublish | CvSyncRepos | Promote | Unpromote | Rollback | BreakLock)
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
                    | Exports
                    | Imports
                    | Filter
            ),
        }
    }

    pub fn needs_item(self) -> bool {
        !matches!(
            self,
            Act::Create
                | Act::DismissTasks
                | Act::ImportCv
                | Act::ExportCv
                | Act::ExportRepos
                | Act::ImportRepos
                | Act::FallbackReplicate
                | Act::PruneOrphans
                | Act::Filter
                | Act::PurgeTasks
        )
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
            Kind::Remotes(t, alt) => format!("{} Remotes  [t: toggle type]", remote_type(*t, *alt).0),
            Kind::Acs(t) => format!("{} Alternate Content Sources  [t: toggle type]", t.label()),
            Kind::Versions(Some((_, n))) => format!("Versions of {n}"),
            Kind::Versions(None) => "Repository Versions (all repositories)".into(),
            Kind::Content { version, ctype } => {
                let label = RepoType::from_href(version).and_then(|t| t.content_types().get(*ctype)).map_or("?", |c| c.label);
                format!("Content {label} of {}  [t: type]", short(version))
            }
            Kind::Publications(t) => format!("{} Publications  [t: toggle type]", t.label()),
            Kind::Distributions(t) => format!("{} Distributions  [t: toggle type]", t.label()),
            Kind::ContentViews => "Content Views (managed by pulp-tui, not a Pulp API)".into(),
            Kind::CvVersions { name, .. } => format!("Content View {name}: versions"),
            Kind::Environments => "Lifecycle Environments (pulp-tui)".into(),
            Kind::UpstreamPulps => "Replication: Upstream Pulps (native, tech preview)".into(),
            Kind::PulpExporters => "Pulp Exporters (disconnected export)  [t: toggle type]".into(),
            Kind::FsExporters => "Filesystem Exporters (files of a publication or version)  [t: toggle type]".into(),
            Kind::Exports { name, .. } => format!("Exports of {name}"),
            Kind::Importers => "Pulp Importers (disconnected import)".into(),
            Kind::Imports { name, .. } => format!("Imports of {name}"),
            Kind::Users => "Users".into(),
            Kind::UserRoles { name, .. } => format!("Roles of user {name}"),
            Kind::Groups => "Groups".into(),
            Kind::GroupUsers { name, .. } => format!("Members of group {name}"),
            Kind::GroupRoles { name, .. } => format!("Roles of group {name}"),
            Kind::Roles => "Roles".into(),
            Kind::Tasks => "Task Center (tracked tasks)  [t: all server tasks]".into(),
            Kind::ServerTasks(q) => {
                let f: Vec<String> = q.iter().map(|(k, v)| format!("{k}={v}")).collect();
                format!("Server tasks{}  [t: task groups]", if f.is_empty() { String::new() } else { format!(" ({})", f.join(", ")) })
            }
            Kind::TaskGroups => "Task groups (newest first)  [t: workers]".into(),
            Kind::Workers => "Workers  [t: tracked tasks]".into(),
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
            Kind::Acs(t) if caps.supports(t.feature()) && !caps.has_endpoint(t.acs()) => {
                return Some(format!("not supported by this server: {} alternate content sources ({})", t.label(), t.acs()));
            }
            Kind::Remotes(t, true) if caps.supports(t.feature()) && !caps.has_endpoint(remote_type(*t, true).1) => {
                let (label, path, _) = remote_type(*t, true);
                return Some(format!("not supported by this server: {label} remotes ({path})"));
            }
            Kind::Repos(t, _) | Kind::Remotes(t, _) | Kind::Acs(t) | Kind::Publications(t) | Kind::Distributions(t) => t.feature(),
            Kind::UpstreamPulps => Feature::NativeReplication,
            Kind::PulpExporters => Feature::PulpExport,
            Kind::FsExporters => Feature::FilesystemExport,
            Kind::Importers => Feature::PulpImport,
            Kind::Users | Kind::UserRoles { .. } | Kind::Groups | Kind::GroupUsers { .. } | Kind::GroupRoles { .. } => Feature::Users,
            Kind::Roles => Feature::Roles,
            Kind::Workers => Feature::Workers,
            Kind::TaskGroups => Feature::TaskGroups,
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
        Some(match self {
            Kind::Repos(t, snapshots) => Kind::Repos(*t, !snapshots),
            // A plugin's second remote type comes right after its first.
            Kind::Remotes(t, false) if t.alt_remote().is_some() => Kind::Remotes(*t, true),
            Kind::Remotes(t, _) => Kind::Remotes(t.next(), false),
            Kind::Acs(t) => Kind::Acs(t.next()),
            Kind::Publications(t) => Kind::Publications(t.next()),
            Kind::Distributions(t) => Kind::Distributions(t.next()),
            Kind::Content { version, ctype } => {
                let n = RepoType::from_href(version)?.content_types().len();
                Kind::Content { version: version.clone(), ctype: (ctype + 1) % n }
            }
            Kind::Tasks => Kind::ServerTasks(vec![]),
            Kind::ServerTasks(_) => Kind::TaskGroups,
            Kind::TaskGroups => Kind::Workers,
            Kind::Workers => Kind::Tasks,
            Kind::PulpExporters => Kind::FsExporters,
            Kind::FsExporters => Kind::PulpExporters,
            Kind::Logs { polls } => Kind::Logs { polls: !polls },
            _ => return None,
        })
    }

    /// `/` filters this list: on the server, or in the client for lists that are not API-backed.
    pub fn searchable(&self) -> bool {
        self.source().is_none_or(|(_, _, search)| search.is_some())
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
            Kind::Remotes(t, alt) => (remote_type(*t, *alt).1.into(), vec![("ordering", "name".into())], name),
            Kind::Acs(t) => (t.acs().into(), vec![("ordering", "name".into())], name),
            Kind::Versions(Some((repo, _))) => (format!("{repo}versions/"), vec![("ordering", "-number".into())], None),
            Kind::Versions(None) => ("repository_versions/".into(), vec![("ordering", "-pulp_created".into())], None),
            Kind::Content { version, ctype } => {
                let t = RepoType::from_href(version)?;
                let path = t.content_types()[*ctype].path;
                // Search field, ordering, and whether to drop the heavy package fields (RPM packages only).
                let (search, ordering, exclude_fields) = match (t, *ctype) {
                    (RepoType::Rpm, 0) => (Some("name__contains"), Some("name"), true),
                    (RepoType::Rpm, 1..) => (None, None, false),
                    (RepoType::File, _) => (Some("relative_path__icontains"), None, false),
                    (RepoType::Deb, 0) => (Some("package__contains"), Some("package"), false),
                    (RepoType::Deb, 1..) => (None, None, false),
                };
                let mut q = vec![("repository_version", version.clone())];
                if let Some(o) = ordering {
                    q.push(("ordering", o.into()));
                }
                if exclude_fields {
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
            Kind::PulpExporters => ("exporters/core/pulp/".into(), vec![("ordering", "name".into())], name),
            Kind::FsExporters => ("exporters/core/filesystem/".into(), vec![("ordering", "name".into())], name),
            Kind::Exports { exporter, .. } => (format!("{exporter}exports/"), vec![], None),
            Kind::Importers => ("importers/core/pulp/".into(), vec![("ordering", "name".into())], name),
            Kind::Imports { importer, .. } => (format!("{importer}imports/"), vec![], None),
            Kind::Users => ("users/".into(), vec![("ordering", "username".into())], Some("username__icontains")),
            Kind::UserRoles { user, .. } => (format!("{user}roles/"), vec![], None),
            Kind::Groups => ("groups/".into(), vec![("ordering", "name".into())], name),
            Kind::GroupUsers { group, .. } => (format!("{group}users/"), vec![], None),
            Kind::GroupRoles { group, .. } => (format!("{group}roles/"), vec![], None),
            Kind::Roles => ("roles/".into(), vec![("ordering", "name".into())], name),
            Kind::Workers => ("workers/".into(), vec![("ordering", "name".into())], name),
            // No filters and no ordering in the API; the task list of a group comes with Inspect.
            Kind::TaskGroups => ("task-groups/".into(), vec![("exclude_fields", "tasks".into())], None),
            Kind::ServerTasks(f) => {
                let q = [("ordering", "-pulp_created".into()), ("exclude_fields", "progress_reports".into())];
                ("tasks/".into(), q.into_iter().chain(f.iter().cloned()).collect(), Some("name__contains"))
            }
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
            // pulp_file Git remotes have no policy (always immediate).
            Kind::Remotes(RepoType::File, true) => {
                vec![col("name", 28, |v| s(v, "name")), col("git ref", 14, |v| s(v, "git_ref")), col("url", 50, |v| s(v, "url"))]
            }
            Kind::Remotes(..) => {
                vec![col("name", 28, |v| s(v, "name")), col("policy", 10, |v| s(v, "policy")), col("url", 50, |v| s(v, "url"))]
            }
            Kind::Acs(_) => vec![
                col("name", 28, |v| s(v, "name")),
                col("paths", 30, |v| v["paths"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")),
                col("last refreshed", 19, |v| ts(v, "last_refreshed")),
                col("remote", 13, |v| short(&s(v, "remote"))),
            ],
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
                // Package groups, categories and environments.
                (Some(RepoType::Rpm), 3 | 8 | 9) => vec![col("id", 30, |v| s(v, "id")), col("name", 40, |v| s(v, "name"))],
                (Some(RepoType::File), _) => vec![
                    col("relative_path", 50, |v| s(v, "relative_path")),
                    col("sha256", 20, |v| s(v, "sha256").chars().take(16).collect()),
                ],
                (Some(RepoType::Deb), 0) => vec![
                    col("package", 30, |v| s(v, "package")),
                    col("version", 24, |v| s(v, "version")),
                    col("architecture", 8, |v| s(v, "architecture")),
                    col("sha256", 14, |v| s(v, "sha256").chars().take(12).collect()),
                ],
                (Some(RepoType::Deb), 1) => vec![
                    col("distribution", 20, |v| s(v, "distribution")),
                    col("codename", 14, |v| s(v, "codename")),
                    col("suite", 14, |v| s(v, "suite")),
                    col("version", 10, |v| s(v, "version")),
                ],
                (Some(RepoType::Deb), 2) => {
                    vec![col("distribution", 20, |v| s(v, "distribution")), col("component", 20, |v| s(v, "component"))]
                }
                (Some(RepoType::Rpm), 4..) | (Some(RepoType::Deb), 3..) | (None, _) => {
                    vec![col("href", 70, |v| s(v, "pulp_href")), col("created", 19, |v| ts(v, "pulp_created"))]
                }
            },
            Kind::Publications(t) => {
                let mut c = vec![
                    col("id", 13, |v| short(&s(v, "pulp_href"))),
                    col("version", 14, |v| short(&s(v, "repository_version"))),
                    col("created", 19, |v| ts(v, "pulp_created")),
                ];
                // Only File publications have the field; Delete looks up the distributions for every type.
                match t {
                    RepoType::Rpm => {}
                    RepoType::Deb => {
                        c.push(col("kind", 8, |v| if deb::is_verbatim(&s(v, "pulp_href")) { "verbatim" } else { "apt" }.into()))
                    }
                    RepoType::File => {
                        c.push(col("distributions", 6, |v| v["distributions"].as_array().map(|a| a.len().to_string()).unwrap_or_default()))
                    }
                }
                c.push(col("checkpoint", 5, |v| s(v, "checkpoint")));
                c
            }
            Kind::Distributions(_) => vec![
                col("name", 26, |v| s(v, "name")),
                col("base_path", 26, |v| s(v, "base_path")),
                col("binding", 10, |v| {
                    match (v["publication"].is_string(), v["repository_version"].is_string(), v["repository"].is_string()) {
                        (true, _, _) | (_, true, _) => "pinned".into(),
                        (_, _, true) if v["checkpoint"] == true => "checkpoint".into(),
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
            Kind::PulpExporters => vec![
                col("name", 24, |v| s(v, "name")),
                col("path", 40, |v| s(v, "path")),
                col("repos", 5, |v| v["repositories"].as_array().map(|a| a.len().to_string()).unwrap_or_default()),
            ],
            Kind::FsExporters => {
                vec![col("name", 24, |v| s(v, "name")), col("path", 40, |v| s(v, "path")), col("method", 9, |v| s(v, "method"))]
            }
            Kind::Exports { .. } => vec![
                col("created", 19, |v| ts(v, "pulp_created")),
                col("exported", 8, |v| v["exported_resources"].as_array().map(|a| a.len().to_string()).unwrap_or_default()),
                col("toc", 60, |v| s(&v["toc_info"], "file")),
            ],
            Kind::Importers => vec![col("name", 24, |v| s(v, "name")), col("mapping", 50, |v| format_labels(&v["repo_mapping"]))],
            // pulp_import keeps the TOC path as `params.path`.
            Kind::Imports { .. } => vec![
                col("created", 19, |v| ts(v, "pulp_created")),
                col("toc", 60, |v| s(&v["params"], "path")),
                col("task", 13, |v| short(&s(v, "task"))),
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
            Kind::ServerTasks(_) => vec![
                col("state", 10, |v| s(v, "state")),
                col("name", 44, |v| s(v, "name").trim_start_matches("pulpcore.app.tasks.").to_string()),
                col("created", 19, |v| ts(v, "pulp_created")),
            ],
            Kind::TaskGroups => vec![
                col("description", 36, |v| s(v, "description")),
                col("waiting", 7, |v| s(v, "waiting")),
                col("running", 7, |v| s(v, "running")),
                col("completed", 9, |v| s(v, "completed")),
                col("failed", 6, |v| s(v, "failed")),
                col("canceled", 8, |v| s(v, "canceled")),
                col("skipped", 7, |v| s(v, "skipped")),
            ],
            Kind::Workers => vec![
                col("name", 30, |v| s(v, "name")),
                col("last heartbeat", 19, |v| ts(v, "last_heartbeat")),
                col("current task", 14, |v| short(&s(v, "current_task"))),
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
            Kind::Profiles => vec![Connect, TestProfile, Create, Edit, Delete, StoreSecret, ExportCv, ImportCv, ExportRepos, ImportRepos],
            Kind::Repos(t, _) => {
                let mut actions = vec![
                    Versions,
                    Content,
                    Sync,
                    SyncHistory,
                    Upload,
                    Publish,
                    Distribute,
                    CopyContent,
                    Reclaim,
                    Create,
                    Edit,
                    Delete,
                    Inspect,
                ];
                match t {
                    RepoType::Rpm => {
                        actions.insert(9, PrunePackages); // after Reclaim
                        actions.splice(5..5, [UploadComps, UploadAdvisory, CreateModule]);
                    }
                    RepoType::File | RepoType::Deb => {}
                }
                actions
            }
            Kind::Remotes(..) | Kind::Roles => vec![Create, Edit, Delete, Inspect],
            Kind::Acs(_) => vec![Refresh, Create, Edit, Delete, Inspect],
            Kind::Versions(_) => {
                vec![Content, Publish, Distribute, Rollback, Restore, Compare, Repair, Scan, Delete, PruneOrphans, Inspect]
            }
            // A structured Deb copy brings the release records of binary packages only; other units would arrive alone.
            Kind::Content { version, ctype } if RepoType::from_href(version) == Some(RepoType::Deb) && *ctype != 0 => {
                vec![Inspect, RemoveContent]
            }
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
                Unpromote,
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
            Kind::PulpExporters | Kind::FsExporters => vec![Export, Exports, Create, Edit, Delete, Inspect],
            Kind::Exports { .. } => vec![Delete, Inspect],
            Kind::Importers => vec![Import, Imports, Create, Edit, Delete, Inspect],
            Kind::Imports { .. } => vec![Inspect],
            Kind::Users => vec![Roles, Create, Edit, Password, ToggleActive, Delete, Inspect],
            Kind::UserRoles { .. } | Kind::GroupRoles { .. } | Kind::GroupUsers { .. } => vec![Create, Delete],
            Kind::Groups => vec![Members, Roles, Create, Edit, Delete, Inspect],
            Kind::Tasks => vec![WatchTask, Inspect, CancelTask, DismissTasks],
            Kind::ServerTasks(_) => vec![WatchTask, Inspect, TrackTask, CancelTask, Filter, Delete, PurgeTasks],
            Kind::TaskGroups => vec![CancelTask, Inspect],
            Kind::Workers | Kind::Logs { .. } => vec![Inspect],
        }
    }

    /// The actions that fit `item`: a composite view has components instead of repositories, a normal view the opposite;
    /// only an RPM version has Scan.
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
        if RepoType::from_href(&s(item, "pulp_href")) != Some(RepoType::Rpm) {
            actions.retain(|a| *a != Scan);
        }
        actions
    }
}

/// One page of the Deb Publications screen: APT and verbatim publications, newest first. The merge needs each
/// collection from its start to the end of the page; then the page is cut out.
async fn deb_publications(c: &crate::pulp::PulpClient, offset: u64) -> Result<(Vec<Value>, u64)> {
    let q = [("ordering", "-pulp_created".to_string()), ("offset", "0".into()), ("limit", (offset + PAGE).to_string())];
    let (mut items, mut count) = (vec![], 0);
    for coll in [RepoType::Deb.publications(), deb::VERBATIM_PUBLICATIONS] {
        let page: crate::pulp::client::Page<Value> = c.list(&c.api(coll), &q).await?;
        count += page.count;
        items.extend(page.results);
    }
    items.sort_by(|a, b| b["pulp_created"].as_str().cmp(&a["pulp_created"].as_str()));
    Ok((items.into_iter().skip(offset as usize).take(PAGE as usize).collect(), count))
}

fn sum_counts(v: &Value) -> String {
    v.as_object().map(|m| m.values().filter_map(|x| x["count"].as_u64()).sum::<u64>().to_string()).unwrap_or_default()
}

/// ` (state)` after a binding that is not `active`.
fn binding_note(state: &str, running: bool) -> String {
    match crate::cv::shown_state(state, running) {
        "active" => String::new(),
        s => format!(" ({s})"),
    }
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
    if let (Kind::Publications(RepoType::Deb), Some(ctx)) = (k, ctx)
        && ctx.caps.has_endpoint(deb::VERBATIM_PUBLICATIONS)
    {
        return deb_publications(&ctx.client, offset).await;
    }
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
                let running = store.lock_live(cv.id, crate::cv::unix_now()).await?;
                let bound: Vec<String> = store
                    .bindings_for_cv(cv.id)
                    .await?
                    .iter()
                    .filter_map(|b| {
                        let e = envs.iter().find(|e| e.id == b.env_id)?;
                        let st = binding_note(&b.state, running);
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
                        "number": v.number, "state": crate::cv::shown_state(&v.state, running), "notes": v.notes, "created_at": v.created_at, "error": v.error
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
            // Resume continues an interrupted version.
            let running = store.lock_live(*cv, crate::cv::unix_now()).await?;
            let mut out = vec![];
            for v in store.versions(*cv).await? {
                let state = crate::cv::shown_state(&v.state, running);
                let in_envs: Vec<String> = bindings
                    .iter()
                    .filter(|b| b.version_id == v.id)
                    .filter_map(|b| envs.iter().find(|e| e.id == b.env_id).map(|e| e.name.clone()))
                    .collect();
                let repos: Vec<Value> = store
                    .version_repos(v.id)
                    .await?
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
            // Three queries per view, not three per environment and view.
            let mut bound: std::collections::HashMap<i64, Vec<String>> = std::collections::HashMap::new();
            for cv in store.content_views(scope).await? {
                let numbers: std::collections::HashMap<i64, i64> = store.versions(cv.id).await?.iter().map(|v| (v.id, v.number)).collect();
                let running = store.lock_live(cv.id, crate::cv::unix_now()).await?;
                for b in store.bindings_for_cv(cv.id).await? {
                    if let Some(n) = numbers.get(&b.version_id) {
                        bound.entry(b.env_id).or_default().push(format!("{}:v{n}{}", cv.name, binding_note(&b.state, running)));
                    }
                }
            }
            envs.iter()
                .map(|e| {
                    json!({
                        "id": e.id, "name": e.name, "label": e.label, "library": e.name == LIBRARY && e.prior_id.is_none(),
                        "prior": e.prior_id.and_then(|p| envs.iter().find(|x| x.id == p)).map(|x| x.name.clone()).unwrap_or_default(),
                        "protected": e.protected, "bound": bound.get(&e.id).map(|b| b.join(", ")).unwrap_or_default(),
                    })
                })
                .collect()
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
            Kind::ServerTasks(vec![]),
            Kind::Exports { exporter: "/e/".into(), name: "e".into() },
            Kind::FsExporters,
            Kind::Imports { importer: "/i/".into(), name: "i".into() },
            Kind::TaskGroups,
            Kind::Workers,
        ]);
        for k in kinds {
            // A Content View row is a normal or a composite view, a version row can be an RPM one: the actions differ.
            for item in [json!({}), json!({ "composite": true }), json!({ "pulp_href": "/x/rpm/rpm/1/versions/1/" })] {
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
    fn distribution_binding_column_names_checkpoint_distributions() {
        let cols = Kind::Distributions(RepoType::File).columns();
        let binding = cols.iter().find(|c| c.title == "binding").unwrap();
        let cases = [
            (json!({ "publication": "/p/" }), "pinned"),
            (json!({ "repository_version": "/r/versions/1/" }), "pinned"),
            (json!({ "repository": "/r/" }), "latest"),
            (json!({ "repository": "/r/", "checkpoint": false }), "latest"),
            (json!({ "repository": "/r/", "checkpoint": true }), "checkpoint"),
            (json!({}), "unbound"),
        ];
        for (row, want) in cases {
            assert_eq!((binding.get)(&row), want, "{row}");
        }
        assert!(usize::from(binding.width) >= "checkpoint".len());
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

    #[test]
    fn search_only_where_a_filter_applies() {
        assert!(Kind::Repos(RepoType::Rpm, false).searchable(), "server-side name filter");
        assert!(Kind::ContentViews.searchable(), "filtered in the client");
        assert!(!Kind::Versions(Some(("/r/".into(), "r".into()))).searchable(), "the API has no filter for it");
    }

    #[tokio::test]
    async fn environment_rows_name_the_bound_views() {
        let (st, cfg) = (Store::open_in_memory().await.unwrap(), Config::default());
        let lib = st.ensure_library("lab").await.unwrap();
        let dev = st.create_env("lab", "Dev", lib, false).await.unwrap();
        let (os, web) = (st.create_cv("lab", "os", "").await.unwrap(), st.create_cv("lab", "web", "").await.unwrap());
        let (o1, o2, w1) = (
            st.begin_version(os, "", &[]).await.unwrap(),
            st.begin_version(os, "", &[]).await.unwrap(),
            st.begin_version(web, "", &[]).await.unwrap(),
        );
        st.set_binding(lib, os, o2.id, "active").await.unwrap();
        st.set_binding(dev, os, o1.id, "active").await.unwrap();
        st.set_binding(lib, web, w1.id, "partial").await.unwrap();
        let (rows, _) = load(&Kind::Environments, None, Ok(&st), "lab", "lab", &cfg, 0, "").await.unwrap();
        let bound: Vec<(String, String)> = rows.iter().map(|r| (s(r, "name"), s(r, "bound"))).collect();
        assert_eq!(bound, [("Library".to_string(), "os:v2, web:v1 (partial)".to_string()), ("Dev".to_string(), "os:v1".to_string())]);
    }

    #[tokio::test]
    async fn a_publishing_version_without_a_live_lock_shows_interrupted() {
        let (st, cfg) = (Store::open_in_memory().await.unwrap(), Config::default());
        let cv = st.create_cv("lab", "os", "").await.unwrap();
        st.begin_version(cv, "", &[]).await.unwrap();
        let k = Kind::CvVersions { cv, name: "os".into() };
        let row = async |k: &Kind| load(k, None, Ok(&st), "lab", "lab", &cfg, 0, "").await.unwrap().0.remove(0);
        let state = async || {
            let (v, view) = (row(&k).await, row(&Kind::ContentViews).await);
            assert_eq!(v["state"], view["versions"][0]["state"], "the details of the view show the same state");
            v["state"].clone()
        };
        assert_eq!(state().await, "interrupted", "no lock: the client that ran it ended");
        let now = crate::cv::unix_now();
        st.try_lock(cv, "a@h", "t1", "publish", now).await.unwrap().unwrap();
        assert_eq!(state().await, "publishing");
        st.renew_lock(cv, "t1", now - crate::store::LOCK_TTL_SECS - 1).await.unwrap();
        assert_eq!(state().await, "interrupted", "lock expired");
    }

    #[tokio::test]
    async fn a_promoting_binding_without_a_live_lock_shows_interrupted() {
        let (st, cfg) = (Store::open_in_memory().await.unwrap(), Config::default());
        let lib = st.ensure_library("lab").await.unwrap();
        let cv = st.create_cv("lab", "os", "").await.unwrap();
        let v = st.begin_version(cv, "", &[]).await.unwrap();
        st.set_binding(lib, cv, v.id, "promoting").await.unwrap();
        let bound = async |k: Kind| s(&load(&k, None, Ok(&st), "lab", "lab", &cfg, 0, "").await.unwrap().0[0], "bound");
        let both = async || (bound(Kind::ContentViews).await, bound(Kind::Environments).await);
        assert_eq!(both().await, ("Library:v1 (interrupted)".into(), "os:v1 (interrupted)".into()), "quit, crash or Connect");
        st.try_lock(cv, "a@h", "t1", "promote to Library", crate::cv::unix_now()).await.unwrap().unwrap();
        assert_eq!(both().await, ("Library:v1 (promoting)".into(), "os:v1 (promoting)".into()), "the promotion still runs");
    }

    #[tokio::test]
    async fn cv_versions_report_unreadable_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cv.db");
        let (st, cfg) = (Store::open(&path).await.unwrap(), Config::default());
        let cv = st.create_cv("lab", "os", "").await.unwrap();
        let repo = crate::store::CvRepo { repo_href: "/r/1/".into(), repo_name: "os".into(), repo_type: RepoType::Rpm, filters: vec![] };
        st.begin_version(cv, "", &[repo]).await.unwrap();
        let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&path);
        let other = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
        sqlx::query("UPDATE cv_version_repos SET filters = 'x'").execute(&other).await.unwrap();
        let k = Kind::CvVersions { cv, name: "os".into() };
        let e = format!("{:#}", load(&k, None, Ok(&st), "lab", "lab", &cfg, 0, "").await.unwrap_err());
        assert!(e.contains("invalid filter rules"), "{e}");
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
    fn deb_repositories_upload_and_have_their_own_screens() {
        for t in RepoType::ALL {
            assert!(Kind::Repos(t, false).actions().contains(&Act::Upload), "{t:?}");
        }
        assert!(NAV.iter().any(|(l, k)| *l == "Deb Repositories" && *k == Kind::Repos(RepoType::Deb, false)));
        let names = |k: &Kind| k.columns().iter().map(|c| c.title).collect::<Vec<_>>();
        let content = |ctype| Kind::Content { version: "/pulp/api/v3/repositories/deb/apt/1/versions/1/".into(), ctype };
        assert_eq!(
            RepoType::Deb.content_types()[..4].iter().map(|c| c.path).collect::<Vec<_>>(),
            ["content/deb/packages/", "content/deb/releases/", "content/deb/release_components/", "content/deb/source_packages/"]
        );
        assert_eq!(names(&content(0)), ["package", "version", "architecture", "sha256"]);
        assert_eq!(names(&content(1)), ["distribution", "codename", "suite", "version"]);
        assert_eq!(names(&content(2)), ["distribution", "component"]);
        assert_eq!(names(&content(3)), ["href", "created"]);
        let src = |ctype| content(ctype).source().unwrap();
        assert_eq!((src(0).2, src(1).2, src(3).2), (Some("package__contains"), None, None));
        assert!(src(0).1.contains(&("ordering", "package".to_string())));
        let row = json!({ "package": "p", "version": "1.0-1", "architecture": "amd64", "sha256": "0123456789abcdef0123" });
        assert_eq!(content(0).columns().iter().map(|c| (c.get)(&row)).collect::<Vec<_>>(), ["p", "1.0-1", "amd64", "0123456789ab"]);
        let pubs = Kind::Publications(RepoType::Deb).columns();
        assert!(pubs.iter().all(|c| c.title != "distributions"), "the Deb publication has no distributions field");
    }

    #[test]
    fn rpm_comps_content_shows_id_and_name() {
        let content = |ctype| Kind::Content { version: "/pulp/api/v3/repositories/rpm/rpm/1/versions/1/".into(), ctype };
        for (ctype, label) in [(3, "package groups"), (8, "package categories"), (9, "package environments")] {
            assert_eq!(content(ctype).title(), format!("Content {label} of 1@v1  [t: type]"));
            assert_eq!(content(ctype).columns().iter().map(|c| c.title).collect::<Vec<_>>(), ["id", "name"], "{label}");
        }
    }

    #[test]
    fn deb_content_adds_to_another_repository_only_binary_packages() {
        let content = |t: &str, ctype| Kind::Content { version: format!("/pulp/api/v3/repositories/{t}/1/versions/1/"), ctype };
        assert!(content("deb/apt", 0).actions().contains(&Act::AddToRepo));
        for ctype in 1..RepoType::Deb.content_types().len() {
            let acts = content("deb/apt", ctype).actions();
            assert!(!acts.contains(&Act::AddToRepo), "{}", RepoType::Deb.content_types()[ctype].label);
            assert!(acts.contains(&Act::RemoveContent));
        }
        assert!(content("rpm/rpm", 1).actions().contains(&Act::AddToRepo), "RPM advisories go through modify/");
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
        assert!(Act::PruneOrphans.mutates(&Kind::Versions(None), false), "deletes content on the server");
        assert!(Act::Restore.mutates(&Kind::Versions(None), false), "makes a repository version");
        assert_eq!((Act::Restore.key(), Act::Restore.label()), ('R', "Restore as latest version (new version)"), "the menu row");
        assert!(!Act::ImportCv.mutates(&Kind::Profiles, false));
        assert!(Act::MigrateDb.mutates(&Kind::Profiles, false) && Act::CopyDb.mutates(&Kind::Profiles, false));
        assert!(!Kind::Profiles.actions().iter().any(|a| matches!(a, Act::MigrateDb | Act::CopyDb)), "offered on Connect and save only");
    }

    #[test]
    fn exporter_screens_list_exporters_and_their_exports() {
        let k = Kind::PulpExporters;
        assert!(NAV.iter().any(|(l, n)| *l == "Exporters" && *n == k));
        assert_eq!(k.source().unwrap().0, "exporters/core/pulp/");
        assert!(k.searchable());
        assert_eq!(k.actions(), [Act::Export, Act::Exports, Act::Create, Act::Edit, Act::Delete, Act::Inspect]);
        assert!(Act::Export.mutates(&k, false) && !Act::Exports.mutates(&k, false));
        let cells = |k: &Kind, row: Value| k.columns().iter().map(|c| (c.get)(&row)).collect::<Vec<_>>();
        let row = json!({ "name": "dvd", "path": "/var/lib/pulp/exports/dvd", "repositories": ["/r/1/", "/r/2/"] });
        assert_eq!(cells(&k, row), ["dvd", "/var/lib/pulp/exports/dvd", "2"]);

        let e = Kind::Exports { exporter: "/pulp/api/v3/exporters/core/pulp/1/".into(), name: "dvd".into() };
        assert_eq!(e.title(), "Exports of dvd");
        assert_eq!(e.source().unwrap().0, "/pulp/api/v3/exporters/core/pulp/1/exports/", "the href, not below the API root");
        assert_eq!(e.actions(), [Act::Delete, Act::Inspect]);
        let row = json!({ "pulp_created": "2026-11-02T10:00:00.123Z", "exported_resources": ["/v/1/", "/v/2/"],
            "toc_info": { "file": "/var/lib/pulp/exports/dvd/export-9-toc.json" } });
        assert_eq!(cells(&e, row), ["2026-11-02 10:00:00", "2", "/var/lib/pulp/exports/dvd/export-9-toc.json"]);

        let mut caps = Capabilities::default();
        caps.paths.insert("status/".into());
        assert_eq!(k.unsupported(&caps).as_deref(), Some("not supported by this server: Pulp export (exporters/core/pulp/)"));
        caps.paths.insert("exporters/core/pulp/".into());
        assert_eq!(k.unsupported(&caps), None);
    }

    #[test]
    fn filesystem_exporters_are_the_other_type_of_the_exporters_screen() {
        let k = Kind::FsExporters;
        assert_eq!((Kind::PulpExporters.toggle(), k.toggle()), (Some(Kind::FsExporters), Some(Kind::PulpExporters)));
        assert!(Kind::PulpExporters.title().ends_with("[t: toggle type]") && k.title().ends_with("[t: toggle type]"));
        assert_eq!(k.source().unwrap().0, "exporters/core/filesystem/");
        assert_eq!(k.actions(), Kind::PulpExporters.actions());
        let row = json!({ "name": "web", "path": "/var/lib/pulp/exports/web", "method": "hardlink" });
        assert_eq!(k.columns().iter().map(|c| (c.get)(&row)).collect::<Vec<_>>(), ["web", "/var/lib/pulp/exports/web", "hardlink"]);
        let mut caps = Capabilities::default();
        caps.paths.insert("exporters/core/pulp/".into());
        let e = k.unsupported(&caps);
        assert_eq!(e.as_deref(), Some("not supported by this server: filesystem export (exporters/core/filesystem/)"));
        caps.paths.insert("exporters/core/filesystem/".into());
        assert_eq!(k.unsupported(&caps), None);
    }

    #[test]
    fn importer_screens_list_importers_and_their_imports() {
        let k = Kind::Importers;
        assert!(NAV.iter().any(|(l, n)| *l == "Importers" && *n == k));
        assert_eq!(k.source().unwrap().0, "importers/core/pulp/");
        assert_eq!(k.actions(), [Act::Import, Act::Imports, Act::Create, Act::Edit, Act::Delete, Act::Inspect]);
        assert!(Act::Import.mutates(&k, false) && !Act::Imports.mutates(&k, false));
        let cells = |k: &Kind, row: Value| k.columns().iter().map(|c| (c.get)(&row)).collect::<Vec<_>>();
        assert_eq!(cells(&k, json!({ "name": "dvd", "repo_mapping": { "web": "web-copy", "os": "os" } })), ["dvd", "os=os,web=web-copy"]);

        let i = Kind::Imports { importer: "/pulp/api/v3/importers/core/pulp/1/".into(), name: "dvd".into() };
        assert_eq!((i.title(), i.source().unwrap().0), ("Imports of dvd".into(), "/pulp/api/v3/importers/core/pulp/1/imports/".into()));
        assert_eq!(i.actions(), [Act::Inspect]);
        let row = json!({ "pulp_created": "2026-11-02T10:00:00Z", "params": { "path": "/x/export-1-toc.json" }, "task": "/t/0190abcd-1234-5678/" });
        assert_eq!(cells(&i, row), ["2026-11-02 10:00:00", "/x/export-1-toc.json", "0190abcd-1234"]);

        let mut caps = Capabilities::default();
        caps.paths.insert("importers/core/pulp/".into());
        let e = k.unsupported(&caps);
        assert_eq!(e.as_deref(), Some("not supported by this server: Pulp import (importers/core/pulp/, import-check/)"));
        caps.paths.insert("importers/core/pulp/import-check/".into());
        assert_eq!(k.unsupported(&caps), None);
    }

    #[test]
    fn acs_screens_list_each_type_and_need_its_endpoint() {
        let k = Kind::Acs(RepoType::Rpm);
        assert!(NAV.iter().any(|(l, n)| *l == "Alternate Sources" && *n == k));
        assert_eq!(k.title(), "RPM Alternate Content Sources  [t: toggle type]");
        assert_eq!(k.toggle(), Some(Kind::Acs(RepoType::File)));
        let paths: Vec<String> = RepoType::ALL.iter().map(|t| Kind::Acs(*t).source().unwrap().0).collect();
        assert_eq!(paths, ["acs/rpm/rpm/", "acs/file/file/", "acs/deb/deb/"]);
        assert_eq!(k.actions(), [Act::Refresh, Act::Create, Act::Edit, Act::Delete, Act::Inspect]);
        assert!(Act::Refresh.mutates(&k, false), "a refresh runs sync tasks");
        let mut caps = Capabilities::default();
        caps.versions.insert("rpm".into(), "3.39.5".into());
        caps.load_openapi(&json!({ "paths": { "/pulp/api/v3/remotes/rpm/rpm/": {} } }));
        assert_eq!(k.unsupported(&caps).as_deref(), Some("not supported by this server: RPM alternate content sources (acs/rpm/rpm/)"));
        caps.paths.insert("acs/rpm/rpm/".into());
        assert_eq!(k.unsupported(&caps), None);
        assert_eq!(Kind::Acs(RepoType::Deb).unsupported(&caps).as_deref(), Some("not supported by this server: pulp_deb plugin"));
        let row = json!({ "name": "cdn", "paths": ["rpm/", "rpm2/"], "last_refreshed": "2026-11-02T10:00:00.123Z",
            "remote": "/pulp/api/v3/remotes/rpm/rpm/0190abcd-1234-5678/" });
        let cells: Vec<(&str, String)> = k.columns().iter().map(|c| (c.title, (c.get)(&row))).collect();
        assert_eq!(
            cells,
            [
                ("name", "cdn".into()),
                ("paths", "rpm/, rpm2/".into()),
                ("last refreshed", "2026-11-02 10:00:00".into()),
                ("remote", "0190abcd-1234".into())
            ]
        );
    }

    #[test]
    fn rpm_uln_remotes_have_their_own_screen_and_need_the_endpoint() {
        let k = Kind::Remotes(RepoType::Rpm, false);
        assert!(NAV.iter().any(|(l, n)| *l == "Remotes" && *n == k));
        let uln = k.toggle().unwrap();
        assert_eq!(uln, Kind::Remotes(RepoType::Rpm, true));
        assert_eq!(uln.title(), "RPM ULN Remotes  [t: toggle type]");
        assert_eq!((k.source().unwrap().0, uln.source().unwrap().0), ("remotes/rpm/rpm/".into(), "remotes/rpm/uln/".into()));
        assert_eq!(uln.toggle(), Some(Kind::Remotes(RepoType::File, false)));
        assert_eq!(Kind::Remotes(RepoType::Deb, false).toggle(), Some(k.clone()), "Deb has one remote type");
        assert_eq!(uln.actions(), [Act::Create, Act::Edit, Act::Delete, Act::Inspect]);
        assert!(Act::Create.mutates(&uln, false));
        let mut caps = Capabilities::default();
        caps.versions.insert("rpm".into(), "3.39.5".into());
        caps.load_openapi(&json!({ "paths": { "/pulp/api/v3/remotes/rpm/rpm/": {} } }));
        assert_eq!(uln.unsupported(&caps).as_deref(), Some("not supported by this server: RPM ULN remotes (remotes/rpm/uln/)"));
        assert_eq!(k.unsupported(&caps), None);
        caps.paths.insert("remotes/rpm/uln/".into());
        assert_eq!(uln.unsupported(&caps), None);
        caps.versions.clear();
        assert_eq!(uln.unsupported(&caps).as_deref(), Some("not supported by this server: pulp_rpm plugin"));
    }

    #[test]
    fn file_git_remotes_follow_file_remotes_and_need_the_endpoint() {
        let git = Kind::Remotes(RepoType::File, false).toggle().unwrap();
        assert_eq!(git, Kind::Remotes(RepoType::File, true));
        assert_eq!(git.title(), "File Git Remotes  [t: toggle type]");
        assert_eq!(git.source().unwrap().0, "remotes/file/git/");
        assert_eq!(git.toggle(), Some(Kind::Remotes(RepoType::Deb, false)));
        let row = json!({ "name": "docs", "git_ref": "v1", "url": "https://git.example/docs.git", "policy": "immediate" });
        let cells: Vec<(&str, String)> = git.columns().iter().map(|c| (c.title, (c.get)(&row))).collect();
        assert_eq!(cells, [("name", "docs".into()), ("git ref", "v1".into()), ("url", "https://git.example/docs.git".into())]);
        // pulp_file before pulpcore 3.123 (3.95.3 tested) has no Git remotes.
        let mut caps = Capabilities::default();
        caps.versions.insert("file".into(), "3.95.3".into());
        caps.load_openapi(&json!({ "paths": { "/pulp/api/v3/remotes/file/file/": {} } }));
        assert_eq!(git.unsupported(&caps).as_deref(), Some("not supported by this server: File Git remotes (remotes/file/git/)"));
        caps.paths.insert("remotes/file/git/".into());
        assert_eq!(git.unsupported(&caps), None);
    }

    #[test]
    fn short_ids() {
        assert_eq!(short("/pulp/api/v3/repositories/rpm/rpm/0190abcd-1234/versions/3/"), "0190abcd@v3");
        assert_eq!(short("/pulp/api/v3/publications/file/file/0190abcd-1234-5678/"), "0190abcd-1234");
    }
}
