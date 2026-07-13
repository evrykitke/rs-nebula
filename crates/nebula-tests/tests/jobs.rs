//! Proof of concept: background jobs against live Redis (docker compose)
//! and Postgres — a module contributes a worker, a handler enqueues
//! through the Jobs client, the worker executes; the tenant migration
//! queue is reachable from the admin endpoint and permission-guarded.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use nebula::apalis::prelude::*;
use nebula::config::{Config, DatabaseConfig};
use nebula::{AdministrationModule, Kernel, Module, ModuleContext, db};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tower::ServiceExt;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CountUp {
    amount: usize,
}

async fn count_up(job: CountUp, counter: Data<Arc<AtomicUsize>>) -> Result<(), nebula::Error> {
    counter.fetch_add(job.amount, Ordering::SeqCst);
    Ok(())
}

/// A module contributing one worker on its own queue.
struct CountingModule {
    queue: String,
    counter: Arc<AtomicUsize>,
}

impl Module for CountingModule {
    fn name(&self) -> &'static str {
        "counting"
    }

    fn configure(&self, ctx: &mut ModuleContext) {
        let jobs = ctx.jobs().expect("this test enables jobs");
        let storage = jobs.storage::<CountUp>(&self.queue);
        let counter = self.counter.clone();
        ctx.add_worker(move |monitor| {
            monitor.register(
                WorkerBuilder::new("counting")
                    .data(counter)
                    .backend(storage)
                    .build_fn(count_up),
            )
        });
    }
}

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
        req = req.header("X-Tenant", "jobsco");
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
async fn background_jobs_end_to_end() {
    let Ok(url) = std::env::var("NEBULA_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set NEBULA_TEST_DATABASE_URL to run database tests");
        return;
    };
    // Redis from docker-compose; override for a non-default setup.
    let redis_url =
        std::env::var("NEBULA_TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());

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
    config.redis.url = redis_url.as_str().into();
    config.jobs.enabled = true;

    // A fresh queue name per run: Redis persists across test runs.
    let queue = format!("test-count-{}", uuid::Uuid::new_v4().simple());
    let counter = Arc::new(AtomicUsize::new(0));

    let mut app = Kernel::builder()
        .with_config(config)
        .add_module(AdministrationModule)
        .add_module(CountingModule {
            queue: queue.clone(),
            counter: counter.clone(),
        })
        .build()
        .unwrap()
        .init()
        .await
        .expect("boot must succeed (is docker compose up?)");
    let router = app.router();

    assert!(app.start_jobs(), "monitor must start when jobs are enabled");
    assert!(!app.start_jobs(), "starting twice is a no-op");

    // -- A module worker picks up jobs enqueued through the client.
    let jobs = app.jobs().expect("jobs client must exist");
    jobs.enqueue(&queue, CountUp { amount: 2 })
        .await
        .expect("enqueue must work");
    jobs.enqueue(&queue, CountUp { amount: 3 })
        .await
        .expect("enqueue must work");

    let mut waited = Duration::ZERO;
    while counter.load(Ordering::SeqCst) < 5 && waited < Duration::from_secs(15) {
        tokio::time::sleep(Duration::from_millis(200)).await;
        waited += Duration::from_millis(200);
    }
    assert_eq!(
        counter.load(Ordering::SeqCst),
        5,
        "the worker must have executed both jobs"
    );

    // -- Tenant migration is queued from the admin endpoint.
    let (status, body) = send(
        &router,
        "POST",
        "/auth/register",
        None,
        Some(serde_json::json!({
            "tenant_name": "jobsco",
            "email": "boss@jobsco.test",
            "password": "hunter2hunter2",
            "first_name": "Jo",
            "last_name": "Bs",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "register failed: {body}");
    let (status, body) = send(
        &router,
        "POST",
        "/auth/login",
        None,
        Some(serde_json::json!({ "login": "boss@jobsco.test", "password": "hunter2hunter2" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "login failed: {body}");
    let boss = body["access_token"].as_str().unwrap().to_string();

    let (status, body) = send(&router, "POST", "/auth/tenant/migrate", Some(&boss), None).await;
    assert_eq!(status, StatusCode::OK, "migrate failed: {body}");
    assert_eq!(body["status"], "queued");
    assert!(body["task_id"].is_string(), "got: {body}");

    // The queue action lands in the audit trail.
    let (status, body) = send(
        &router,
        "GET",
        "/audit/logs?action=event",
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.as_array()
            .unwrap()
            .iter()
            .any(|r| r["message"] == "boss@jobsco.test queued a tenant database migration"),
        "queueing must be audited: {body}"
    );

    // -- Non-admins cannot queue migrations.
    let (status, _) = send(
        &router,
        "POST",
        "/auth/users",
        Some(&boss),
        Some(serde_json::json!({
            "user_name": "plain",
            "email": "plain@jobsco.test",
            "password": "plainpass123",
            "first_name": "Pla",
            "last_name": "In",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = send(
        &router,
        "POST",
        "/auth/login",
        None,
        Some(serde_json::json!({ "login": "plain", "password": "plainpass123" })),
    )
    .await;
    let plain = body["access_token"].as_str().unwrap().to_string();
    let (status, _) = send(&router, "POST", "/auth/tenant/migrate", Some(&plain), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
