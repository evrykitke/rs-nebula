//! Proof of concept: the audit trail against a live database — mutating
//! requests get `request` rows with ip/user-agent/status/duration,
//! admin mutations get before/after entity snapshots, the diff endpoint
//! shows only what changed, bodies are never recorded, and the trail is
//! permission-guarded.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use nebula::audit::{FieldChange, diff};
use nebula::config::{Config, DatabaseConfig};
use nebula::{AdministrationModule, Kernel, db};
use tower::ServiceExt;

const CLIENT_IP: &str = "203.0.113.9";
const USER_AGENT: &str = "nebula-tests/0.1";

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
        .header("content-type", "application/json")
        .header("x-forwarded-for", CLIENT_IP)
        .header("user-agent", USER_AGENT);
    if path != "/auth/register" {
        req = req.header("X-Tenant", "auditco");
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
async fn audit_trail_end_to_end() {
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
    // This test asserts against the main database; no per-tenant database.
    config.multitenancy.provision_databases = false;
    config.auth.jwt_secret = "test-secret-not-for-production".into();
    let audit_config = config.audit.clone();

    let app = Kernel::builder()
        .with_config(config)
        .add_module(AdministrationModule)
        .build()
        .unwrap()
        .init()
        .await
        .expect("boot must succeed");
    let router = app.router();

    let (status, body) = send(
        &router,
        "POST",
        "/auth/register",
        None,
        Some(serde_json::json!({
            "tenant_name": "auditco",
            "email": "boss@auditco.test",
            "password": "hunter2hunter2",
            "first_name": "Aud",
            "last_name": "Itor",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "register failed: {body}");
    let tenant_id = body["tenant_id"].as_str().unwrap().to_string();
    let boss = login(&router, "boss@auditco.test", "hunter2hunter2").await;

    let (status, body) = send(
        &router,
        "POST",
        "/auth/users",
        Some(&boss),
        Some(serde_json::json!({
            "user_name": "clerk",
            "email": "clerk@auditco.test",
            "password": "clerkpass123",
            "first_name": "Cle",
            "last_name": "Rk",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create user failed: {body}");
    let clerk_id = body["id"].as_str().unwrap().to_string();

    // -- Entity rows: registration and user creation were snapshotted,
    //    with the full request context on every row.
    let (status, body) = send(
        &router,
        "GET",
        "/audit/logs?action=create&entity_type=user",
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "list failed: {body}");
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), 2, "boss + clerk create rows: {body}");
    let clerk_row = rows
        .iter()
        .find(|r| r["entity_id"] == clerk_id.to_string())
        .expect("clerk create row");
    assert_eq!(clerk_row["ip_address"], CLIENT_IP);
    assert_eq!(clerk_row["user_agent"], USER_AGENT);
    assert_eq!(clerk_row["new_values"]["email"], "clerk@auditco.test");
    assert!(
        clerk_row["new_values"].get("password_hash").is_none(),
        "snapshots must be the client-safe view"
    );
    assert!(clerk_row["request_id"].is_string(), "rows link to traces");

    // -- Request rows: mutating requests are logged with status and
    //    duration but never their bodies; reads are not logged.
    let (status, body) = send(
        &router,
        "GET",
        "/audit/logs?action=request",
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows = body.as_array().unwrap();
    let login_row = rows
        .iter()
        .find(|r| r["path"] == "/auth/login")
        .expect("login request row");
    assert_eq!(login_row["status_code"], 200);
    assert!(login_row["old_values"].is_null() && login_row["new_values"].is_null());
    assert!(login_row["duration_ms"].is_i64());
    assert!(
        !rows.iter().any(|r| r["method"] == "GET"),
        "reads are not logged by default"
    );

    // -- Special events: logins succeed and fail as plain messages with
    //    full request context, no snapshots.
    let (status, _) = send(
        &router,
        "POST",
        "/auth/login",
        None,
        Some(serde_json::json!({ "login": "clerk", "password": "wrong-password" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = send(
        &router,
        "GET",
        "/audit/logs?action=event",
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows = body.as_array().unwrap();
    let login_event = rows
        .iter()
        .find(|r| r["message"] == "boss@auditco.test logged in")
        .expect("login event");
    assert!(login_event["user_id"].is_string(), "events know who");
    assert!(
        login_event["old_values"].is_null(),
        "events carry no snapshots"
    );
    assert_eq!(login_event["ip_address"], CLIENT_IP);
    assert!(
        rows.iter()
            .any(|r| r["message"] == "failed login attempt for \"clerk\""),
        "failed logins are events too: {body}"
    );

    // -- The diff view shows exactly what changed.
    let (status, body) = send(
        &router,
        "PUT",
        &format!("/auth/users/{clerk_id}/admin"),
        Some(&boss),
        Some(serde_json::json!({ "is_admin": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "promote failed: {body}");
    let (_, body) = send(
        &router,
        "GET",
        "/audit/logs?action=update&entity_type=user",
        Some(&boss),
        None,
    )
    .await;
    let update_id = body.as_array().unwrap()[0]["id"].as_i64().unwrap();
    let (status, body) = send(
        &router,
        "GET",
        &format!("/audit/logs/{update_id}/diff"),
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "diff failed: {body}");
    let changes = body["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "only the changed field: {body}");
    assert_eq!(changes[0]["field"], "is_tenant_admin");
    assert_eq!(changes[0]["old"], false);
    assert_eq!(changes[0]["new"], true);

    // -- The trail is permission-guarded (clerk is now admin; demote
    //    them back via a fresh non-admin instead).
    let (status, _) = send(
        &router,
        "POST",
        "/auth/users",
        Some(&boss),
        Some(serde_json::json!({
            "user_name": "intern",
            "email": "intern@auditco.test",
            "password": "internpass12",
            "first_name": "In",
            "last_name": "Tern",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let intern = login(&router, "intern", "internpass12").await;
    let (status, _) = send(&router, "GET", "/audit/logs", Some(&intern), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(
        &router,
        "PUT",
        "/audit/retention",
        Some(&intern),
        Some(serde_json::json!({ "retention_days": 60 })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // -- Retention: 30 days by default, tenant override capped at 180.
    sea_orm::ConnectionTrait::execute_unprepared(
        &admin_db,
        &format!(
            "INSERT INTO audit_logs (tenant_id, method, path, action, message, created_at) \
             VALUES ('{tenant_id}', 'POST', '/old', 'event', 'ancient event', \
                     now() - interval '40 days')"
        ),
    )
    .await
    .expect("old row must insert");
    let ancient_visible = || async {
        let (_, body) = send(
            &router,
            "GET",
            "/audit/logs?action=event&limit=500",
            Some(&boss),
            None,
        )
        .await;
        body.as_array()
            .unwrap()
            .iter()
            .any(|r| r["message"] == "ancient event")
    };
    assert!(ancient_visible().await);

    let (status, body) = send(
        &router,
        "PUT",
        "/audit/retention",
        Some(&boss),
        Some(serde_json::json!({ "retention_days": 200 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "cap is six months: {body}");

    let (status, body) = send(
        &router,
        "PUT",
        "/audit/retention",
        Some(&boss),
        Some(serde_json::json!({ "retention_days": 60 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "override failed: {body}");
    assert_eq!(body["effective_days"], 60);

    let tenants = app.tenants();
    nebula::audit::pruner::prune_once(app.database().unwrap(), tenants.as_ref(), &audit_config)
        .await
        .expect("prune must run");
    assert!(
        ancient_visible().await,
        "a 60-day window keeps a 40-day-old row"
    );

    let (status, body) = send(
        &router,
        "PUT",
        "/audit/retention",
        Some(&boss),
        Some(serde_json::json!({ "retention_days": null })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["effective_days"], 30, "back to the system default");

    nebula::audit::pruner::prune_once(app.database().unwrap(), tenants.as_ref(), &audit_config)
        .await
        .expect("prune must run");
    assert!(
        !ancient_visible().await,
        "the default 30-day window prunes a 40-day-old row"
    );
}

#[test]
fn diff_reports_only_changed_fields() {
    let old = serde_json::json!({ "name": "a", "count": 1, "kept": true });
    let new = serde_json::json!({ "name": "b", "count": 1, "added": "x" });
    let changes = diff(Some(&old), Some(&new));
    assert_eq!(
        changes,
        vec![
            FieldChange {
                field: "added".into(),
                old: None,
                new: Some(serde_json::json!("x")),
            },
            FieldChange {
                field: "kept".into(),
                old: Some(serde_json::json!(true)),
                new: None,
            },
            FieldChange {
                field: "name".into(),
                old: Some(serde_json::json!("a")),
                new: Some(serde_json::json!("b")),
            },
        ]
    );

    assert!(diff(None, None).is_empty());
    let same = serde_json::json!({ "x": 1 });
    assert!(diff(Some(&same), Some(&same)).is_empty());

    let created = diff(None, Some(&serde_json::json!({ "x": 1 })));
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].old, None);
}
