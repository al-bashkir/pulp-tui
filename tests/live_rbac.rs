//! RBAC against a live Pulp: operations succeed only with granted roles, users cannot
//! escalate their own privileges, and authentication failures are reported clearly.

mod common;

use common::*;
use pulp_tui::config::{Profile, Settings};
use pulp_tui::pulp::ops::{self, no_progress};
use pulp_tui::pulp::{PulpClient, RepoType};
use serde_json::{Value, json};

fn user_client(user: &str, password: &str) -> PulpClient {
    let dir = tempfile::tempdir().unwrap().keep();
    std::fs::write(dir.join("pw"), password).unwrap();
    let p = Profile {
        url: std::env::var("PULP_TUI_TEST_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into()),
        username: Some(user.into()),
        secret: Some(format!("file:{}", dir.join("pw").display())),
        ..Default::default()
    };
    PulpClient::new(user, &p, &Settings { poll_min_ms: 300, poll_max_ms: 2000, ..Default::default() }).unwrap()
}

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
