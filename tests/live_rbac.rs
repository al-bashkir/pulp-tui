//! RBAC against a live Pulp: operations succeed only with granted roles, users cannot
//! escalate their own privileges, and authentication failures are reported clearly.

mod common;

use common::*;
use pulp_tui::cv::{CvService, LeftOut};
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{PulpClient, RepoType};
use pulp_tui::store::{BindingRepo, Store};
use serde_json::{Value, json};
use std::sync::Arc;

#[tokio::test]
async fn rbac_is_enforced_and_not_bypassed() {
    if !live_enabled() {
        return;
    }
    let admin = client();
    let name = unique("rbac");
    let password = "Rbac-test-Pw-9a!";
    let user = create(&admin, "users/", json!({ "username": name, "password": password, "is_active": true }), "/users/").await;
    let u = user_client(&name, password);

    // Wrong password: clear 401 (not login/: pulpcore 3.63 has none and answers 404).
    let e = user_client(&name, "wrong").get::<Value>(&admin.api(RepoType::File.repositories()), &[]).await.unwrap_err();
    assert_eq!(e.status(), Some(401), "{e}");

    // No roles: reading lists works but shows nothing; creating is forbidden.
    let repo_name = format!("{name}-repo");
    let e = u.post(&u.api(RepoType::File.repositories()), &json!({ "name": repo_name })).await.unwrap_err();
    assert_eq!(e.status(), Some(403), "{e}");
    let visible: Value = u.get(&u.api(RepoType::File.repositories()), &[]).await.unwrap();
    assert_eq!(visible["count"], 0);

    // Self-escalation is refused by Pulp, not by this client.
    let e = u.post(&format!("{user}roles/"), &json!({ "role": "file.filerepository_creator", "content_object": null })).await.unwrap_err();
    assert_eq!(e.status(), Some(403), "{e}");
    let e = u.patch(&user, &json!({ "is_staff": true })).await.unwrap_err();
    assert_eq!(e.status(), Some(403), "{e}");

    // Admin grants a model-level creator role: now allowed, and the creator owns the object.
    let role = create(
        &admin,
        &format!("{user}roles/").replacen(admin.api_prefix(), "", 1),
        json!({ "role": "file.filerepository_creator", "content_object": null }),
        "/roles/",
    )
    .await;
    let repo = ops::create(&u, &u.api(RepoType::File.repositories()), &json!({ "name": repo_name }), "/repositories/", &no_progress)
        .await
        .unwrap();
    let visible: Value = u.get(&u.api(RepoType::File.repositories()), &[]).await.unwrap();
    assert_eq!(visible["count"], 1);

    // Still no user administration.
    let e = u.post(&u.api("users/"), &json!({ "username": format!("{name}-x"), "password": password })).await.unwrap_err();
    assert_eq!(e.status(), Some(403), "{e}");

    // Group-granted object role: a second user sees exactly that repository.
    let name2 = format!("{name}-g");
    let user2 = create(&admin, "users/", json!({ "username": name2, "password": password }), "/users/").await;
    let group = create(&admin, "groups/", json!({ "name": format!("{name}-grp") }), "/groups/").await;
    admin.post(&format!("{group}users/"), &json!({ "username": name2 })).await.unwrap();
    admin.post(&format!("{group}roles/"), &json!({ "role": "file.filerepository_viewer", "content_object": repo })).await.unwrap();
    let u2 = user_client(&name2, password);
    let seen: Value = u2.get(&u2.api(RepoType::File.repositories()), &[]).await.unwrap();
    assert_eq!(seen["count"], 1);
    let e = u2.delete(&repo).await.unwrap_err();
    assert_eq!(e.status(), Some(403), "viewer cannot delete: {e}");

    // Removing the role revokes access again.
    cleanup(&admin, &[&role]).await;
    let e = u.post(&u.api(RepoType::File.repositories()), &json!({ "name": format!("{name}-2") })).await.unwrap_err();
    assert_eq!(e.status(), Some(403), "{e}");

    cleanup(&admin, &[&repo, &group, &user, &user2]).await;
}

/// F59: pulpcore scopes reads by view permission, so a distribution that a user may not view answers 404 to that user,
/// as a deleted one does. `ops::sees_everything` tells a Pulp administrator from that user. For a left-out Content View
/// row that records a publication, the user's 404 keeps the row (`LeftOut::Refuse`); only the administrator's 404
/// (the distribution is gone) forgets it.
#[tokio::test]
async fn a_404_of_a_hidden_distribution_keeps_a_content_view_row_for_a_user() {
    if !live_enabled() {
        return;
    }
    let admin = client();
    let name = unique("rbac404");
    let password = "Rbac-test-Pw-9a!";
    let user = create(&admin, "users/", json!({ "username": name, "password": password }), "/users/").await;
    let u = user_client(&name, password);
    let dist = create(&admin, RepoType::File.distributions(), json!({ "name": name, "base_path": name }), "/distributions/").await;
    assert_eq!(u.get::<Value>(&dist, &[]).await.unwrap_err().status(), Some(404), "hidden from the user");
    assert!(ops::sees_everything(&admin).await.unwrap());
    assert!(!ops::sees_everything(&u).await.unwrap());

    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let scope = admin.profile().to_string();
    let lib = store.ensure_library(&scope).await.unwrap();
    let env = store.environments(&scope).await.unwrap().into_iter().find(|e| e.id == lib).unwrap();
    let row = BindingRepo {
        env_id: lib,
        cv_id: 1,
        repo_href: "/pulp/api/v3/repositories/file/file/0/".into(),
        repo_name: "docs".into(),
        repo_type: RepoType::File,
        distribution: dist.clone(),
        base_path: name.clone(),
        publication: Some("/pulp/api/v3/publications/file/file/0/".into()),
    };
    let svc = |c: &PulpClient| CvService { client: c.clone(), store: store.clone(), scope: scope.clone(), actor: "t".into(), lock: None };
    let refused = |what: Option<LeftOut>| matches!(what, Some(LeftOut::Refuse(why)) if why.contains("this Pulp user may not view it"));
    assert!(refused(svc(&u).left_out("web", &env, &[], &row).await.unwrap()), "hidden: the row stays");
    cleanup(&admin, &[&dist]).await;
    assert!(refused(svc(&u).left_out("web", &env, &[], &row).await.unwrap()), "gone, but the user cannot tell");
    let gone = LeftOut::Forget("its distribution is gone (deleted outside pulp-tui): pulp-tui forgets it".into());
    assert_eq!(svc(&admin).left_out("web", &env, &[], &row).await.unwrap(), Some(gone));

    cleanup(&admin, &[&user]).await;
}
