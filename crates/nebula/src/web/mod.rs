//! Web host plumbing: health endpoint, OpenAPI document + Swagger UI,
//! and the resilience layers every application gets for free —
//! request timeout, panic containment, request ids and tracing.

mod health;

use crate::config::Config;
use crate::error::ProblemDetails;
use axum::Router;
use axum::http::StatusCode;
use axum::response::Response;
use std::time::Duration;
use tower::ServiceBuilder;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
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
    permissions: std::sync::Arc<crate::auth::permission::Registry>,
    jobs: Option<crate::jobs::Jobs>,
    api_docs: Vec<utoipa::openapi::OpenApi>,
) -> Router {
    let mut api = ApiDoc::openapi();
    for doc in api_docs {
        api.merge(doc);
    }

    let mut router = router
        .merge(health::routes(config, database.clone()))
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", api))
        // Public files (tenant logos, uploads under {root}/{tenant-id}/).
        .nest_service(
            "/public",
            tower_http::services::ServeDir::new(&config.files.root),
        )
        .fallback(not_found);

    // Innermost, so it sees the tenant-swapped database connection.
    if config.audit.enabled {
        router = router.layer(axum::middleware::from_fn_with_state(
            config.audit.clone(),
            crate::audit::middleware::record,
        ));
    }
    if let Some(manager) = tenants {
        router = router.layer(axum::middleware::from_fn_with_state(
            manager,
            crate::tenancy::middleware::resolve_tenant,
        ));
    }
    if let Some(db) = database {
        router = router.layer(axum::Extension(db));
    }
    router = router.layer(axum::Extension(config.auth.clone()));
    router = router.layer(axum::Extension(permissions));
    if let Some(jobs) = jobs {
        router = router.layer(axum::Extension(jobs));
    }

    if let Some(cors) = cors_layer(config) {
        router = router.layer(cors);
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

/// Cross-origin access for browser frontends. Only the origins listed in
/// `server.cors_origins` are admitted; an empty list means no CORS layer at
/// all. Misconfigured origins fail the boot rather than silently allowing
/// nothing.
fn cors_layer(config: &Config) -> Option<CorsLayer> {
    let origins = &config.server.cors_origins;
    if origins.is_empty() {
        return None;
    }
    let origins: Vec<_> = origins
        .iter()
        .map(|o| {
            o.parse()
                .unwrap_or_else(|e| panic!("invalid server.cors_origins entry {o:?}: {e}"))
        })
        .collect();
    Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_methods(tower_http::cors::Any)
            .allow_headers(tower_http::cors::Any)
            .expose_headers(tower_http::cors::Any),
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
