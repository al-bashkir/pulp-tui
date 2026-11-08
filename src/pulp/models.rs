//! Typed views of the Pulp objects the application logic depends on. Unknown fields are
//! kept in `extra` so detail views never silently drop server metadata.

use super::caps::Feature;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Repository plugin types supported end-to-end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoType {
    Rpm,
    File,
    Deb,
}

impl RepoType {
    pub const ALL: [RepoType; 3] = [RepoType::Rpm, RepoType::File, RepoType::Deb];

    /// Pulp status component name of the plugin.
    pub fn component(self) -> &'static str {
        match self {
            RepoType::Rpm => "rpm",
            RepoType::File => "file",
            RepoType::Deb => "deb",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            RepoType::Rpm => "RPM",
            RepoType::File => "File",
            RepoType::Deb => "Deb",
        }
    }
    /// The next type in `ALL`, wrapping around (type toggles on screens).
    pub fn next(self) -> RepoType {
        let i = Self::ALL.iter().position(|t| *t == self).map_or(0, |i| i + 1);
        Self::ALL[i % Self::ALL.len()]
    }
    /// Server capability the plugin needs.
    pub fn feature(self) -> Feature {
        match self {
            RepoType::Rpm => Feature::Rpm,
            RepoType::File => Feature::File,
            RepoType::Deb => Feature::Deb,
        }
    }
    /// OpenAPI schema of the plugin's repository serializer.
    pub fn repository_schema(self) -> &'static str {
        match self {
            RepoType::Rpm => "rpm.RpmRepository",
            RepoType::File => "file.FileRepository",
            RepoType::Deb => "deb.AptRepository",
        }
    }
    /// OpenAPI schema of the plugin's remote serializer.
    pub fn remote_schema(self) -> &'static str {
        match self {
            RepoType::Rpm => "rpm.RpmRemote",
            RepoType::File => "file.FileRemote",
            RepoType::Deb => "deb.AptRemote",
        }
    }
    /// OpenAPI schema of the plugin's publication serializer.
    pub fn publication_schema(self) -> &'static str {
        match self {
            RepoType::Rpm => "rpm.RpmPublication",
            RepoType::File => "file.FilePublication",
            RepoType::Deb => "deb.AptPublication",
        }
    }
    /// OpenAPI schema of the plugin's distribution serializer.
    pub fn distribution_schema(self) -> &'static str {
        match self {
            RepoType::Rpm => "rpm.RpmDistribution",
            RepoType::File => "file.FileDistribution",
            RepoType::Deb => "deb.AptDistribution",
        }
    }
    pub fn repositories(self) -> &'static str {
        match self {
            RepoType::Rpm => "repositories/rpm/rpm/",
            RepoType::File => "repositories/file/file/",
            RepoType::Deb => "repositories/deb/apt/",
        }
    }
    pub fn remotes(self) -> &'static str {
        match self {
            RepoType::Rpm => "remotes/rpm/rpm/",
            RepoType::File => "remotes/file/file/",
            RepoType::Deb => "remotes/deb/apt/",
        }
    }
    /// The plugin's second remote type: label, collection, OpenAPI schema. Its repositories sync from it too
    /// (`REMOTE_TYPES`); an ACS does not take it.
    pub fn alt_remote(self) -> Option<(&'static str, &'static str, &'static str)> {
        match self {
            RepoType::Rpm => Some(("ULN", "remotes/rpm/uln/", "rpm.UlnRemote")),
            RepoType::File => Some(("Git", "remotes/file/git/", "file.FileGitRemote")),
            RepoType::Deb => None,
        }
    }
    pub fn publications(self) -> &'static str {
        match self {
            RepoType::Rpm => "publications/rpm/rpm/",
            RepoType::File => "publications/file/file/",
            RepoType::Deb => "publications/deb/apt/",
        }
    }
    pub fn distributions(self) -> &'static str {
        match self {
            RepoType::Rpm => "distributions/rpm/rpm/",
            RepoType::File => "distributions/file/file/",
            RepoType::Deb => "distributions/deb/apt/",
        }
    }
    /// Alternate content sources of the plugin.
    pub fn acs(self) -> &'static str {
        match self {
            RepoType::Rpm => "acs/rpm/rpm/",
            RepoType::File => "acs/file/file/",
            RepoType::Deb => "acs/deb/deb/",
        }
    }
    /// OpenAPI path of the repair of a repository version.
    pub fn version_repair(self) -> &'static str {
        match self {
            RepoType::Rpm => "{rpm_rpm_repository_version_href}repair/",
            RepoType::File => "{file_file_repository_version_href}repair/",
            RepoType::Deb => "{deb_apt_repository_version_href}repair/",
        }
    }
    /// Primary content type (packages / files).
    pub fn content(self) -> &'static str {
        match self {
            RepoType::Rpm => "content/rpm/packages/",
            RepoType::File => "content/file/files/",
            RepoType::Deb => "content/deb/packages/",
        }
    }
    /// Content types of a repository: the Content screen lists them, a version compare covers them.
    pub fn content_types(self) -> &'static [ContentType] {
        match self {
            RepoType::Rpm => RPM_TYPES,
            RepoType::File => FILE_TYPES,
            RepoType::Deb => DEB_TYPES,
        }
    }
    /// Detect the type from any rpm/file/deb href (repository, remote, version, publication...).
    pub fn from_href(href: &str) -> Option<Self> {
        if href.contains("/rpm/rpm/") || href.contains("/rpm/packages/") {
            Some(RepoType::Rpm)
        } else if href.contains("/file/file/") || href.contains("/file/files/") {
            Some(RepoType::File)
        } else if href.contains("/deb/apt/") || href.contains("/deb/packages/") {
            Some(RepoType::Deb)
        } else {
            None
        }
    }
}

