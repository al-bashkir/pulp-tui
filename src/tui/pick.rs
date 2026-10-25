//! Option loading for picker fields (the popup itself is drawn in `view.rs`).

use super::app::{Ctx, unavailable};
use super::form::{PickItem, Source};
use super::kinds::short;
use crate::pulp::RepoType;
use anyhow::{anyhow, bail};
use serde_json::Value;

const CAP: usize = 20_000;
const NAME_HREF: &str = "name,pulp_href";

/// One collection to list: path, `fields` query, label field, value field, note.
type Spec = (&'static str, &'static str, &'static str, &'static str, &'static str);

fn text(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

/// Options of a picker. `role` is the current value of the form's `role` field (used by `RoleObjects` only).
pub async fn load_options(ctx: &Ctx, source: &Source, role: &str) -> anyhow::Result<Vec<PickItem>> {
    let specs: Vec<Spec> = match source {
        Source::Repos(Some(t)) => vec![(t.repositories(), NAME_HREF, "name", "name", "")],
        // A server may lack one of the plugins: list only what its API offers.
        Source::Repos(None) => RepoType::ALL
            .into_iter()
            .filter(|t| ctx.caps.has_endpoint(t.repositories()))
            .map(|t| (t.repositories(), NAME_HREF, "name", "pulp_href", t.component()))
            .collect(),
        Source::Remotes(t) => vec![(t.remotes(), NAME_HREF, "name", "name", "")],
        Source::Distributions(t) => vec![(t.distributions(), NAME_HREF, "name", "name", "")],
        Source::ContentGuards => vec![("contentguards/", NAME_HREF, "name", "pulp_href", "")],
        Source::SigningServices => vec![("signing-services/", NAME_HREF, "name", "pulp_href", "")],
        Source::Roles => vec![("roles/", "name", "name", "name", "")],
        Source::Users => vec![("users/", "username", "username", "username", "")],
        Source::Domains if !ctx.caps.status.as_ref().is_some_and(|s| s.domain_enabled == Some(true)) => {
            bail!("domains are not enabled on this server")
        }
        Source::Domains => vec![("domains/", NAME_HREF, "name", "pulp_href", "")],
        Source::RoleObjects => return role_objects(ctx, role).await,
        Source::ContentViews => return content_views(ctx).await,
    };
    let mut out = vec![];
    for (path, fields, label, value, note) in specs {
        let mut query = vec![("fields", fields.to_string()), ("ordering", label.to_string())];
        if matches!(source, Source::Repos(None)) {
            query.push(("pulp_label_select", format!("!{}", crate::cv::LABEL_CV))); // a snapshot is never a source
        }
        let rows: Vec<Value> = ctx.client.list_all(&ctx.client.api(path), &query, CAP).await?;
        out.extend(rows.iter().map(|r| PickItem { label: text(r, label), value: text(r, value), note: note.into() }));
    }
    Ok(out)
}

/// The views a composite can use as components: not composites themselves.
async fn content_views(ctx: &Ctx) -> anyhow::Result<Vec<PickItem>> {
    let st = ctx.cv_store.as_ref().map_err(|e| unavailable(e.clone()))?;
    let views = st.content_views(&ctx.scope).await?;
    Ok(views
        .into_iter()
        .filter(|v| !v.composite)
        .map(|v| PickItem { label: v.name, value: v.id.to_string(), note: String::new() })
        .collect())
}

/// Objects of the one type a role grants permissions on (for the object-level role forms).
async fn role_objects(ctx: &Ctx, role: &str) -> anyhow::Result<Vec<PickItem>> {
    let unknown = || anyhow!("role {role} covers no single known object type; type the href");
    if role.is_empty() {
        bail!("pick a role first");
    }
    let roles: Vec<Value> = ctx.client.list_all(&ctx.client.api("roles/"), &[("name", role.to_string())], 100).await?;
    let permissions: Vec<String> = roles.first().and_then(|r| serde_json::from_value(r["permissions"].clone()).ok()).unwrap_or_default();
    let path = role_collection(&permissions).ok_or_else(unknown)?;
    let rows: Vec<Value> = ctx.client.list_all(&ctx.client.api(path), &[], CAP).await?;
    Ok(rows
        .iter()
        .map(|r| {
            let href = text(r, "pulp_href");
            let name = text(r, "name");
            PickItem { label: if name.is_empty() { short(&href) } else { name }, value: href, note: String::new() }
        })
        .collect())
}

/// Known models: app label, model name (in permission codenames), collection.
const MODELS: [(&str, &str, &str); 9] = [
    ("rpm", "rpmrepository", "repositories/rpm/rpm/"),
    ("rpm", "rpmremote", "remotes/rpm/rpm/"),
    ("rpm", "rpmdistribution", "distributions/rpm/rpm/"),
    ("rpm", "rpmpublication", "publications/rpm/rpm/"),
    ("file", "filerepository", "repositories/file/file/"),
    ("file", "fileremote", "remotes/file/file/"),
    ("file", "filedistribution", "distributions/file/file/"),
    ("file", "filepublication", "publications/file/file/"),
    ("core", "group", "groups/"),
];

/// Collection of the single known model all `permissions` belong to. The codename only has to contain `_<model>`,
/// so `rpm.delete_rpmrepository_version` and `rpm.manage_roles_rpmrepository` count for `rpmrepository`,
/// while `core.view_taskgroup` does not count for `group`.
pub fn role_collection(permissions: &[String]) -> Option<&'static str> {
    let mut found = permissions.iter().map(|p| {
        let (app, codename) = p.split_once('.')?;
        MODELS.iter().find(|(a, model, _)| *a == app && codename.contains(&format!("_{model}"))).map(|m| m.2)
    });
    let first = found.next()??;
    found.all(|c| c == Some(first)).then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthMethod, Config, Profile, Settings};
    use crate::pulp::{Capabilities, PulpClient};
    use crate::store::Store;
    use crate::tui::app::{App, Conn};
    use serde_json::json;
    use std::sync::Arc;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn role_collection_maps_single_model() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(role_collection(&p(&["rpm.view_rpmrepository", "rpm.change_rpmrepository"])), Some("repositories/rpm/rpm/"));
        assert_eq!(role_collection(&p(&["file.view_filedistribution"])), Some("distributions/file/file/"));
        assert_eq!(role_collection(&p(&["core.view_group"])), Some("groups/"));
        assert_eq!(role_collection(&p(&["rpm.view_rpmrepository", "rpm.view_rpmremote"])), None);
        assert_eq!(role_collection(&p(&["core.view_task"])), None);
        assert_eq!(role_collection(&[]), None);
    }

    #[test]
    fn role_collection_handles_owner_roles() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let owner = p(&[
            "rpm.view_rpmrepository",
            "rpm.change_rpmrepository",
            "rpm.delete_rpmrepository",
            "rpm.delete_rpmrepository_version",
            "rpm.manage_roles_rpmrepository",
            "rpm.modify_content_rpmrepository",
            "rpm.repair_rpmrepository",
            "rpm.sync_rpmrepository",
        ]);
        assert_eq!(role_collection(&owner), Some("repositories/rpm/rpm/"));
        assert_eq!(
            role_collection(&p(&["file.view_filedistribution", "file.manage_roles_filedistribution"])),
            Some("distributions/file/file/")
        );
        assert_eq!(role_collection(&p(&["core.view_group", "core.manage_roles_group"])), Some("groups/"));
        assert_eq!(role_collection(&p(&["rpm.view_rpmrepository", "rpm.view_rpmremote"])), None);
        assert_eq!(role_collection(&p(&["file.view_rpmrepository"])), None);
    }

    #[test]
    fn role_collection_ignores_models_that_only_end_alike() {
        assert_eq!(role_collection(&["core.view_taskgroup".to_string()]), None);
    }

    async fn ctx_for(server: &MockServer, caps: Capabilities) -> Ctx {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pw"), "s3cret").unwrap();
        let profile = Profile {
            url: server.uri(),
            auth: AuthMethod::Basic,
            username: Some("admin".into()),
            secret: Some(format!("file:{}", dir.path().join("pw").display())),
            timeout_secs: 5,
            ..Default::default()
        };
        let client = PulpClient::new("mock", &profile, &Settings::default()).unwrap();
        let st = Arc::new(Store::open_in_memory().await.unwrap());
        let mut app = App::new(Config::default(), "/dev/null".into(), st.clone(), Ok(st));
        app.conn = Some(Conn { client, caps: Arc::new(caps), scope: "mock".into() });
        app.ctx().unwrap()
    }

    #[tokio::test]
    async fn domains_need_domains_enabled_and_role_objects_need_a_role() {
        let server = MockServer::start().await;
        let mut caps = Capabilities::default();
        let ctx = ctx_for(&server, caps.clone()).await;
        let e = load_options(&ctx, &Source::Domains, "").await.unwrap_err();
        assert_eq!(e.to_string(), "domains are not enabled on this server");
        let e = load_options(&ctx, &Source::RoleObjects, "").await.unwrap_err();
        assert_eq!(e.to_string(), "pick a role first");

        Mock::given(method("GET"))
            .and(path("/pulp/api/v3/domains/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "count": 1, "next": null, "results": [{ "name": "d1", "pulp_href": "/h/d1/" }]
            })))
            .mount(&server)
            .await;
        caps.status = Some(serde_json::from_value(json!({ "versions": [], "domain_enabled": true })).unwrap());
        let ctx = ctx_for(&server, caps).await;
        let items = load_options(&ctx, &Source::Domains, "").await.unwrap();
        assert_eq!(items.iter().map(|i| (i.label.as_str(), i.value.as_str())).collect::<Vec<_>>(), [("d1", "/h/d1/")]);
    }

    #[tokio::test]
    async fn content_view_picker_lists_non_composite_views() {
        let server = MockServer::start().await;
        let ctx = ctx_for(&server, Capabilities::default()).await;
        let st = ctx.cv_store.clone().unwrap();
        let os = st.create_cv(&ctx.scope, "os", "").await.unwrap();
        st.create_composite(&ctx.scope, "rocky9", "").await.unwrap();
        st.create_cv("other", "elsewhere", "").await.unwrap();
        let items = load_options(&ctx, &Source::ContentViews, "").await.unwrap();
        assert_eq!(items.iter().map(|i| (i.label.as_str(), i.value.clone())).collect::<Vec<_>>(), [("os", os.to_string())]);
    }

    #[tokio::test]
    async fn content_view_picker_reports_an_unavailable_store() {
        let server = MockServer::start().await;
        let mut ctx = ctx_for(&server, Capabilities::default()).await;
        ctx.cv_store = Err("timeout".into());
        let e = load_options(&ctx, &Source::ContentViews, "").await.unwrap_err();
        assert_eq!(e.to_string(), "shared database unavailable: timeout");
    }

    #[tokio::test]
    async fn load_options_lists_rpm_and_file_with_names_only() {
        let server = MockServer::start().await;
        for (kind, name) in [("rpm", "r1"), ("file", "f1")] {
            Mock::given(method("GET"))
                .and(path(format!("/pulp/api/v3/repositories/{kind}/{kind}/")))
                .and(query_param("fields", "name,pulp_href"))
                .and(query_param("pulp_label_select", "!pulp_tui_cv"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "count": 1, "next": null, "results": [{ "name": name, "pulp_href": format!("/h/{kind}/") }]
                })))
                .mount(&server)
                .await;
        }
        let ctx = ctx_for(&server, Capabilities::default()).await;

        let items = load_options(&ctx, &Source::Repos(None), "").await.unwrap();
        let got: Vec<_> = items.iter().map(|i| (i.label.as_str(), i.value.as_str(), i.note.as_str())).collect();
        assert_eq!(got, [("r1", "/h/rpm/", "rpm"), ("f1", "/h/file/", "file")]);
    }
}
