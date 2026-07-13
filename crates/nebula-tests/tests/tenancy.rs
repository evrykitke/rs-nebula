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
            "DROP TABLE IF EXISTS user_directory; DROP TABLE IF EXISTS currencies; DROP TABLE IF EXISTS audit_logs; DROP TABLE IF EXISTS permission_grants; \
             DROP TABLE IF EXISTS user_roles; DROP TABLE IF EXISTS roles; \
             DROP TABLE IF EXISTS users; DROP TABLE IF EXISTS tenants; \
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
    // This case covers the two explicit routings — a tenant sharing the main
    // database and one given a connection string. Provisioning gets its own
    // test below.
    config.multitenancy.provision_databases = false;
    config.multitenancy.allow_shared_database = true;
    // Room for acme + globex, then the cap trips.
    config.multitenancy.max_tenants = 2;
    // This test flips is_active with raw SQL, which bypasses the manager's
    // cache invalidation — resolve fresh every time.
    config.multitenancy.directory_cache_secs = 0;

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
            default_currency: None,
        })
        .await
        .expect("shared tenant must be created");
    manager
        .create(NewTenant {
            name: "globex".into(),
            display_name: "Globex Corp".into(),
            connection_string: Some(t2_url),
            default_currency: None,
        })
        .await
        .expect("own-db tenant must be created");

    let dup = manager
        .create(NewTenant {
            name: "acme".into(),
            display_name: "Duplicate".into(),
            connection_string: None,
            default_currency: None,
        })
        .await;
    assert!(matches!(dup, Err(nebula::Error::Conflict(_))));
    assert!(
        manager
            .create(NewTenant {
                name: "Bad Name!".into(),
                display_name: "x".into(),
                connection_string: None,
                default_currency: None,
            })
            .await
            .is_err()
    );

    // Reserved slugs (framework namespaces: /public containers, cache
    // scopes) cannot be claimed.
    let reserved = manager
        .create(NewTenant {
            name: "reports".into(),
            display_name: "Sneaky".into(),
            connection_string: None,
            default_currency: None,
        })
        .await;
    assert!(matches!(reserved, Err(nebula::Error::Validation(_))));

    // The deployment cap (max_tenants = 2) refuses a third tenant.
    let capped = manager
        .create(NewTenant {
            name: "initech".into(),
            display_name: "Initech".into(),
            connection_string: None,
            default_currency: None,
        })
        .await;
    assert!(
        matches!(capped, Err(nebula::Error::Validation(_))),
        "the tenant cap must refuse a third tenant"
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

    // Inactive tenant -> 404, indistinguishable from an unknown one so the
    // header is not an existence oracle.
    admin
        .execute_unprepared("UPDATE tenants SET is_active = FALSE WHERE name = 'acme'")
        .await
        .unwrap();
    let (status, _) = get_json(&router, "/whoami", Some("acme")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Without `allow_shared_database`, a tenant that would land on the main
/// database (provisioning off, no explicit connection string) is refused:
/// business tables have no per-row tenant isolation there, so sharing must
/// be a deliberate opt-in, never a config accident.
#[tokio::test]
async fn refuses_shared_database_tenants_without_opt_in() {
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
    // A private directory database so this test never races the others.
    let guard_main = format!("{main_db_name}_guard");
    admin
        .execute_unprepared(&format!("DROP DATABASE IF EXISTS {guard_main} WITH (FORCE)"))
        .await
        .ok();
    admin
        .execute_unprepared(&format!("CREATE DATABASE {guard_main}"))
        .await
        .expect("must create the directory database");

    let mut config = Config::default();
    config.database = DatabaseConfig {
        url: main_url.replace(&main_db_name, &guard_main).as_str().into(),
        auto_migrate: true,
        ..DatabaseConfig::default()
    };
    config.multitenancy.enabled = true;
    config.multitenancy.provision_databases = false;
    // allow_shared_database stays at its default: false.

    let app = Kernel::builder()
        .with_config(config)
        .add_module(WhoAmI)
        .build()
        .expect("kernel must build")
        .init()
        .await
        .expect("boot must succeed");

    let refused = app
        .tenants()
        .expect("tenant manager must exist")
        .create(NewTenant {
            name: "freeloader".into(),
            display_name: "Freeloader Inc".into(),
            connection_string: None,
            default_currency: None,
        })
        .await;
    assert!(
        matches!(refused, Err(nebula::Error::Validation(_))),
        "a shared-database tenant must be refused without the opt-in, got {refused:?}"
    );

    drop(app);
    admin
        .execute_unprepared(&format!("DROP DATABASE IF EXISTS {guard_main} WITH (FORCE)"))
        .await
        .ok();
}

/// With `provision_databases` on, creating a tenant cuts and migrates a
/// dedicated database named `{slug}-{key}` — no explicit connection
/// string required — and the tenant then runs isolated on it.
#[tokio::test]
async fn provisions_a_database_per_tenant() {
    let Ok(main_url) = std::env::var("NEBULA_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set NEBULA_TEST_DATABASE_URL to run database tests");
        return;
    };

    // Run against a private directory database so this test never races
    // the shared-table setup of the end-to-end test above (both run in
    // the same binary, in parallel, over NEBULA_TEST_DATABASE_URL).
    let admin = db::connect(&DatabaseConfig {
        url: main_url.as_str().into(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("must connect to main");

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
    let prov_main = format!("{main_db_name}_prov");
    admin
        .execute_unprepared(&format!("DROP DATABASE IF EXISTS {prov_main} WITH (FORCE)"))
        .await
        .ok();
    admin
        .execute_unprepared(&format!("CREATE DATABASE {prov_main}"))
        .await
        .expect("must create the directory database");
    let prov_main_url = main_url.replace(&main_db_name, &prov_main);

    let mut config = Config::default();
    config.database = DatabaseConfig {
        url: prov_main_url.as_str().into(),
        auto_migrate: true,
        ..DatabaseConfig::default()
    };
    config.multitenancy.enabled = true;
    config.multitenancy.provision_databases = true;

    let app = Kernel::builder()
        .with_config(config)
        .add_module(WhoAmI)
        .build()
        .expect("kernel must build")
        .init()
        .await
        .expect("boot with provisioning must succeed");

    let manager = app.tenants().expect("tenant manager must exist");
    let tenant = manager
        .create(NewTenant {
            name: "provco".into(),
            display_name: "Provisioned Co".into(),
            connection_string: None,
            default_currency: None,
        })
        .await
        .expect("tenant must be provisioned");

    // A dedicated database was cut and recorded — not the main one.
    let conn = tenant
        .connection_string
        .clone()
        .expect("a provisioned tenant carries its own connection string");
    let db_name = conn.rsplit('/').next().unwrap().to_string();
    assert!(
        db_name.starts_with("provco-") && db_name.len() > "provco-".len(),
        "database name {db_name:?} should be slug-keyed (provco-<key>)"
    );
    assert_ne!(db_name, main_db_name);

    // The tenant resolves to and runs on its own database.
    let router = app.router();
    let (status, body) = get_json(&router, "/whoami", Some("provco")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tenant"], "provco");
    assert_eq!(body["database"], db_name.as_str());

    // The framework schema is present in the fresh database (a tenant
    // user store is what the migrations exist to create).
    let tenant_db = db::connect(&DatabaseConfig {
        url: conn.as_str().into(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("must connect to the provisioned database");
    let has_users: bool = {
        let row = tenant_db
            .query_one(sea_orm::Statement::from_string(
                tenant_db.get_database_backend(),
                "SELECT to_regclass('public.users') IS NOT NULL AS present",
            ))
            .await
            .unwrap()
            .unwrap();
        row.try_get("", "present").unwrap()
    };
    assert!(has_users, "the provisioned database must be migrated");

    // Tidy up the databases this test cut: the provisioned tenant's and
    // the private directory database.
    drop(tenant_db);
    drop(app);
    admin
        .execute_unprepared(&format!("DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)"))
        .await
        .ok();
    admin
        .execute_unprepared(&format!("DROP DATABASE IF EXISTS {prov_main} WITH (FORCE)"))
        .await
        .ok();
}
