//! Proof of concept: database connectivity and readiness.
//!
//! Tests that need a live Postgres read `NEBULA_TEST_DATABASE_URL` and
//! skip (loudly) when it is not set, so the suite still passes on
//! machines without a database.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use nebula::config::{Config, DatabaseConfig};
use nebula::{db, Kernel};
use tower::ServiceExt;

fn test_db_url() -> Option<String> {
    let url = std::env::var("NEBULA_TEST_DATABASE_URL").ok();
    if url.is_none() {
        eprintln!("SKIPPED: set NEBULA_TEST_DATABASE_URL to run database tests");
    }
    url
}

fn db_config(url: &str) -> DatabaseConfig {
    DatabaseConfig {
        url: url.into(),
        ..DatabaseConfig::default()
    }
}

#[tokio::test]
async fn connects_and_pings_a_live_database() {
    let Some(url) = test_db_url() else { return };
    let db = db::connect(&db_config(&url)).await.expect("must connect");
    db::ping(&db).await.expect("must ping");
}

#[tokio::test]
async fn boot_fails_fast_when_database_is_unreachable() {
    // Nothing listens on port 59999; boot must error, not hang.
    let config = DatabaseConfig {
        connect_timeout_secs: 3,
        ..db_config("postgres://nobody:wrong@127.0.0.1:59999/nope")
    };
    let err = db::connect(&config).await;
    assert!(err.is_err(), "connecting to a dead database must fail");
}

#[tokio::test]
async fn readiness_reports_database_up() {
    let Some(url) = test_db_url() else { return };

    let mut config = Config::default();
    config.database = db_config(&url);

    let app = Kernel::builder()
        .with_config(config)
        .build()
        .expect("kernel must build")
        .init()
        .await
        .expect("boot with database must succeed");

    let response = app
        .router()
        .oneshot(Request::get("/health/ready").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["database"], "up");
}