/// One content type: label and API path, and the `fields` that the human-readable `key` of a unit reads.
pub struct ContentType {
    pub label: &'static str,
    pub path: &'static str,
    pub fields: &'static str,
    pub key: fn(&Value) -> String,
}

fn text(v: &Value, k: &str) -> String {
    match &v[k] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        x => x.to_string(),
    }
}

/// `key` of a unit read as `T`; the raw JSON when it does not parse, so a compare never drops a unit.
fn typed<'a, T: Deserialize<'a>>(v: &'a Value, key: impl Fn(T) -> String) -> String {
    T::deserialize(v).map_or_else(|_| v.to_string(), key)
}

/// Last part of the unit's `pulp_href`: it keeps units apart whose API shows no unique field.
fn unit_id(v: &Value) -> String {
    text(v, "pulp_href").trim_end_matches('/').rsplit('/').next().unwrap_or_default().to_string()
}

/// `relative_path` and the unit id: the API shows no digest of these units (or one that is `null` until download).
fn path_id(v: &Value) -> String {
    format!("{}  ({})", text(v, "relative_path"), unit_id(v))
}

const RPM_TYPES: &[ContentType] = &[
    ContentType {
        label: "packages",
        path: "content/rpm/packages/",
        fields: "pulp_href,name,epoch,version,release,arch",
        key: |v| format!("package {}", typed(v, |p: Package| p.nevra())),
    },
    ContentType { label: "advisories", path: "content/rpm/advisories/", fields: "id", key: |v| format!("advisory {}", text(v, "id")) },
    ContentType {
        label: "modulemds",
        path: "content/rpm/modulemds/",
        fields: "name,stream,version,context,arch",
        key: |v| format!("modulemd {}", ["name", "stream", "version", "context", "arch"].map(|k| text(v, k)).join(":")),
    },
    ContentType {
        label: "package groups",
        path: "content/rpm/packagegroups/",
        fields: "id",
        key: |v| format!("package group {}", text(v, "id")),
    },
    ContentType {
        label: "distribution trees",
        path: "content/rpm/distribution_trees/",
        fields: "release_name,release_version,arch,build_timestamp",
        key: |v| {
            let [n, ver, arch, ts] = ["release_name", "release_version", "arch", "build_timestamp"].map(|k| text(v, k));
            format!("distribution tree {n} {ver} {arch} (build {ts})")
        },
    },
    ContentType {
        label: "repo metadata files",
        path: "content/rpm/repo_metadata_files/",
        fields: "data_type,checksum",
        key: |v| format!("repo metadata file {}  ({:.12})", text(v, "data_type"), text(v, "checksum")),
    },
    ContentType {
        label: "modulemd defaults",
        path: "content/rpm/modulemd_defaults/",
        fields: "module,stream",
        key: |v| format!("modulemd defaults {} (stream {})", text(v, "module"), text(v, "stream")),
    },
    ContentType {
        label: "modulemd obsoletes",
        path: "content/rpm/modulemd_obsoletes/",
        fields: "module_name,module_stream,modified",
        key: |v| format!("modulemd obsolete {}:{} (modified {})", text(v, "module_name"), text(v, "module_stream"), text(v, "modified")),
    },
    ContentType {
        label: "package categories",
        path: "content/rpm/packagecategories/",
        fields: "id",
        key: |v| format!("package category {}", text(v, "id")),
    },
    ContentType {
        label: "package environments",
        path: "content/rpm/packageenvironments/",
        fields: "id",
        key: |v| format!("package environment {}", text(v, "id")),
    },
    ContentType {
        label: "package langpacks",
        path: "content/rpm/packagelangpacks/",
        fields: "digest",
        key: |v| format!("package langpacks {:.12}", text(v, "digest")),
    },
];

const FILE_TYPES: &[ContentType] = &[ContentType {
    label: "files",
    path: "content/file/files/",
    fields: "pulp_href,relative_path,sha256",
    key: |v| format!("file {}", typed(v, |f: FileContent| super::file::key(&f))),
}];

