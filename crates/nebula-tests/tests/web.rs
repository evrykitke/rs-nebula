//! Proof of concept: the kernel composes modules into a working app and
//! errors surface as RFC 9457 problem+json.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use nebula::{Config, Error, Kernel, Module, ModuleContext};
use tower::ServiceExt;

struct PingModule;

impl Module for PingModule {
    fn name(&self) -> &'static str {
        "ping"
    }

    fn configure(&self, ctx: &mut ModuleContext) {
        ctx.add_routes(
            Router::new()
                .route("/ping", get(|| async { "pong" }))
                .route(
                    "/missing",
                    get(|| async { Err::<(), _>(Error::NotFound("widget".into())) }),
                )
                .route(
                    "/boom",
                    get(|| async { Err::<(), _>(Error::internal("db exploded")) }),
                ),
        );
    }
}

fn app() -> Router {
    Kernel::builder()
        .with_config(Config::default())
        .add_module(PingModule)
        .build()
        .expect("kernel must build")
        .router()
}

async fn get_response(path: &str) -> (StatusCode, Option<String>, serde_json::Value) {
    let response = app()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, content_type, json)
}

#[tokio::test]
async fn module_routes_are_served() {
    let response = app()
        .oneshot(Request::get("/ping").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn health_endpoint_is_always_available() {
    let (status, _, body) = get_response("/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "healthy");
}

#[tokio::test]
async fn openapi_document_is_served() {
    let (status, _, body) = get_response("/api-docs/openapi.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["info"]["title"], "Nebula API");
    assert!(body["paths"]["/health"].is_object());
}

#[tokio::test]
async fn domain_errors_map_to_problem_details() {
    let (status, content_type, body) = get_response("/missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(content_type.as_deref(), Some("application/problem+json"));
    assert_eq!(body["status"], 404);
    assert_eq!(body["detail"], "widget was not found");
}

#[tokio::test]
async fn internal_error_details_are_not_leaked() {
    let (status, content_type, body) = get_response("/boom").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(content_type.as_deref(), Some("application/problem+json"));
    assert!(body.get("detail").is_none(), "5xx must not expose internals");
}

#[tokio::test]
async fn unknown_routes_get_problem_details_too() {
    let (status, content_type, body) = get_response("/definitely-not-a-route").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(content_type.as_deref(), Some("application/problem+json"));
    assert_eq!(body["status"], 404);
}
