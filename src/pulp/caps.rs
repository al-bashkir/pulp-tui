//! Runtime capability discovery: component versions from `status/`, the current user
//! from `login/` (or `users/?username=` on older servers), and available endpoints/fields/enums from the live OpenAPI schema.

use super::client::{PulpClient, PulpError, Result};
use super::models::Status;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default)]
pub struct CurrentUser {
    pub href: String,
    pub username: String,
    pub is_staff: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    pub status: Option<Status>,
    pub versions: BTreeMap<String, String>,
    pub user: Option<CurrentUser>,
    /// OpenAPI paths relative to `api/v3/` (`upstream-pulps/`, `{upstream_pulp_href}replicate/`).
    pub paths: BTreeSet<String>,
    /// Schema name -> property names (request and response schemas).
    pub schemas: BTreeMap<String, BTreeSet<String>>,
    pub enums: BTreeMap<String, Vec<String>>,
    pub openapi_error: Option<String>,
}

/// Capabilities a screen or workflow may require.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    Rpm,
    File,
    RpmCopy,
    ChunkedUpload,
    NativeReplication,
    ReplicationPolicy,
    Users,
    Roles,
}

impl Feature {
    fn describe(self) -> &'static str {
        match self {
            Feature::Rpm => "pulp_rpm plugin",
            Feature::File => "pulp_file plugin",
            Feature::RpmCopy => "RPM copy API (rpm/copy/)",
            Feature::ChunkedUpload => "chunked uploads API (uploads/)",
            Feature::NativeReplication => "Upstream Pulp replication (upstream-pulps/, pulpcore >= 3.23, tech preview)",
            Feature::ReplicationPolicy => "Upstream Pulp `policy` field",
            Feature::Users => "users API",
            Feature::Roles => "roles API",
        }
    }
}

pub fn parse_version(v: &str) -> (u64, u64, u64) {
    let mut it = v.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).map(|s| s.parse().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}

impl Capabilities {
    pub async fn detect(c: &PulpClient) -> Result<Self> {
        let status: Status = c.get(&c.api("status/"), &[]).await?;
        let mut caps = Capabilities {
            versions: status.versions.iter().map(|v| (v.component.clone(), v.version.clone())).collect(),
            status: Some(status),
            ..Default::default()
        };
        let user_of = |v: &Value| CurrentUser {
            href: v["pulp_href"].as_str().unwrap_or_default().into(),
            username: v["username"].as_str().unwrap_or_default().into(),
            is_staff: v["is_staff"].as_bool(),
        };
        // Anonymous status works without credentials; login/ proves them.
        match c.get::<Value>(&c.api("login/"), &[]).await {
            Ok(v) => caps.user = Some(user_of(&v)),
            // Older pulpcore has no login endpoint: look up the Basic-auth user by name.
            Err(e) if e.status() == Some(404) => {
                if let Some(name) = c.username() {
                    match c.list::<Value>(&c.api("users/"), &[("username", name.into())]).await {
                        Ok(page) => caps.user = page.results.iter().find(|v| v["username"] == name).map(user_of),
                        Err(e) if matches!(e.status(), Some(403 | 404)) => {} // no permission to read users, or no users API
                        Err(e) => return Err(e),
                    }
                }
            }
            Err(e) => return Err(e),
        }
        if let Some(u) = caps.user.as_mut().filter(|u| u.is_staff.is_none()) {
            // Staff flag is only visible to users allowed to read their own user object.
            if let Ok(v) = c.get::<Value>(&u.href, &[("fields", "is_staff".into())]).await {
                u.is_staff = v["is_staff"].as_bool();
            }
        }
        match c.get::<Value>(&c.docs_href(), &[]).await {
            Ok(doc) => caps.load_openapi(&doc),
            Err(e) => caps.openapi_error = Some(e.to_string()),
        }
        Ok(caps)
    }

    pub fn load_openapi(&mut self, doc: &Value) {
        if let Some(paths) = doc["paths"].as_object() {
            self.paths = paths.keys().map(|p| p.rsplit_once("api/v3/").map(|(_, r)| r.to_string()).unwrap_or_else(|| p.clone())).collect();
        }
        if let Some(schemas) = doc["components"]["schemas"].as_object() {
            for (name, s) in schemas {
                if let Some(props) = s["properties"].as_object() {
                    self.schemas.insert(name.clone(), props.keys().cloned().collect());
                }
                if let Some(e) = s["enum"].as_array() {
                    self.enums.insert(name.clone(), e.iter().filter_map(|x| x.as_str().map(str::to_string)).collect());
                }
            }
        }
    }