const DEB_TYPES: &[ContentType] = &[
    ContentType {
        label: "packages",
        path: "content/deb/packages/",
        fields: "pulp_href,package,version,architecture",
        key: |v| format!("package {}", typed(v, |p: DebPackage| p.key())),
    },
    ContentType {
        label: "releases",
        path: "content/deb/releases/",
        fields: "distribution,codename,suite",
        key: |v| format!("release {} (codename {}, suite {})", text(v, "distribution"), text(v, "codename"), text(v, "suite")),
    },
    ContentType {
        label: "release components",
        path: "content/deb/release_components/",
        fields: "distribution,component",
        key: |v| format!("release component {}/{}", text(v, "distribution"), text(v, "component")),
    },
    // pulp_deb 3.11 answers HTTP 500 to `fields` without `artifacts` (its serializer sets that field read-only).
    ContentType {
        label: "source packages",
        path: "content/deb/source_packages/",
        fields: "source,version,artifacts",
        key: |v| format!("source package {}_{}", text(v, "source"), text(v, "version")),
    },
    ContentType {
        label: "generic contents",
        path: "content/deb/generic_contents/",
        fields: "relative_path,pulp_href",
        key: |v| format!("generic content {}", path_id(v)),
    },
    ContentType {
        label: "installer packages",
        path: "content/deb/installer_packages/",
        fields: "pulp_href,package,version,architecture",
        key: |v| format!("installer package {}", typed(v, |p: DebPackage| p.key())),
    },
    ContentType {
        label: "installer file indices",
        path: "content/deb/installer_file_indices/",
        fields: "relative_path,pulp_href",
        key: |v| format!("installer file index {}", path_id(v)),
    },
    ContentType {
        label: "package indices",
        path: "content/deb/package_indices/",
        fields: "relative_path,pulp_href",
        key: |v| format!("package index {}", path_id(v)),
    },
    ContentType {
        label: "source indices",
        path: "content/deb/source_indices/",
        fields: "relative_path,pulp_href",
        key: |v| format!("source index {}", path_id(v)),
    },
    ContentType {
        label: "release architectures",
        path: "content/deb/release_architectures/",
        fields: "distribution,architecture",
        key: |v| format!("release architecture {}/{}", text(v, "distribution"), text(v, "architecture")),
    },
    ContentType {
        label: "release files",
        path: "content/deb/release_files/",
        fields: "relative_path,pulp_href",
        key: |v| format!("release file {}", path_id(v)),
    },
    // A package can have one record per index architecture, and the API does not show that field.
    ContentType {
        label: "package release components",
        path: "content/deb/package_release_components/",
        fields: "pulp_href",
        key: |v| format!("package release component {}", unit_id(v)),
    },
    ContentType {
        label: "source release components",
        path: "content/deb/source_release_components/",
        fields: "pulp_href",
        key: |v| format!("source release component {}", unit_id(v)),
    },
];

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct ProgressReport {
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub state: String,
    pub total: Option<u64>,
    pub done: Option<u64>,
    pub suffix: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Task {
    pub pulp_href: String,
    pub state: String,
    #[serde(default)]
    pub name: String,
    pub pulp_created: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    #[serde(default)]
    pub progress_reports: Vec<ProgressReport>,
    #[serde(default)]
    pub created_resources: Vec<Option<String>>,
    pub error: Option<Value>,
    pub task_group: Option<String>,
    #[serde(default)]
    pub reserved_resources_record: Vec<String>,
}

pub const TASK_FINAL_STATES: [&str; 4] = ["completed", "failed", "canceled", "skipped"];

/// pulp-tui's final state of a task, or a task group, that Pulp no longer has (purged): how it ended is unknown.
pub const GONE: &str = "gone";

/// An idle task group whose newest task ended this long ago is final (`TaskGroup::is_final`). Long enough for a
/// client clock that is some minutes ahead of Pulp's; a parent task dispatches its children before it ends.
const IDLE_FINAL_SECS: u64 = 600;

impl Task {
    pub fn is_final(&self) -> bool {
        TASK_FINAL_STATES.contains(&self.state.as_str())
    }
    pub fn error_message(&self) -> Option<String> {
        let e = self.error.as_ref()?;
        Some(e.get("description").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| e.to_string()))
    }
    pub fn created(&self) -> impl Iterator<Item = &str> {
        self.created_resources.iter().flatten().map(String::as_str)
    }
    /// First created resource whose href contains `needle` (e.g. "/versions/").
    pub fn created_matching(&self, needle: &str) -> Option<String> {
        self.created().find(|h| h.contains(needle)).map(str::to_string)
    }
}

