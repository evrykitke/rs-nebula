//! Proof of concept: company setup against a live database — the seeded
//! currency table (anonymous list, custom currencies, undeletable system
//! rows), a currency chosen at registration, the company profile
//! (display name, tax identifiers, default currency) and the logo
//! upload served back from `/public/{slug}/{id}/`.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use nebula::config::{Config, DatabaseConfig};
use nebula::{AdministrationModule, Kernel, db};
use tower::ServiceExt;

const TENANT: &str = "paintco";

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
    // Anonymous calls (register, the currency list, credential-resolved
    // login) carry no tenant header — that is the point of them.
    if let Some(token) = bearer {
        req = req
            .header("X-Tenant", TENANT)
            .header("authorization", format!("Bearer {token}"));
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

/// A minimal multipart/form-data request with one `file` field.
async fn upload(
    router: &Router,
    path: &str,
    bearer: &str,
    file_name: &str,
    bytes: &[u8],
) -> (StatusCode, serde_json::Value) {
    const BOUNDARY: &str = "XNEBULATESTBOUNDARY";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("X-Tenant", TENANT)
                .header("authorization", format!("Bearer {bearer}"))
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
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
async fn company_setup_end_to_end() {
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

    let files_root = std::env::temp_dir().join(format!(
        "nebula-test-files-{}",
        uuid::Uuid::new_v4().simple()
    ));

    let mut config = Config::default();
    config.database = DatabaseConfig {
        url: url.as_str().into(),
        auto_migrate: true,
        ..DatabaseConfig::default()
    };
    config.multitenancy.enabled = true;
    // This test asserts against the main database; no per-tenant database.
    config.multitenancy.provision_databases = false;
    config.multitenancy.allow_shared_database = true;
    config.auth.jwt_secret = "test-secret-not-for-production".into();
    config.files.root = files_root.to_string_lossy().to_string();

    let app = Kernel::builder()
        .with_config(config)
        .add_module(AdministrationModule)
        .build()
        .unwrap()
        .init()
        .await
        .expect("boot must succeed");
    let router = app.router();

    // -- The currency list is seeded and anonymous: onboarding needs it
    //    before any account exists.
    let (status, body) = send(&router, "GET", "/currencies", None, None).await;
    assert_eq!(status, StatusCode::OK, "list currencies failed: {body}");
    let currencies = body.as_array().unwrap();
    assert!(currencies.len() >= 50, "world currencies must be seeded");
    let kes = currencies
        .iter()
        .find(|c| c["code"] == "KES")
        .expect("KES must be seeded");
    assert_eq!(kes["is_system"], true);
    assert_eq!(kes["minor_units"], 2);
    let jpy = currencies.iter().find(|c| c["code"] == "JPY").unwrap();
    assert_eq!(jpy["minor_units"], 0, "JPY has no minor unit");

    // -- Registration validates and stores the chosen currency.
    let (status, body) = send(
        &router,
        "POST",
        "/auth/register",
        None,
        Some(serde_json::json!({
            "tenant_name": TENANT,
            "company_display_name": "Paint Co",
            "currency": "XXX",
            "email": "boss@paintco.test",
            "password": "hunter2hunter2",
            "first_name": "Pa",
            "last_name": "Int",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "unknown currency: {body}");

    let (status, body) = send(
        &router,
        "POST",
        "/auth/register",
        None,
        Some(serde_json::json!({
            "tenant_name": TENANT,
            "company_display_name": "Paint Co",
            "currency": "KES",
            "email": "boss@paintco.test",
            "password": "hunter2hunter2",
            "first_name": "Pa",
            "last_name": "Int",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "register failed: {body}");
    let tenant_id = body["tenant_id"].as_str().unwrap().to_string();
    let boss = login(&router, "boss@paintco.test", "hunter2hunter2").await;

    // -- The profile is readable by any user of the tenant.
    let (status, _) = send(
        &router,
        "POST",
        "/auth/users",
        Some(&boss),
        Some(serde_json::json!({
            "user_name": "clerk",
            "email": "clerk@paintco.test",
            "password": "clerkpass123",
            "first_name": "Cle",
            "last_name": "Rk",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let clerk = login(&router, "clerk", "clerkpass123").await;

    let (status, body) = send(&router, "GET", "/auth/tenant/profile", Some(&clerk), None).await;
    assert_eq!(status, StatusCode::OK, "profile get failed: {body}");
    assert_eq!(body["display_name"], "Paint Co");
    assert_eq!(body["default_currency"], "KES");
    assert!(body["logo_url"].is_null());

    // -- Editing needs the tenant-settings permission and a real currency.
    let profile_update = serde_json::json!({
        "display_name": "Paint Company Ltd",
        "default_currency": "USD",
        "tax_pin": "A012345678Z",
        "vat_number": "VAT-99",
    });
    let (status, _) = send(
        &router,
        "PUT",
        "/auth/tenant/profile",
        Some(&clerk),
        Some(profile_update.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "clerks cannot edit");
    let (status, body) = send(
        &router,
        "PUT",
        "/auth/tenant/profile",
        Some(&boss),
        Some(serde_json::json!({ "display_name": "x", "default_currency": "NOPE" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "bad currency: {body}");
    let (status, body) = send(
        &router,
        "PUT",
        "/auth/tenant/profile",
        Some(&boss),
        Some(profile_update),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "profile update failed: {body}");
    assert_eq!(body["display_name"], "Paint Company Ltd");
    assert_eq!(body["default_currency"], "USD");
    assert_eq!(body["tax_pin"], "A012345678Z");
    assert_eq!(body["vat_number"], "VAT-99");

    // -- Logo upload: stored at /public/{slug}/{id}/logo.{ext} and served
    //    back; executables and svg (a script container) are refused.
    let (status, body) = upload(&router, "/auth/tenant/logo", &boss, "virus.exe", b"MZ").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "exe must be refused: {body}");
    let (status, body) =
        upload(&router, "/auth/tenant/logo", &boss, "logo.svg", b"<svg onload=alert(1)/>").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "svg must be refused: {body}");
    // The name lies: a .png carrying HTML. Content validation must catch
    // it — otherwise /public serves stored XSS.
    let (status, body) = upload(
        &router,
        "/auth/tenant/logo",
        &boss,
        "logo.png",
        b"<!DOCTYPE html><script>alert(document.cookie)</script>",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "html-as-png must be refused: {body}");

    let png: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
    let (status, body) = upload(&router, "/auth/tenant/logo", &boss, "logo.png", png).await;
    assert_eq!(status, StatusCode::OK, "logo upload failed: {body}");
    let logo_url = body["logo_url"].as_str().unwrap().to_string();
    assert!(
        logo_url.starts_with(&format!("/public/{TENANT}/")) && logo_url.ends_with("/logo.png"),
        "slug/id/resource layout, got {logo_url}"
    );
    assert!(!logo_url.contains(&tenant_id), "the slug replaced the tenant id");

    let fetch = |url: String| {
        let router = router.clone();
        async move {
            let response = router
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(&url)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            (status, to_bytes(response.into_body(), usize::MAX).await.unwrap())
        }
    };
    let (status, served) = fetch(logo_url.clone()).await;
    assert_eq!(status, StatusCode::OK, "logo must be served");
    assert_eq!(&served[..], png, "served bytes must match the upload");

    // -- Re-uploading replaces the logo: new id, new URL, old file gone.
    let png2: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 9, 9, 9];
    let (status, body) = upload(&router, "/auth/tenant/logo", &boss, "rebrand.png", png2).await;
    assert_eq!(status, StatusCode::OK, "second upload failed: {body}");
    let second_url = body["logo_url"].as_str().unwrap().to_string();
    assert_ne!(second_url, logo_url, "every upload gets a fresh URL");
    let (status, served) = fetch(second_url).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&served[..], png2);
    let (status, _) = fetch(logo_url).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the stale logo must be removed");

    // -- Custom currencies: admins add and remove them; system rows and
    //    plain users are refused.
    let new_currency = serde_json::json!({ "code": "QQQ", "name": "Test Unit", "minor_units": 2 });
    let (status, _) = send(
        &router,
        "POST",
        "/currencies",
        Some(&clerk),
        Some(new_currency.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = send(
        &router,
        "POST",
        "/currencies",
        Some(&boss),
        Some(new_currency),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create currency failed: {body}");
    assert_eq!(body["is_system"], false);
    let custom_id = body["id"].as_str().unwrap().to_string();

    let kes_id = kes["id"].as_str().unwrap();
    let (status, body) = send(
        &router,
        "DELETE",
        &format!("/currencies/{kes_id}"),
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "system currencies must survive: {body}"
    );
    let (status, body) = send(
        &router,
        "DELETE",
        &format!("/currencies/{custom_id}"),
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "delete custom failed: {body}");

    let _ = std::fs::remove_dir_all(&files_root);
}
