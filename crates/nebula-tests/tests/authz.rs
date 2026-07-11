//! Proof of concept: ASP.NET-Zero-style authorization against a live
//! database — registration seeds the static Admin role, roles carry
//! permission grants, per-user overrides win over role grants (deny
//! beats grant), and admin endpoints are guarded by permissions.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use nebula::auth::permission::{self, PermissionDef, Registry, names};
use nebula::config::{Config, DatabaseConfig};
use nebula::{AdministrationModule, Kernel, db};
use tower::ServiceExt;

async fn send(
    router: &Router,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if path != "/auth/register" {
        req = req.header("X-Tenant", "globex");
    }
    if let Some(token) = bearer {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let body = match body {
        Some(json) => Body::from(json.to_string()),
        None => Body::empty(),
    };
    let response = router
        .clone()
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn login(router: &Router, login: &str, password: &str) -> String {
    let (status, body) = send(
        router,
        "POST",
        "/auth/login",
        None,
        Some(serde_json::json!({ "login": login, "password": password })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "login failed: {body}");
    body["access_token"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn authorization_end_to_end() {
    let Ok(url) = std::env::var("NEBULA_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set NEBULA_TEST_DATABASE_URL to run database tests");
        return;
    };

    let admin_db = db::connect(&DatabaseConfig {
        url: url.as_str().into(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("must connect");
    sea_orm::ConnectionTrait::execute_unprepared(
        &admin_db,
        "DROP TABLE IF EXISTS user_directory; DROP TABLE IF EXISTS currencies; DROP TABLE IF EXISTS audit_logs; DROP TABLE IF EXISTS permission_grants; \
         DROP TABLE IF EXISTS user_roles; DROP TABLE IF EXISTS roles; \
         DROP TABLE IF EXISTS refresh_tokens; DROP TABLE IF EXISTS users; \
         DROP TABLE IF EXISTS tenants; DROP TABLE IF EXISTS nebula_migrations;",
    )
    .await
    .expect("cleanup must work");

    let mut config = Config::default();
    config.database = DatabaseConfig {
        url: url.as_str().into(),
        auto_migrate: true,
        ..DatabaseConfig::default()
    };
    config.multitenancy.enabled = true;
    config.auth.jwt_secret = "test-secret-not-for-production".into();

    let app = Kernel::builder()
        .with_config(config)
        .add_module(AdministrationModule)
        .build()
        .unwrap()
        .init()
        .await
        .expect("boot must succeed");
    let router = app.router();

    // -- Registration seeds the static Admin role for the new tenant.
    let (status, body) = send(
        &router,
        "POST",
        "/auth/register",
        None,
        Some(serde_json::json!({
            "tenant_name": "globex",
            "email": "boss@globex.test",
            "password": "hunter2hunter2",
            "first_name": "Hank",
            "last_name": "Scorpio",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "register failed: {body}");
    let boss_id = body["user"]["id"].as_str().unwrap().to_string();
    let boss = login(&router, "boss@globex.test", "hunter2hunter2").await;

    // The admin effectively holds every defined permission.
    let (status, body) = send(&router, "GET", "/auth/me/permissions", Some(&boss), None).await;
    assert_eq!(status, StatusCode::OK);
    let held: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(held.contains(&names::USERS_VIEW), "got: {held:?}");
    assert!(held.contains(&names::ROLES_CREATE));

    // The permission tree is visible to role managers.
    let (status, body) = send(&router, "GET", "/auth/permissions", Some(&boss), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["name"], names::ADMINISTRATION);

    // The Admin role exists, is static, and cannot be deleted.
    let (status, body) = send(&router, "GET", "/auth/roles", Some(&boss), None).await;
    assert_eq!(status, StatusCode::OK, "list roles failed: {body}");
    let admin_role = body
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "Admin")
        .expect("Admin role must be seeded");
    assert_eq!(admin_role["is_static"], true);
    let admin_role_id = admin_role["id"].as_str().unwrap().to_string();
    let (status, _) = send(
        &router,
        "DELETE",
        &format!("/auth/roles/{admin_role_id}"),
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "static role must survive");

    // -- A plain employee holds nothing and is locked out of admin APIs.
    let (status, body) = send(
        &router,
        "POST",
        "/auth/users",
        Some(&boss),
        Some(serde_json::json!({
            "user_name": "worker",
            "email": "worker@globex.test",
            "password": "workerpass1",
            "first_name": "Way",
            "last_name": "Lon",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create user failed: {body}");
    let worker_id = body["id"].as_str().unwrap().to_string();
    let worker = login(&router, "worker", "workerpass1").await;

    let (status, body) = send(&router, "GET", "/auth/me/permissions", Some(&worker), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);
    let (status, _) = send(&router, "GET", "/auth/users", Some(&worker), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(&router, "GET", "/auth/roles", Some(&worker), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // -- Roles grant permissions. Unknown permission names are refused.
    let (status, _) = send(
        &router,
        "POST",
        "/auth/roles",
        Some(&boss),
        Some(serde_json::json!({
            "name": "support",
            "display_name": "Support",
            "permissions": ["Pages.Nope"],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "typo must not pass");

    let (status, body) = send(
        &router,
        "POST",
        "/auth/roles",
        Some(&boss),
        Some(serde_json::json!({
            "name": "support",
            "display_name": "Support",
            "permissions": [names::USERS_VIEW],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create role failed: {body}");
    let support_id = body["id"].as_str().unwrap().to_string();

    let (status, body) = send(
        &router,
        "PUT",
        &format!("/auth/users/{worker_id}/roles"),
        Some(&boss),
        Some(serde_json::json!({ "role_ids": [support_id] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "assign role failed: {body}");

    let (status, _) = send(&router, "GET", "/auth/users", Some(&worker), None).await;
    assert_eq!(status, StatusCode::OK, "role grant must open the door");
    let (status, _) = send(&router, "GET", "/auth/roles", Some(&worker), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "but only that door");

    // -- A per-user deny beats the role grant.
    let (status, body) = send(
        &router,
        "PUT",
        &format!("/auth/users/{worker_id}/permissions"),
        Some(&boss),
        Some(serde_json::json!({ "denied": [names::USERS_VIEW] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "deny failed: {body}");
    assert!(
        !body["effective"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == names::USERS_VIEW)
    );
    let (status, _) = send(&router, "GET", "/auth/users", Some(&worker), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "deny must beat the role");

    // -- A per-user grant works without any role.
    let (status, body) = send(
        &router,
        "PUT",
        &format!("/auth/users/{worker_id}/permissions"),
        Some(&boss),
        Some(serde_json::json!({ "granted": [names::USERS_CREATE] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "grant failed: {body}");
    let (status, body) = send(
        &router,
        "POST",
        "/auth/users",
        Some(&worker),
        Some(serde_json::json!({
            "user_name": "temp",
            "email": "temp@globex.test",
            "password": "temppass1234",
            "first_name": "Tem",
            "last_name": "P",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "user grant must suffice: {body}");

    // -- Nobody edits their own access; admin role is transferable.
    let (status, _) = send(
        &router,
        "PUT",
        &format!("/auth/users/{boss_id}/roles"),
        Some(&boss),
        Some(serde_json::json!({ "role_ids": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "own roles are off limits");
    let (status, _) = send(
        &router,
        "PUT",
        &format!("/auth/users/{boss_id}/admin"),
        Some(&boss),
        Some(serde_json::json!({ "is_admin": false })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "no self-demotion");

    let (status, body) = send(
        &router,
        "PUT",
        &format!("/auth/users/{worker_id}/admin"),
        Some(&boss),
        Some(serde_json::json!({ "is_admin": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "promotion failed: {body}");
    let (status, _) = send(&router, "GET", "/auth/roles", Some(&worker), None).await;
    assert_eq!(status, StatusCode::OK, "an admin can do everything");

    // -- Custom roles can be deleted; their assignments die with them.
    let (status, _) = send(
        &router,
        "DELETE",
        &format!("/auth/roles/{support_id}"),
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = send(
        &router,
        "GET",
        &format!("/auth/users/{worker_id}/permissions"),
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body["roles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == "support"),
        "deleted role must not linger: {body}"
    );
}

#[test]
fn registry_rejects_duplicates_and_malformed_names() {
    let dup = Registry::build(vec![
        PermissionDef::new("Pages.A", "A"),
        PermissionDef::new("Pages.A", "A again"),
    ]);
    assert!(dup.is_err(), "duplicate names must be rejected");

    let bad = Registry::build(vec![PermissionDef::new("Pages..Broken", "bad")]);
    assert!(bad.is_err(), "empty segments must be rejected");

    let ok = Registry::build(vec![permission::administration_tree()]).unwrap();
    assert!(ok.contains(names::USERS_EDIT));
    assert!(!ok.contains("Pages.Unknown"));
}