/// Seconds since the epoch of an API time `YYYY-MM-DDTHH:MM:SS[.ffffff](Z|±HH:MM)`: Pulp prints its `TIME_ZONE`.
fn epoch_secs(t: &str) -> Option<i64> {
    let n = |s: &str, r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (m, d) = (n(t, 5..7)?, n(t, 8..10)?);
    let y = n(t, 0..4)? - i64::from(m <= 2);
    // Days of the civil date: the inverse of `store::rfc3339`.
    let (era, doy) = (y.div_euclid(400), (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1);
    let yoe = y - era * 400;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    let zone = t.get(19..)?.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let hm = || Some(n(zone, 1..3)? * 3600 + n(zone, 4..6)? * 60);
    let offset = match zone.get(..1)? {
        "Z" if zone.len() == 1 => 0,
        "+" => hm()?,
        "-" => -hm()?,
        _ => return None,
    };
    Some(days * 86_400 + n(t, 11..13)? * 3600 + n(t, 14..16)? * 60 + n(t, 17..19)? - offset)
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct TaskGroup {
    pub pulp_href: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub all_tasks_dispatched: bool,
    #[serde(default)]
    pub waiting: u64,
    #[serde(default)]
    pub running: u64,
    #[serde(default)]
    pub completed: u64,
    #[serde(default)]
    pub failed: u64,
    #[serde(default)]
    pub canceled: u64,
    #[serde(default)]
    pub skipped: u64,
    #[serde(default)]
    pub canceling: u64,
    #[serde(default)]
    pub tasks: Vec<Value>,
}

impl TaskGroup {
    pub fn active(&self) -> u64 {
        self.waiting + self.running + self.canceling
    }
    pub fn done(&self) -> u64 {
        self.completed + self.failed + self.canceled + self.skipped
    }
    pub fn counts(&self) -> String {
        format!("completed {}  failed {}  canceled {}  skipped {}", self.completed, self.failed, self.canceled, self.skipped)
    }
    /// `all_tasks_dispatched` is not reliable (pulpcore 3.123 leaves it false after a
    /// replication, an import or an RPM prune), so a failure, the replication finalizer or the completed
    /// `pulp_import` or `prune_packages` (each dispatches every repository task) also ends the group. An
    /// ACS refresh dispatches all its tasks before it answers and never sets the flag. Last, a group whose
    /// newest task ended `IDLE_FINAL_SECS` ago is final, so the Task Center stops polling an unknown kind.
    /// A group with no task at all is final too: purge deleted them (only final ones; `Task.task_group` is `SET_NULL`).
    pub fn is_final(&self) -> bool {
        let last = |n: &str| ["finalize_replication", "importer.pulp_import", "prune.prune_packages"].iter().any(|s| n.ends_with(s));
        let finalized = self.tasks.iter().any(|t| t["name"].as_str().is_some_and(last) && t["state"] == "completed");
        let refresh = self.description.starts_with("Refreshing ") && self.description.to_lowercase().contains("alternate content source");
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        // shortcut: a group has no time in Pulp's answer, so an empty one ends at once, not after IDLE_FINAL_SECS; one
        // seen in the instant between its create and its first dispatch ends too. Wait when Pulp shows `pulp_created`.
        self.purged()
            || self.active() == 0
                && self.done() > 0
                && (self.all_tasks_dispatched || self.failed + self.canceled > 0 || finalized || refresh || self.idle_long(now))
    }
    /// No task and no count: a purge deleted the tasks, and how they ended is unknown.
    fn purged(&self) -> bool {
        self.tasks.is_empty() && self.done() + self.active() == 0
    }
    /// The newest `finished_at` (`epoch_secs`) is `IDLE_FINAL_SECS` or more before `now` (seconds since the epoch,
    /// client clock).
    fn idle_long(&self, now: u64) -> bool {
        let newest = self.tasks.iter().filter_map(|t| epoch_secs(t["finished_at"].as_str()?)).max();
        newest.is_some_and(|f| f + IDLE_FINAL_SECS as i64 <= now as i64)
    }
    /// How a final group ended: a failed task fails it; a canceled or skipped task is not a success either, nor is
    /// a group whose tasks were purged (`GONE`).
    pub fn outcome(&self) -> &'static str {
        if self.purged() {
            GONE
        } else if self.failed > 0 {
            "failed"
        } else if self.canceled > 0 {
            "canceled"
        } else if self.skipped > 0 {
            "skipped"
        } else {
            "completed"
        }
    }
    /// The group as one Task Center task. Its state is `outcome` once final; the counts are the error when not completed.
    pub fn as_task(&self) -> Task {
        let state = match self.is_final() {
            true => self.outcome(),
            false if self.running + self.canceling > 0 => "running",
            false => "waiting",
        };
        let progress = ProgressReport {
            message: "tasks".into(),
            state: state.into(),
            done: Some(self.done()),
            total: Some(self.tasks.len() as u64),
            ..Default::default()
        };
        Task {
            pulp_href: self.pulp_href.clone(),
            state: state.into(),
            name: self.description.clone(),
            progress_reports: vec![progress],
            error: (self.is_final() && state != "completed").then(|| {
                let why = if state == GONE { "its tasks were purged".into() } else { self.counts() };
                serde_json::json!({ "description": why })
            }),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Repository {
    pub pulp_href: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub latest_version_href: Option<String>,
    #[serde(default)]
    pub retain_repo_versions: Option<u64>,
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default)]
    pub pulp_labels: BTreeMap<String, String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct CountHref {
    pub count: u64,
    pub href: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct ContentSummary {
    #[serde(default)]
    pub added: BTreeMap<String, CountHref>,
    #[serde(default)]
    pub removed: BTreeMap<String, CountHref>,
    #[serde(default)]
    pub present: BTreeMap<String, CountHref>,
}

impl ContentSummary {
    /// Units in the version, of every content type.
    pub fn present_count(&self) -> u64 {
        self.present.values().map(|x| x.count).sum()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RepoVersion {
    pub pulp_href: String,
    pub number: u64,
    pub pulp_created: Option<String>,
    pub repository: Option<String>,
    pub base_version: Option<String>,
    #[serde(default)]
    pub content_summary: ContentSummary,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Publication {
    pub pulp_href: String,
    pub repository_version: Option<String>,
    pub repository: Option<String>,
    pub pulp_created: Option<String>,
    #[serde(default)]
    pub signing_service: Option<String>,
    /// File only: the manifest name it serves (`None`: no manifest, or another type).
    #[serde(default)]
    pub manifest: Option<String>,
    /// Newer pulpcore (checkpoint publications); older servers lack the field.
    #[serde(default)]
    pub checkpoint: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Distribution {
    pub pulp_href: String,
    pub name: String,
    pub base_path: String,
    pub base_url: Option<String>,
    #[serde(default)]
    pub publication: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    /// Newer pulpcore: serve one repository version directly (used by native replication).
    #[serde(default)]
    pub repository_version: Option<String>,
    /// Newer pulpcore: serves the checkpoint publications of `repository`.
    #[serde(default)]
    pub checkpoint: bool,
    #[serde(default)]
    pub pulp_labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Package {
    pub pulp_href: String,
    pub name: String,
    #[serde(default)]
    pub epoch: String,
    pub version: String,
    pub release: String,
    pub arch: String,
    #[serde(default)]
    pub location_href: String,
    #[serde(default)]
    pub sha256: Option<String>,
}

impl Package {
    pub fn nevra(&self) -> String {
        let e = if self.epoch.is_empty() || self.epoch == "0" { String::new() } else { format!("{}:", self.epoch) };
        format!("{}-{e}{}-{}.{}", self.name, self.version, self.release, self.arch)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct DebPackage {
    pub pulp_href: String,
    pub package: String,
    pub version: String,
    pub architecture: String,
    #[serde(default)]
    pub relative_path: String,
    #[serde(default)]
    pub sha256: Option<String>,
}

impl DebPackage {
    pub fn key(&self) -> String {
        format!("{}_{}_{}", self.package, self.version, self.architecture)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileContent {
    pub pulp_href: String,
    pub relative_path: String,
    #[serde(default)]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ComponentVersion {
    pub component: String,
    pub version: String,
    #[serde(default)]
    pub package: Option<String>,
    #[serde(default)]
    pub domain_compatible: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Status {
    pub versions: Vec<ComponentVersion>,
    #[serde(default)]
    pub online_workers: Vec<Value>,
    #[serde(default)]
    pub online_content_apps: Vec<Value>,
    #[serde(default)]
    pub database_connection: Option<Value>,
    #[serde(default)]
    pub redis_connection: Option<Value>,
    #[serde(default)]
    pub storage: Option<Value>,
    #[serde(default)]
    pub content_settings: Option<Value>,
    #[serde(default)]
    pub domain_enabled: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_type_from_href() {
        assert_eq!(RepoType::from_href("/pulp/api/v3/repositories/rpm/rpm/0190-ab/versions/2/"), Some(RepoType::Rpm));
        assert_eq!(RepoType::from_href("/pulp/api/v3/content/file/files/1/"), Some(RepoType::File));
        assert_eq!(RepoType::from_href("/pulp/api/v3/repositories/deb/apt/1/versions/2/"), Some(RepoType::Deb));
        assert_eq!(RepoType::from_href("/pulp/api/v3/content/deb/packages/1/"), Some(RepoType::Deb));
        assert_eq!(RepoType::from_href("/pulp/api/v3/repositories/container/container/1/"), None);
    }

    #[test]
    fn type_helpers_cover_all() {
        assert_eq!(RepoType::Rpm.next(), RepoType::File);
        assert_eq!(RepoType::File.next(), RepoType::Deb);
        assert_eq!(RepoType::Deb.next(), RepoType::Rpm);
        assert_eq!(RepoType::ALL, [RepoType::Rpm, RepoType::File, RepoType::Deb]);
        assert_eq!(RepoType::Rpm.feature(), Feature::Rpm);
        assert_eq!(RepoType::File.remote_schema(), "file.FileRemote");
        assert_eq!((RepoType::Deb.feature(), RepoType::Deb.component(), RepoType::Deb.label()), (Feature::Deb, "deb", "Deb"));
        assert_eq!((RepoType::Deb.repository_schema(), RepoType::Deb.remote_schema()), ("deb.AptRepository", "deb.AptRemote"));
        assert_eq!(RepoType::Deb.repositories(), "repositories/deb/apt/");
        assert_eq!(RepoType::Deb.content(), "content/deb/packages/");
    }

    #[test]
    fn content_type_keys_name_the_type_and_the_unit() {
        use serde_json::json;
        use std::collections::BTreeSet;
        // Each sample has exactly the `fields` that the compare asks for, as Pulp returns them.
        let key = |t: RepoType, label: &str, v: Value| {
            let ct = t.content_types().iter().find(|c| c.label == label).unwrap();
            let asked: BTreeSet<&str> = ct.fields.split(',').collect();
            assert_eq!(v.as_object().unwrap().keys().map(String::as_str).collect::<BTreeSet<_>>(), asked, "{label}");
            (ct.key)(&v)
        };
        // Every content type of the plugin's OpenAPI (pulp_rpm 3.39, pulp_deb 3.11), so a compare misses no unit.
        let paths = |t: RepoType, plugin: &str| {
            t.content_types().iter().map(|c| c.path.strip_prefix(&format!("content/{plugin}/")).unwrap()).collect::<Vec<_>>().join(" ")
        };
        assert_eq!(paths(RepoType::File, "file"), "files/");
        let rpm = "packages/ advisories/ modulemds/ packagegroups/ distribution_trees/ repo_metadata_files/ modulemd_defaults/ \
                   modulemd_obsoletes/ packagecategories/ packageenvironments/ packagelangpacks/";
        assert_eq!(paths(RepoType::Rpm, "rpm"), rpm);
        let deb = "packages/ releases/ release_components/ source_packages/ generic_contents/ installer_packages/ \
                   installer_file_indices/ package_indices/ source_indices/ release_architectures/ release_files/ \
                   package_release_components/ source_release_components/";
        assert_eq!(paths(RepoType::Deb, "deb"), deb);
        let defaults = json!({ "module": "nodejs", "stream": "18" });
        assert_eq!(key(RepoType::Rpm, "modulemd defaults", defaults), "modulemd defaults nodejs (stream 18)");
        let obsolete = json!({ "module_name": "nodejs", "module_stream": "16", "modified": "202401010000" });
        assert_eq!(key(RepoType::Rpm, "modulemd obsoletes", obsolete), "modulemd obsolete nodejs:16 (modified 202401010000)");
        assert_eq!(key(RepoType::Rpm, "package categories", json!({ "id": "base-system" })), "package category base-system");
        assert_eq!(key(RepoType::Rpm, "package environments", json!({ "id": "server" })), "package environment server");
        let langpacks = json!({ "digest": "0123456789abcdef" });
        assert_eq!(key(RepoType::Rpm, "package langpacks", langpacks), "package langpacks 0123456789ab");
        // The API shows no digest of these units (or one that is `null` until download): the unit id keeps them apart.
        let at = |kind: &str, path: &str| json!({ "relative_path": path, "pulp_href": format!("/pulp/api/v3/content/deb/{kind}/01a1/") });
        assert_eq!(key(RepoType::Deb, "generic contents", at("generic_contents", "x.txt")), "generic content x.txt  (01a1)");
        let udeb = json!({ "pulp_href": "/d/", "package": "di-utils", "version": "1.1", "architecture": "amd64" });
        assert_eq!(key(RepoType::Deb, "installer packages", udeb), "installer package di-utils_1.1_amd64");
        let sums = at("installer_file_indices", "dists/b/main/installer-amd64/current/images/SHA256SUMS");
        assert_eq!(
            key(RepoType::Deb, "installer file indices", sums),
            "installer file index dists/b/main/installer-amd64/current/images/SHA256SUMS  (01a1)"
        );
        let index = at("package_indices", "dists/b/main/binary-amd64/Packages");
        assert_eq!(key(RepoType::Deb, "package indices", index), "package index dists/b/main/binary-amd64/Packages  (01a1)");
        let sources = at("source_indices", "dists/b/main/source/Sources");
        assert_eq!(key(RepoType::Deb, "source indices", sources), "source index dists/b/main/source/Sources  (01a1)");
        let arch = json!({ "distribution": "bookworm", "architecture": "amd64" });
        assert_eq!(key(RepoType::Deb, "release architectures", arch), "release architecture bookworm/amd64");
        assert_eq!(key(RepoType::Deb, "release files", at("release_files", "dists/b/Release")), "release file dists/b/Release  (01a1)");
        let prc = json!({ "pulp_href": "/pulp/api/v3/content/deb/package_release_components/01a1/" });
        assert_eq!(key(RepoType::Deb, "package release components", prc), "package release component 01a1");
        let sprc = json!({ "pulp_href": "/pulp/api/v3/content/deb/source_release_components/01a1/" });
        assert_eq!(key(RepoType::Deb, "source release components", sprc), "source release component 01a1");
        let pkg = json!({ "pulp_href": "/p/", "name": "bash", "epoch": "0", "version": "5.2", "release": "1.el9", "arch": "x86_64" });
        assert_eq!(key(RepoType::Rpm, "packages", pkg), "package bash-5.2-1.el9.x86_64");
        assert_eq!(key(RepoType::Rpm, "advisories", json!({ "id": "RHSA-2024:0001" })), "advisory RHSA-2024:0001");
        let md = json!({ "name": "nodejs", "stream": "18", "version": "9040020240108", "context": "rhel9", "arch": "x86_64" });
        assert_eq!(key(RepoType::Rpm, "modulemds", md), "modulemd nodejs:18:9040020240108:rhel9:x86_64");
        assert_eq!(key(RepoType::Rpm, "package groups", json!({ "id": "core" })), "package group core");
        let tree = json!({ "release_name": "Rocky Linux", "release_version": "9.4", "arch": "x86_64", "build_timestamp": 1714730227.0 });
        assert_eq!(key(RepoType::Rpm, "distribution trees", tree), "distribution tree Rocky Linux 9.4 x86_64 (build 1714730227.0)");
        let md_file = json!({ "data_type": "productid", "checksum": "0123456789abcdef" });
        assert_eq!(key(RepoType::Rpm, "repo metadata files", md_file), "repo metadata file productid  (0123456789ab)");
        let f = json!({ "pulp_href": "/f/", "relative_path": "docs/readme.txt", "sha256": "0123456789abcdef" });
        assert_eq!(key(RepoType::File, "files", f), "file docs/readme.txt  (0123456789ab)");
        let deb = json!({ "pulp_href": "/d/", "package": "hello", "version": "2.10-3", "architecture": "amd64" });
        assert_eq!(key(RepoType::Deb, "packages", deb), "package hello_2.10-3_amd64");
        let rel = json!({ "distribution": "bookworm", "codename": "bookworm", "suite": "stable" });
        assert_eq!(key(RepoType::Deb, "releases", rel), "release bookworm (codename bookworm, suite stable)");
        let comp = json!({ "distribution": "bookworm", "component": "main" });
        assert_eq!(key(RepoType::Deb, "release components", comp), "release component bookworm/main");
        let src = json!({ "source": "hello", "version": "2.10-3", "artifacts": { "hello_2.10-3.dsc": "/a/" } });
        assert_eq!(key(RepoType::Deb, "source packages", src), "source package hello_2.10-3");
        // A unit that does not parse still shows (as its JSON), so a compare never drops it.
        assert_eq!((RepoType::Rpm.content_types()[0].key)(&json!({ "name": "x" })), r#"package {"name":"x"}"#);
    }

    #[test]
    fn nevra_and_task_helpers() {
        let p = Package {
            pulp_href: "h".into(),
            name: "bash".into(),
            epoch: "0".into(),
            version: "5.2".into(),
            release: "1.fc40".into(),
            arch: "x86_64".into(),
            location_href: String::new(),
            sha256: None,
        };
        assert_eq!(p.nevra(), "bash-5.2-1.fc40.x86_64");
        let t: Task = serde_json::from_value(serde_json::json!({
            "pulp_href": "/t/1/", "state": "failed", "created_resources": [null, "/r/versions/1/"],
            "error": {"description": "boom", "traceback": "..."}
        }))
        .unwrap();
        assert!(t.is_final());
        assert_eq!(t.error_message().as_deref(), Some("boom"));
        assert_eq!(t.created_matching("/versions/").as_deref(), Some("/r/versions/1/"));
    }

    #[test]
    fn a_task_group_is_one_task_center_task() {
        use serde_json::json;
        let group = |counts: Value| -> TaskGroup {
            let mut g = json!({ "pulp_href": "/g/1/", "description": "Replication of up", "all_tasks_dispatched": true,
                "tasks": [{ "name": "a" }, { "name": "b" }] });
            g.as_object_mut().unwrap().extend(counts.as_object().unwrap().clone());
            serde_json::from_value(g).unwrap()
        };
        let row = |counts: Value| {
            let t = group(counts).as_task();
            (t.state.clone(), t.error_message())
        };
        assert_eq!(row(json!({ "waiting": 2 })), ("waiting".into(), None));
        assert_eq!(row(json!({ "running": 1, "waiting": 1 })), ("running".into(), None));
        assert_eq!(row(json!({ "canceling": 1, "completed": 1 })), ("running".into(), None), "not final while a task cancels");
        // Final: a failed task fails the group; else a canceled one cancels it; else a skipped one.
        let counts = |c, f, x, s| Some(format!("completed {c}  failed {f}  canceled {x}  skipped {s}"));
        assert_eq!(row(json!({ "completed": 2 })), ("completed".into(), None));
        assert_eq!(row(json!({ "failed": 1, "canceled": 1 })), ("failed".into(), counts(0, 1, 1, 0)));
        assert_eq!(row(json!({ "canceled": 1, "skipped": 1 })), ("canceled".into(), counts(0, 0, 1, 1)));
        assert_eq!(row(json!({ "completed": 1, "skipped": 1 })), ("skipped".into(), counts(1, 0, 0, 1)));
        let t = group(json!({ "completed": 1, "running": 1 })).as_task();
        assert_eq!((t.pulp_href.as_str(), t.name.as_str()), ("/g/1/", "Replication of up"));
        assert_eq!((t.progress_reports[0].done, t.progress_reports[0].total), (Some(1), Some(2)), "finished of all tasks");
    }

    #[test]
    fn an_import_group_ends_when_pulp_import_completed_and_nothing_runs() {
        use serde_json::json;
        // pulpcore 3.123 never sets `all_tasks_dispatched` on an import group; `pulp_import` dispatches every repository task.
        let group = |main: &str, repo: Option<&str>, counts: Value| -> TaskGroup {
            let mut tasks = vec![json!({ "name": "pulpcore.app.tasks.importer.pulp_import", "state": main })];
            tasks.extend(repo.map(|s| json!({ "name": "pulpcore.app.tasks.importer.import_repository_version", "state": s })));
            let mut g =
                json!({ "pulp_href": "/g/2/", "description": "Import of /x-toc.json", "all_tasks_dispatched": false, "tasks": tasks });
            g.as_object_mut().unwrap().extend(counts.as_object().unwrap().clone());
            serde_json::from_value(g).unwrap()
        };
        let state = |g: TaskGroup| (g.is_final(), g.as_task().state);
        assert_eq!(state(group("running", None, json!({ "running": 1 }))), (false, "running".into()));
        assert_eq!(state(group("completed", Some("running"), json!({ "completed": 1, "running": 1 }))), (false, "running".into()));
        assert_eq!(state(group("completed", Some("completed"), json!({ "completed": 2 }))), (true, "completed".into()));
        assert_eq!(state(group("completed", None, json!({ "completed": 1 }))), (true, "completed".into()), "no destination found");
        assert_eq!(state(group("failed", None, json!({ "failed": 1 }))), (true, "failed".into()));
    }

    /// pulp_rpm 3.39.5 never sets `all_tasks_dispatched` on a prune group; `prune_packages` dispatches every repository task.
    #[test]
    fn a_prune_group_ends_when_prune_packages_completed_and_nothing_runs() {
        use serde_json::json;
        let group = |tasks: &[(&str, &str)], counts: Value| -> TaskGroup {
            let tasks: Vec<Value> =
                tasks.iter().map(|(n, s)| json!({ "name": format!("pulp_rpm.app.tasks.prune.{n}"), "state": s })).collect();
            let mut g =
                json!({ "pulp_href": "/g/3/", "description": "Prune old Packages.", "all_tasks_dispatched": false, "tasks": tasks });
            g.as_object_mut().unwrap().extend(counts.as_object().unwrap().clone());
            serde_json::from_value(g).unwrap()
        };
        let state = |g: TaskGroup| (g.is_final(), g.as_task().state);
        let (main, repo) = ("prune_packages", "prune_repo_packages");
        assert_eq!(state(group(&[(main, "running")], json!({ "running": 1 }))), (false, "running".into()));
        let waiting = group(&[(main, "completed"), (repo, "waiting")], json!({ "completed": 1, "waiting": 1 }));
        assert_eq!(state(waiting), (false, "waiting".into()));
        assert_eq!(state(group(&[(main, "completed"), (repo, "completed")], json!({ "completed": 2 }))), (true, "completed".into()));
        assert_eq!(state(group(&[(repo, "completed")], json!({ "completed": 1 }))), (false, "waiting".into()), "a repository task alone");
    }

    /// The refresh of an alternate content source dispatches all its tasks before it answers and never sets
    /// `all_tasks_dispatched`: the group ends with its tasks (the descriptions of pulp_rpm, pulp_file, pulp_deb).
    #[test]
    fn an_acs_refresh_group_ends_with_its_tasks() {
        use serde_json::json;
        for d in [
            "Refreshing 2 alternate content source path(s).",
            "Refreshing 1 alternate content source paths.",
            "Refreshing Alternate Content Source 'apt'.",
        ] {
            let g = |counts: Value| -> TaskGroup {
                let mut g = json!({ "pulp_href": "/g/1/", "description": d, "all_tasks_dispatched": false, "tasks": [{}, {}] });
                g.as_object_mut().unwrap().extend(counts.as_object().unwrap().clone());
                serde_json::from_value(g).unwrap()
            };
            assert_eq!(g(json!({ "completed": 2 })).as_task().state, "completed", "{d}");
            assert_eq!(g(json!({ "completed": 1, "running": 1 })).as_task().state, "running", "{d}");
        }
        let other = json!({ "pulp_href": "/g/2/", "description": "Replication of up", "completed": 2, "tasks": [{}, {}] });
        let other: TaskGroup = serde_json::from_value(other).unwrap();
        assert!(!other.is_final(), "another group may still dispatch tasks");
    }

    /// A group that no other rule ends (unknown kind, flag never set) is final once it was idle for
    /// `IDLE_FINAL_SECS`: its Task Center row stops polling.
    #[test]
    fn an_idle_group_of_an_unknown_kind_ends_after_a_while() {
        use serde_json::json;
        let group = |tasks: Value| -> TaskGroup {
            let done = |s: &str| tasks.as_array().unwrap().iter().filter(|t| t["state"] == s).count();
            let g = json!({ "pulp_href": "/g/4/", "description": "Something new", "all_tasks_dispatched": false,
                "completed": done("completed"), "skipped": done("skipped"), "running": done("running"), "tasks": tasks });
            serde_json::from_value(g).unwrap()
        };
        let state = |g: TaskGroup| (g.is_final(), g.as_task().state);
        let long_ago = "2000-01-01T00:00:00.123456Z";
        let old = group(json!([{ "state": "completed", "finished_at": long_ago }, { "state": "skipped", "finished_at": long_ago }]));
        assert_eq!(state(old), (true, "skipped".into()));
        let now = crate::store::now();
        let recent = group(json!([{ "state": "completed", "finished_at": long_ago }, { "state": "completed", "finished_at": now }]));
        assert_eq!(state(recent), (false, "waiting".into()), "the newest task ended just now: more may come");
        let busy = group(json!([{ "state": "completed", "finished_at": long_ago }, { "state": "running" }]));
        assert_eq!(state(busy), (false, "running".into()));
        // The rule starts at exactly IDLE_FINAL_SECS: 2026-01-01T00:20:00Z is 1_767_226_800.
        let ended = group(json!([{ "state": "completed", "finished_at": "2026-01-01T00:10:00.900000Z" }]));
        assert!(ended.idle_long(1_767_226_800) && !ended.idle_long(1_767_226_799));
    }

    /// Pulp prints times in its `TIME_ZONE` (audit F41): the same instant in UTC and with an offset gives the same rule.
    #[test]
    fn the_idle_rule_reads_the_offset_of_finished_at() {
        use serde_json::json;
        for at in ["2026-01-01T00:10:00.900000Z", "2026-01-01T05:10:00.900000+05:00", "2025-12-31T17:10:00-07:00"] {
            let g = json!({ "pulp_href": "/g/7/", "completed": 1, "tasks": [{ "state": "completed", "finished_at": at }] });
            let g: TaskGroup = serde_json::from_value(g).unwrap();
            assert!(g.idle_long(1_767_226_800) && !g.idle_long(1_767_226_799), "{at}");
        }
        assert_eq!(epoch_secs("2026-01-01T00:20:00Z"), Some(1_767_226_800));
        assert_eq!(epoch_secs("2000-03-01T00:00:00Z"), Some(951_868_800), "after a leap day");
        assert_eq!(epoch_secs("yesterday"), None);
    }

    /// February is the month that the civil-date rule turns on: the end of a leap February reads right, in UTC and with
    /// an offset. A `finished_at` without a zone, or with one that is not `Z` or `±HH:MM`, never ends a group.
    #[test]
    fn the_idle_rule_reads_a_leap_february_and_never_a_time_without_a_zone() {
        let group = |at: &str| -> TaskGroup {
            let g = serde_json::json!({ "pulp_href": "/g/8/", "completed": 1, "tasks": [{ "state": "completed", "finished_at": at }] });
            serde_json::from_value(g).unwrap()
        };
        // 2024-02-29T00:00:00Z is 1_709_164_800.
        let ended = 1_709_164_800 + IDLE_FINAL_SECS;
        for at in ["2024-02-29T00:00:00Z", "2024-02-29T01:30:00.5+01:30", "2024-02-28T21:00:00-03:00"] {
            assert!(group(at).idle_long(ended) && !group(at).idle_long(ended - 1), "{at}");
        }
        for at in ["2024-02-29T00:00:00", "2024-02-29T00:00:00 UTC", "2024-02-29T00:00:00+0100", "2024-02-29T00:00:00Zulu"] {
            assert!(!group(at).idle_long(u64::MAX / 2), "{at}");
        }
    }

    /// No group with tasks is final before a task of it ended; a failed or canceled task ends it also without the flag.
    #[test]
    fn a_group_ends_with_a_failure_and_never_before_a_task_ended() {
        let group = |g: Value| -> TaskGroup { serde_json::from_value(g).unwrap() };
        let unknown = group(serde_json::json!({ "pulp_href": "/g/5/", "all_tasks_dispatched": true, "tasks": [{}] }));
        assert_eq!((unknown.is_final(), unknown.as_task().state.as_str()), (false, "waiting"));
        let both = group(serde_json::json!({ "pulp_href": "/g/5/", "failed": 1, "canceled": 1, "tasks": [{}, {}] }));
        assert_eq!((both.is_final(), both.as_task().state.as_str()), (true, "failed"));
    }

    /// Purge deletes tasks and `Task.task_group` is `SET_NULL`: the group stays with no task and no count (audit F36).
    /// It is final, but how it ended is unknown (a purge can delete failed tasks too): `gone`, never a success.
    #[test]
    fn a_group_whose_tasks_were_purged_is_final_and_gone() {
        let group = |g: Value| -> TaskGroup { serde_json::from_value(g).unwrap() };
        for flag in [false, true] {
            let purged = group(serde_json::json!({ "pulp_href": "/g/6/", "description": "Import of /x-toc.json",
                "all_tasks_dispatched": flag, "tasks": [] }));
            assert_eq!((purged.is_final(), purged.outcome()), (true, GONE), "{flag}");
            let t = purged.as_task();
            assert_eq!((t.state.as_str(), t.error_message().as_deref()), (GONE, Some("its tasks were purged")), "{flag}");
        }
        let waiting = group(serde_json::json!({ "pulp_href": "/g/6/", "waiting": 1, "tasks": [] }));
        assert!(!waiting.is_final(), "a count says a task waits");
    }
}
