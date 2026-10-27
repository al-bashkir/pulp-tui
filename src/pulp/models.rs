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
    /// Whether local files can be uploaded into repositories of this type.
    pub fn uploads(self) -> bool {
        match self {
            RepoType::Rpm | RepoType::File => true,
            RepoType::Deb => false,
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
];

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
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

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
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
    /// `all_tasks_dispatched` is not reliable (pulpcore 3.123 leaves it false after a
    /// replication), so a failure or the replication finalizer also ends the group.
    pub fn is_final(&self) -> bool {
        let finalized =
            self.tasks.iter().any(|t| t["name"].as_str().is_some_and(|n| n.ends_with("finalize_replication")) && t["state"] == "completed");
        self.active() == 0 && self.done() > 0 && (self.all_tasks_dispatched || self.failed + self.canceled > 0 || finalized)
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
        assert_eq!([RepoType::Rpm, RepoType::File, RepoType::Deb].map(RepoType::uploads), [true, true, false]);
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
        let labels = |t: RepoType| t.content_types().iter().map(|c| c.label).collect::<Vec<_>>();
        assert_eq!(
            labels(RepoType::Rpm),
            ["packages", "advisories", "modulemds", "package groups", "distribution trees", "repo metadata files"]
        );
        assert_eq!((labels(RepoType::File), labels(RepoType::Deb).len()), (vec!["files"], 4));
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
}
