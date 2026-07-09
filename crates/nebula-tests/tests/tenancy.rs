//! Proof of concept: multitenancy end to end against a live database —
//! directory management, header resolution, per-tenant isolation, and
//! the problem+json failures for unknown/inactive tenants.
//!
//! Uses two databases: NEBULA_TEST_DATABASE_URL as the main/directory
//! database (also home of the shared tenant) and a second database
//! (same server, name suffixed `_t2`... created here) for a tenant with
//! its own connection string. Skips when the env var is unset.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use axum::routing::get;
use nebula::config::{Config, DatabaseConfig};
use nebula::tenancy::NewTenant;
use nebula::{CurrentTenant, Kernel, Module, ModuleContext, TenantDb, db};
use sea_orm::ConnectionTrait;
use tower::ServiceExt;

struct WhoAmI;

impl Module for WhoAmI {
    fn name(&self) -> &'static str {
        "whoami"
    }

    fn configure(&self, ctx: &mut ModuleContext) {
        ctx.add_routes(Router::new().route(
            "/whoami",
            get(
                |CurrentTenant(tenant): CurrentTenant, TenantDb(db): TenantDb| async move {
                    let row = db
                        .query_one(sea_orm::Statement::from_string(
                            db.get_database_backend(),
                            "SELECT current_database() AS name",
                        ))
                        .await
                        .unwrap()
                        .unwrap();
                    let database: String = row.try_get("", "name").unwrap();
                    axum::Json(serde_json::json!({
                        "tenant": tenant.map(|t| t.name),
                        "database": database,
                    }))
                },
            ),
        ));
    }
}

async fn get_json(
    router: &Router,
    path: &str,
    tenant: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::get(path);
    if let Some(name) = tenant {
        req = req.header("X-Tenant", name);
    }
    let response = router
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
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
async fn multitenancy_end_to_end() {
    let Ok(main_url) = std::env::var("NEBULA_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set NEBULA_TEST_DATABASE_URL to run database tests");
        return;
    };

    let admin = db::connect(&DatabaseConfig {
        url: main_url.as_str().into(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("must connect to main");
    admin
        .execute_unprepared(
            "DROP TABLE IF EXISTS users; DROP TABLE IF EXISTS tenants; \
             DROP TABLE IF EXISTS nebula_migrations;",
        )
        .await
        .expect("cleanup must work");

    let main_db_name: String = {
        let row = admin
            .query_one(sea_orm::Statement::from_string(
                admin.get_database_backend(),
                "SELECT current_database() AS name",
            ))
            .await
            .unwrap()
            .unwrap();
        row.try_get("", "name").unwrap()
    };
    let t2_db_name = format!("{main_db_name}_t2");
    admin
        .execute_unprepared(&format!("DROP DATABASE IF EXISTS {t2_db_name} (FORCE)"))
        .await
        .ok();
    admin
        .execute_unprepared(&format!("CREATE DATABASE {t2_db_name}"))
        .await
        .expect("must create tenant database");
    let t2_url = main_url.replace(&main_db_name, &t2_db_name);

    let mut config = Config::default();
    config.database = DatabaseConfig {
        url: main_url.as_str().into(),
        auto_migrate: true,
        ..DatabaseConfig::default()
    };
    config.multitenancy.enabled = true;

    let app = Kernel::builder()
        .with_config(config)
        .add_module(WhoAmI)
        .build()
        .expect("kernel must build")
        .init()
        .await
        .expect("boot with multitenancy must succeed");

    let manager = app.tenants().expect("tenant manager must exist");
    manager
        .create(NewTenant {
            name: "acme".into(),
            display_name: "Acme Ltd".into(),
            connection_string: None,
        })
        .await
        .expect("shared tenant must be created");
    manager
        .create(NewTenant {
            name: "globex".into(),
            display_name: "Globex Corp".into(),
            connection_string: Some(t2_url),
        })
        .await
        .expect("own-db tenant must be created");

    let dup = manager
        .create(NewTenant {
            name: "acme".into(),
            display_name: "Duplicate".into(),
            connection_string: None,
        })
        .await;
    assert!(matches!(dup, Err(nebula::Error::Conflict(_))));
    assert!(
        manager
            .create(NewTenant {
                name: "Bad Name!".into(),
                display_name: "x".into(),
                connection_string: None,
            })
            .await
            .is_err()
    );

    let router = app.router();

    // Host context: no tenant, main database.
    let (status, body) = get_json(&router, "/whoami", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tenant"], serde_json::Value::Null);
    assert_eq!(body["database"], main_db_name.as_str());

    // Shared tenant: resolved, still on the main database.
    let (status, body) = get_json(&router, "/whoami", Some("acme")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tenant"], "acme");
    assert_eq!(body["database"], main_db_name.as_str());

    // Own-database tenant: resolved and isolated.
    let (status, body) = get_json(&router, "/whoami", Some("globex")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tenant"], "globex");
    assert_eq!(body["database"], t2_db_name.as_str());

    // Unknown tenant -> 404 problem+json.
    let (status, body) = get_json(&router, "/whoami", Some("nope")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["status"], 404);

    // Inactive tenant -> 403.
    admin
        .execute_unprepared("UPDATE tenants SET is_active = FALSE WHERE name = 'acme'")
        .await
        .unwrap();
    let (status, _) = get_json(&router, "/whoami", Some("acme")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
