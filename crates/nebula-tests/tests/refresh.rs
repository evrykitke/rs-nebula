//! Proof of concept: refresh token rotation with reuse detection, logout,
//! and tenant onboarding — the registering admin manages users and can
//! hand admin rights over.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use nebula::config::{Config, DatabaseConfig};
use nebula::{AuthModule, Kernel, db};
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
        req = req.header("X-Tenant", "initech");
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

#[tokio::test]
async fn refresh_tokens_and_onboarding_end_to_end() {
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
        .add_module(AuthModule)
        .build()
        .unwrap()
        .init()
        .await
        .expect("boot must succeed");
    let router = app.router();

    // Onboarding: one call registers the company; the registrant is the
    // admin automatically.
    let (status, body) = send(
        &router,
        "POST",
        "/auth/register",
        None,
        Some(serde_json::json!({
            "tenant_name": "initech",
            "email": "founder@initech.test",
            "password": "founderpass1",
            "first_name": "Fo",
            "last_name": "Under",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "register: {body}");
    assert_eq!(body["user"]["is_tenant_admin"], true);

    // Login yields an access/refresh pair.
    let (status, body) = send(
        &router,
        "POST",
        "/auth/login",
        None,
        Some(serde_json::json!({ "login": "founder@initech.test", "password": "founderpass1" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "success");
    let access = body["access_token"].as_str().unwrap().to_string();
    let refresh1 = body["refresh_token"].as_str().unwrap().to_string();

    // Rotation: the pair is replaced...
    let (status, body) = send(
        &router,
        "POST",
        "/auth/token/refresh",
        None,
        Some(serde_json::json!({ "refresh_token": refresh1 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "refresh: {body}");
    let refresh2 = body["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(refresh1, refresh2);
    let rotated_access = body["access_token"].as_str().unwrap().to_string();

    // ...and reusing the consumed token is theft: it fails AND revokes
    // every session, so the legitimate successor dies too.
    let (status, _) = send(
        &router,
        "POST",
        "/auth/token/refresh",
        None,
        Some(serde_json::json!({ "refresh_token": refresh1 })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = send(
        &router,
        "POST",
        "/auth/token/refresh",
        None,
        Some(serde_json::json!({ "refresh_token": refresh2 })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "reuse must revoke the whole family"
    );

    // Fresh login; logout revokes the refresh token.
    let (_, body) = send(
        &router,
        "POST",
        "/auth/login",
        None,
        Some(serde_json::json!({ "login": "founder@initech.test", "password": "founderpass1" })),
    )
    .await;
    let refresh3 = body["refresh_token"].as_str().unwrap().to_string();
    let (status, _) = send(
        &router,
        "POST",
        "/auth/logout",
        None,
        Some(serde_json::json!({ "refresh_token": refresh3 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        &router,
        "POST",
        "/auth/token/refresh",
        None,
        Some(serde_json::json!({ "refresh_token": refresh3 })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Access tokens still work independently of refresh revocations
    // (stamp unchanged).
    let (status, _) = send(&router, "GET", "/auth/me", Some(&rotated_access), None).await;
    assert_eq!(status, StatusCode::OK);

    // -- Onboarding the team: admin creates a member and can promote her.
    let (status, body) = send(
        &router,
        "POST",
        "/auth/users",
        Some(&access),
        Some(serde_json::json!({
            "user_name": "peter",
            "email": "peter@initech.test",
            "password": "tpsreports1",
            "first_name": "Peter",
            "last_name": "Gibbons",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create user: {body}");
    assert_eq!(body["is_tenant_admin"], false);
    let peter_id = body["id"].as_str().unwrap().to_string();

    // Non-admins cannot create users or list them.
    let (_, body) = send(
        &router,
        "POST",
        "/auth/login",
        None,
        Some(serde_json::json!({ "login": "peter", "password": "tpsreports1" })),
    )
    .await;
    let peter_access = body["access_token"].as_str().unwrap().to_string();
    let (status, _) = send(&router, "GET", "/auth/users", Some(&peter_access), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Admin rights can be handed over later.
    let (status, body) = send(
        &router,
        "PUT",
        &format!("/auth/users/{peter_id}/admin"),
        Some(&access),
        Some(serde_json::json!({ "is_admin": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "promote: {body}");
    assert_eq!(body["is_tenant_admin"], true);

    // Peter's old token predates the change but the stamp did not rotate,
    // so he re-logs in to pick up the new rights; the list works now.
    let (_, body) = send(
        &router,
        "POST",
        "/auth/login",
        None,
        Some(serde_json::json!({ "login": "peter", "password": "tpsreports1" })),
    )
    .await;
    let peter_access = body["access_token"].as_str().unwrap().to_string();
    let (status, body) = send(&router, "GET", "/auth/users", Some(&peter_access), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 2);

    // Nobody can demote themselves — a tenant cannot lose its last admin.
    let (status, _) = send(
        &router,
        "PUT",
        &format!("/auth/users/{peter_id}/admin"),
        Some(&peter_access),
        Some(serde_json::json!({ "is_admin": false })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Password change revokes refresh tokens (and old access tokens via
    // the stamp).
    let (_, body) = send(
        &router,
        "POST",
        "/auth/login",
        None,
        Some(serde_json::json!({ "login": "peter", "password": "tpsreports1" })),
    )
    .await;
    let peter_access = body["access_token"].as_str().unwrap().to_string();
    let peter_refresh = body["refresh_token"].as_str().unwrap().to_string();
    let (status, _) = send(
        &router,
        "POST",
        "/auth/password",
        Some(&peter_access),
        Some(serde_json::json!({
            "current_password": "tpsreports1",
            "new_password": "newreports22"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        &router,
        "POST",
        "/auth/token/refresh",
        None,
        Some(serde_json::json!({ "refresh_token": peter_refresh })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "password change must revoke refresh tokens"
    );
}
