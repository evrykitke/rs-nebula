//! Web host plumbing: health endpoint, OpenAPI document + Swagger UI,
//! and the resilience layers every application gets for free —
//! request timeout, panic containment, request ids and tracing.

mod health;

use crate::config::Config;
use crate::error::ProblemDetails;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Router;
use std::time::Duration;
use tower::ServiceBuilder;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

/// Base OpenAPI document; module contributions are merged into it.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Nebula API",
        description = "API served by the Nebula framework",
        version = env!("CARGO_PKG_VERSION")
    ),
    paths(health::health, health::ready)
)]
struct ApiDoc;

/// Wrap the module-composed router with framework routes and layers.
/// Applied once by the kernel after all modules have configured.
///
/// Layer order matters for tenancy: the `Extension` with the main
/// database is outer, so the tenant middleware (inner, runs after it)
/// can replace the connection for tenant requests.
pub(crate) fn finalize(
    router: Router,
    config: &Config,
    database: Option<sea_orm::DatabaseConnection>,
    tenants: Option<std::sync::Arc<crate::tenancy::TenantManager>>,
) -> Router {
    let api = ApiDoc::openapi();

    let mut router = router
        .merge(health::routes(config, database.clone()))
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", api))
        .fallback(not_found);

    if let Some(manager) = tenants {
        router = router.layer(axum::middleware::from_fn_with_state(
            manager,
            crate::tenancy::middleware::resolve_tenant,
        ));
    }
    if let Some(db) = database {
        router = router.layer(axum::Extension(db));
    }

    router.layer(
        ServiceBuilder::new()
            .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
            .layer(TraceLayer::new_for_http())
            .layer(CatchPanicLayer::custom(handle_panic))
            .layer(TimeoutLayer::with_status_code(
                StatusCode::REQUEST_TIMEOUT,
                Duration::from_secs(config.server.request_timeout_secs),
            ))
            .layer(PropagateRequestIdLayer::x_request_id()),
    )
}

/// Unknown routes answer with problem+json, like every other error.
async fn not_found() -> Response {
    ProblemDetails::from_status(
        StatusCode::NOT_FOUND,
        Some("the requested resource does not exist".into()),
    )
    .into_response()
}

/// A panicking handler must never tear down the host or leak the panic
/// message to the client; it becomes a plain 500.
fn handle_panic(err: Box<dyn std::any::Any + Send + 'static>) -> Response {
    let detail = err
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| err.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into());
    tracing::error!(panic = %detail, "handler panicked");
    ProblemDetails::from_status(StatusCode::INTERNAL_SERVER_ERROR, None).into_response()
}