    pub fn version(&self, component: &str) -> Option<(u64, u64, u64)> {
        self.versions.get(component).map(|v| parse_version(v))
    }
    pub fn at_least(&self, component: &str, min: (u64, u64, u64)) -> bool {
        self.version(component).is_some_and(|v| v >= min)
    }
    /// Unknown (no OpenAPI) is treated as "present"; the server then gives the final answer.
    pub fn has_endpoint(&self, rel: &str) -> bool {
        self.paths.is_empty() || self.paths.contains(rel)
    }
    pub fn has_field(&self, schema: &str, field: &str) -> bool {
        self.schemas.is_empty() || self.schemas.get(schema).is_some_and(|s| s.contains(field))
    }
    pub fn enum_values(&self, name: &str) -> Option<&[String]> {
        self.enums.get(name).map(Vec::as_slice)
    }

    pub fn supports(&self, f: Feature) -> bool {
        match f {
            Feature::Rpm => self.versions.contains_key("rpm"),
            Feature::File => self.versions.contains_key("file"),
            Feature::RpmCopy => self.versions.contains_key("rpm") && self.has_endpoint("rpm/copy/"),
            Feature::ChunkedUpload => self.has_endpoint("uploads/") && self.has_endpoint("{upload_href}"),
            Feature::NativeReplication => {
                self.at_least("core", (3, 23, 0))
                    && self.has_endpoint("upstream-pulps/")
                    && self.has_endpoint("{upstream_pulp_href}replicate/")
            }
            Feature::ReplicationPolicy => self.supports(Feature::NativeReplication) && self.has_field("UpstreamPulp", "policy"),
            Feature::Users => self.has_endpoint("users/"),
            Feature::Roles => self.has_endpoint("roles/"),
        }
    }

    pub fn require(&self, f: Feature) -> Result<()> {
        if self.supports(f) { Ok(()) } else { Err(PulpError::Unsupported(f.describe().into())) }
    }

    /// `content_origin + content_path_prefix` from status (where distributions are served).
    pub fn content_base(&self) -> Option<String> {
        let cs = self.status.as_ref()?.content_settings.as_ref()?;
        Some(format!("{}{}", cs["content_origin"].as_str()?.trim_end_matches('/'), cs["content_path_prefix"].as_str()?))
    }

    pub fn summary_lines(&self) -> Vec<String> {
        let mut out = vec![];
        for (k, v) in &self.versions {
            out.push(format!("{k:<14} {v}"));
        }
        if let Some(e) = &self.openapi_error {
            out.push(format!("OpenAPI schema unavailable ({e}); feature checks are optimistic"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn versions_and_features() {
        let mut c = Capabilities::default();
        c.versions.insert("core".into(), "3.22.4".into());
        c.versions.insert("rpm".into(), "3.19.0".into());
        assert_eq!(parse_version("3.123.0.dev"), (3, 123, 0));
        assert!(c.at_least("core", (3, 22, 0)));
        assert!(!c.supports(Feature::NativeReplication));
        assert!(c.supports(Feature::Rpm));
        assert!(!c.supports(Feature::File));
        assert!(c.require(Feature::File).is_err());
        c.versions.insert("core".into(), "3.40.0".into());
        c.load_openapi(&json!({
            "paths": {"/pulp/api/v3/upstream-pulps/": {}, "{upstream_pulp_href}replicate/": {}, "/pulp/{pulp_domain}/api/v3/rpm/copy/": {}},
            "components": {"schemas": {"UpstreamPulp": {"properties": {"name": {}}}, "SyncPolicyEnum": {"enum": ["additive", "mirror_complete"]}}}
        }));
        assert!(c.supports(Feature::NativeReplication));
        assert!(!c.supports(Feature::ReplicationPolicy));
        assert!(c.supports(Feature::RpmCopy));
        assert!(!c.supports(Feature::Users));
        assert_eq!(c.enum_values("SyncPolicyEnum").unwrap().len(), 2);
    }
}
