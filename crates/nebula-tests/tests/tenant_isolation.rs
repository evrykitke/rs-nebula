//! A token is spendable only in the tenant it was issued for.
//!
//! Tenant resolution trusts the `X-Tenant` header, so the framework must
//! refuse a token presented against any other tenant — otherwise a signed-in
//! user of one tenant could read another's data simply by changing a header.
//! This matters most in a shared-database deployment, where nothing else
//! separates the two, so that is exactly what this test sets up.
//! Skips when NEBULA_TEST_DATABASE_URL is unset.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use nebula::config::{Config, DatabaseConfig};
use nebula::{AdministrationModule, Kernel, db};
use tower::ServiceExt;

async fn send(
    router: &Router,
    path: &str,
    tenant: Option<&str>,
    bearer: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(path)
        .header("content-type", "application/json");
    if let Some(tenant) = tenant {
        req = req.header("X-Tenant", tenant);
    }
    if let Some(token) = bearer {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let body = match body {
        Some(json) => Body::from(json.to_string()),
        None => Body::empty(),
    };
    let response = router.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn a_token_cannot_be_spent_in_another_tenant() {
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
        "DROP TABLE IF EXISTS user_directory; DROP TABLE IF EXISTS currencies; \
         DROP TABLE IF EXISTS audit_logs; DROP TABLE IF EXISTS permission_grants; \
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
    // The hostile case: both tenants share the main database, so the token
    // check is the *only* thing keeping them apart.
    config.multitenancy.provision_databases = false;
    config.multitenancy.allow_shared_database = true;
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

    for (tenant, email) in [("alpha", "a@alpha.test"), ("beta", "b@beta.test")] {
        let (status, body) = send(
            &router,
            "/auth/register",
            None,
            None,
            Some(serde_json::json!({
                "tenant_name": tenant,
                "email": email,
                "password": "hunter2hunter2",
                "first_name": "T",
                "last_name": "Est",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "register {tenant} failed: {body}");
    }

    let (status, body) = send(
        &router,
        "/auth/login",
        Some("alpha"),
        None,
        Some(serde_json::json!({ "login": "a@alpha.test", "password": "hunter2hunter2" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "login failed: {body}");
    let alpha_token = body["access_token"].as_str().unwrap().to_string();

    // The token works in its own tenant.
    let (status, _) = send(
        &router,
        "/auth/me/permissions",
        Some("alpha"),
        Some(&alpha_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a token must work in its own tenant");

    // ...and nowhere else. This is the regression: it used to answer 200 and
    // serve beta's data from the shared database.
    let (status, _) = send(
        &router,
        "/auth/me/permissions",
        Some("beta"),
        Some(&alpha_token),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a tenant's token must be refused against another tenant"
    );

    // A tenant's token may not act on the host either (no tenant header).
    let (status, _) = send(&router, "/auth/me/permissions", None, Some(&alpha_token), None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a tenant's token must be refused in host context"
    );
}
