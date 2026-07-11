//! Proof of concept: the document-numbering primitive end to end against a
//! live database — a module declares a series, a handler allocates numbers
//! inside its own transaction, and the sequence stays gap-free even when a
//! transaction rolls back. Skips when NEBULA_TEST_DATABASE_URL is unset.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::{Extension, Json, Router};
use nebula::config::{Config, DatabaseConfig};
use nebula::{Kernel, Module, ModuleContext, Numbering, Reset, SeriesDef};
use sea_orm::{DatabaseConnection, TransactionTrait};

/// A stand-in for a future Sales module: declares one invoice series and
/// exposes handlers that exercise the numbering primitive.
struct Sales {
    key: String,
}

impl Module for Sales {
    fn name(&self) -> &'static str {
        "sales-test"
    }

    fn configure(&self, ctx: &mut ModuleContext) {
        ctx.declare_series(
            SeriesDef::new(&self.key, "Sales Invoice", "INV-{YYYY}-{SEQ:5}", Reset::Yearly)
                .expect("template must be valid"),
        );

        let commit_key = self.key.clone();
        let rollback_key = self.key.clone();
        let peek_key = self.key.clone();
        let set_key = self.key.clone();
        let clear_key = self.key.clone();
        let effective_key = self.key.clone();

        ctx.add_routes(
            Router::new()
                .route(
                    "/alloc",
                    get(move |Extension(db): Extension<DatabaseConnection>,
                              Extension(numbering): Extension<Numbering>| {
                        let key = commit_key.clone();
                        async move {
                            let txn = db.begin().await.unwrap();
                            let number = numbering.next(&txn, &key).await.unwrap();
                            txn.commit().await.unwrap();
                            Json(serde_json::json!({
                                "formatted": number.formatted,
                                "sequence": number.sequence,
                                "period": number.period,
                            }))
                        }
                    }),
                )
                .route(
                    "/alloc-rollback",
                    get(move |Extension(db): Extension<DatabaseConnection>,
                              Extension(numbering): Extension<Numbering>| {
                        let key = rollback_key.clone();
                        async move {
                            // Allocate, then throw the transaction away: a
                            // document that never commits must not burn a
                            // number.
                            let txn = db.begin().await.unwrap();
                            let number = numbering.next(&txn, &key).await.unwrap();
                            txn.rollback().await.unwrap();
                            Json(serde_json::json!({ "sequence": number.sequence }))
                        }
                    }),
                )
                .route(
                    "/peek",
                    get(move |Extension(db): Extension<DatabaseConnection>,
                              Extension(numbering): Extension<Numbering>| {
                        let key = peek_key.clone();
                        async move {
                            let formatted = numbering.peek(&db, &key).await.unwrap();
                            Json(serde_json::json!({ "formatted": formatted }))
                        }
                    }),
                )
                .route(
                    "/override-set",
                    get(move |Extension(db): Extension<DatabaseConnection>,
                              Extension(numbering): Extension<Numbering>| {
                        let key = set_key.clone();
                        async move {
                            numbering
                                .set_override(&db, &key, "BILL-{SEQ:3}", Reset::Never)
                                .await
                                .unwrap();
                            StatusCode::OK
                        }
                    }),
                )
                .route(
                    "/override-clear",
                    get(move |Extension(db): Extension<DatabaseConnection>,
                              Extension(numbering): Extension<Numbering>| {
                        let key = clear_key.clone();
                        async move {
                            numbering.clear_override(&db, &key).await.unwrap();
                            StatusCode::OK
                        }
                    }),
                )
                .route(
                    "/effective",
                    get(move |Extension(db): Extension<DatabaseConnection>,
                              Extension(numbering): Extension<Numbering>| {
                        let key = effective_key.clone();
                        async move {
                            let def = numbering.effective(&db, &key).await.unwrap();
                            Json(serde_json::json!({ "template": def.template() }))
                        }
                    }),
                )
                .route(
                    "/alloc-unknown",
                    get(|Extension(db): Extension<DatabaseConnection>,
                         Extension(numbering): Extension<Numbering>| async move {
                        let txn = db.begin().await.unwrap();
                        match numbering.next(&txn, "nobody.declared.this").await {
                            Ok(_) => StatusCode::OK,
                            Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
                        }
                    }),
                ),
        );
    }
}

