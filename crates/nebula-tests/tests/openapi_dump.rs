//! Utility, not a test of behavior: boots the full application module set
//! (the same composition as nebula-server) against the throwaway test
//! database and writes the OpenAPI document to disk, so client proxies can
//! be regenerated without a running server. Ignored by default — run it
//! on demand:
//!
//! ```sh
//! NEBULA_OPENAPI_OUT=/path/to/openapi.json \
//!   cargo test -p nebula-tests --test openapi_dump -- --ignored
//! ```
//!
//! Skips when NEBULA_TEST_DATABASE_URL is unset. Defaults the output to
//! target/openapi.json when NEBULA_OPENAPI_OUT is unset.

use axum::body::{Body, to_bytes};
use axum::http::Request;
use nebula::config::{Config, DatabaseConfig, MigrationsConfig};
use nebula::Kernel;
use nebula_apps::{AccountingApp, ScmApp, WorkspaceApp};
use tower::ServiceExt;

#[tokio::test]
#[ignore = "utility: dumps the OpenAPI document for proxy generation"]
async fn dump_openapi() {
    let Ok(url) = std::env::var("NEBULA_TEST_DATABASE_URL") else {
        eprintln!("NEBULA_TEST_DATABASE_URL unset; skipping");
        return;
    };

    let mut config = Config::default();
    config.auth.jwt_secret = "openapi-dump".into();
    config.database = DatabaseConfig {
        url: url.as_str().into(),
        auto_migrate: true,
        ..DatabaseConfig::default()
    };
    config.migrations = MigrationsConfig {
        root: format!("{}/../../migrations", env!("CARGO_MANIFEST_DIR")),
    };

    let app = Kernel::builder()
        .with_config(config)
        .add_module(WorkspaceApp)
        .add_module(AccountingApp)
        .add_module(ScmApp)
        .build()
        .expect("kernel must build")
        .init()
        .await
        .expect("app must boot");

    let response = app
        .router()
        .oneshot(
            Request::get("/api-docs/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.status().is_success(), "openapi route must serve");
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();

    let out = std::env::var("NEBULA_OPENAPI_OUT")
        .unwrap_or_else(|_| format!("{}/../../target/openapi.json", env!("CARGO_MANIFEST_DIR")));
    std::fs::write(&out, &bytes).expect("write openapi document");
    eprintln!("wrote {} bytes to {out}", bytes.len());
}