async fn get_json(router: &Router, path: &str) -> serde_json::Value {
    let response = tower::ServiceExt::oneshot(
        router.clone(),
        Request::get(path).body(Body::empty()).unwrap(),
    )
    .await
    .unwrap();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn get_status(router: &Router, path: &str) -> StatusCode {
    tower::ServiceExt::oneshot(
        router.clone(),
        Request::get(path).body(Body::empty()).unwrap(),
    )
    .await
    .unwrap()
    .status()
}

#[tokio::test]
async fn allocates_gap_free_numbers_through_the_module_surface() {
    let Ok(url) = std::env::var("NEBULA_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set NEBULA_TEST_DATABASE_URL to run database tests");
        return;
    };

    // A unique series key so parallel tests never share a counter row.
    let key = format!("sales.invoice.test.{}", uuid::Uuid::new_v4().simple());
    let year = chrono::Utc::now().format("%Y").to_string();

    let mut config = Config::default();
    config.database = DatabaseConfig {
        url: url.as_str().into(),
        auto_migrate: true,
        ..DatabaseConfig::default()
    };

    let app = Kernel::builder()
        .with_config(config)
        .add_module(Sales { key })
        .build()
        .expect("kernel must build")
        .init()
        .await
        .expect("boot must succeed");
    let router = app.router();

    // Consecutive allocations increment from one and render the template.
    let first = get_json(&router, "/alloc").await;
    assert_eq!(first["sequence"], 1);
    assert_eq!(first["formatted"], format!("INV-{year}-00001"));
    assert_eq!(first["period"], year);

    let second = get_json(&router, "/alloc").await;
    assert_eq!(second["sequence"], 2);
    assert_eq!(second["formatted"], format!("INV-{year}-00002"));

    // Peek shows what is next without consuming it.
    let peeked = get_json(&router, "/peek").await;
    assert_eq!(peeked["formatted"], format!("INV-{year}-00003"));
    let peeked_again = get_json(&router, "/peek").await;
    assert_eq!(peeked_again["formatted"], format!("INV-{year}-00003"));

    // An allocation whose transaction rolls back must leave no gap.
    let rolled = get_json(&router, "/alloc-rollback").await;
    assert_eq!(rolled["sequence"], 3, "the rolled-back call still drew 3");

    let third = get_json(&router, "/alloc").await;
    assert_eq!(
        third["sequence"], 3,
        "3 must be reused: the rolled-back allocation was undone"
    );
    assert_eq!(third["formatted"], format!("INV-{year}-00003"));

    // Numbering a series nobody declared is a programming error (500).
    assert_eq!(
        get_status(&router, "/alloc-unknown").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn tenant_override_changes_the_format_then_reverts() {
    let Ok(url) = std::env::var("NEBULA_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set NEBULA_TEST_DATABASE_URL to run database tests");
        return;
    };

    let key = format!("sales.invoice.test.{}", uuid::Uuid::new_v4().simple());
    let year = chrono::Utc::now().format("%Y").to_string();

    let mut config = Config::default();
    config.database = DatabaseConfig {
        url: url.as_str().into(),
        auto_migrate: true,
        ..DatabaseConfig::default()
    };

    let app = Kernel::builder()
        .with_config(config)
        .add_module(Sales { key })
        .build()
        .expect("kernel must build")
        .init()
        .await
        .expect("boot must succeed");
    let router = app.router();

    // Zero config: the declared default is in force.
    let effective = get_json(&router, "/effective").await;
    assert_eq!(effective["template"], "INV-{YYYY}-{SEQ:5}");

    // A tenant overrides the format; the next allocation uses it.
    assert_eq!(get_status(&router, "/override-set").await, StatusCode::OK);
    let effective = get_json(&router, "/effective").await;
    assert_eq!(effective["template"], "BILL-{SEQ:3}");

    let billed = get_json(&router, "/alloc").await;
    assert_eq!(billed["sequence"], 1);
    assert_eq!(billed["formatted"], "BILL-001");
    assert_eq!(billed["period"], "-");

    // Clearing the override reverts to the module default (a fresh
    // yearly period, so the sequence starts again at one).
    assert_eq!(get_status(&router, "/override-clear").await, StatusCode::OK);
    let reverted = get_json(&router, "/alloc").await;
    assert_eq!(reverted["formatted"], format!("INV-{year}-00001"));
    assert_eq!(reverted["period"], year);
}
