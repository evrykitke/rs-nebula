//! Web host plumbing: health endpoint, OpenAPI document + Swagger UI,
//! and the resilience layers every application gets for free —
//! request timeout, panic containment, request ids and tracing.

mod health;
mod trace;

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
    events: crate::events::Events,
    storage: crate::storage::Storage,
    cache: crate::cache::Cache,
    numbering: crate::numbering::Numbering,
    reporting: crate::reporting::Reporting,
    api_docs: Vec<utoipa::openapi::OpenApi>,
) -> Router {
    let mut api = ApiDoc::openapi();
    for doc in api_docs {
        api.merge(doc);
    }

    let mut router = router
        .merge(health::routes(config, database.clone()))
        .merge(crate::reporting::routes())
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", api))
        // Public files: uploads under {root}/{tenant-slug}/{id}/{resource}.
        // Served with nosniff + a locked-down CSP so a stored file can
        // never execute as a document, even if it slipped past upload
        // validation (defense in depth — see storage::guard).
        .nest_service("/public", public_files(&config.files.root))
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
    router = router.layer(axum::Extension(events));
    router = router.layer(axum::Extension(storage));
    router = router.layer(axum::Extension(cache));
    router = router.layer(axum::Extension(numbering));
    router = router.layer(axum::Extension(reporting));

    if let Some(cors) = cors_layer(config) {
        router = router.layer(cors);
    }

    router.layer(
        ServiceBuilder::new()
            .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
            .layer(
                TraceLayer::new_for_http()
                    .make_span_with(|request: &axum::http::Request<_>| trace::make_span(request))
                    .on_request(|request: &axum::http::Request<_>, span: &tracing::Span| {
                        trace::on_request(request, span)
                    })
                    .on_response(
                        |response: &Response<_>, latency: Duration, span: &tracing::Span| {
                            trace::on_response(response, latency, span)
                        },
                    )
                    .on_failure(trace::on_failure),
            )
            .layer(CatchPanicLayer::custom(handle_panic))
            .layer(TimeoutLayer::with_status_code(
                StatusCode::REQUEST_TIMEOUT,
                Duration::from_secs(config.server.request_timeout_secs),
            ))
            .layer(PropagateRequestIdLayer::x_request_id()),
    )
}

/// The `/public` static-file service with its hardening headers stamped
/// on every response.
fn public_files(
    root: &str,
) -> tower_http::set_header::SetResponseHeader<
    tower_http::set_header::SetResponseHeader<
        tower_http::services::ServeDir,
        axum::http::HeaderValue,
    >,
    axum::http::HeaderValue,
> {
    use axum::http::{HeaderName, HeaderValue};
    use tower_http::set_header::SetResponseHeaderLayer;

    let header = |(name, value): (&'static str, &'static str)| {
        SetResponseHeaderLayer::overriding(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        )
    };
    let [nosniff, csp] = crate::storage::guard::response_headers();
    ServiceBuilder::new()
        .layer(header(nosniff))
        .layer(header(csp))
        .service(tower_http::services::ServeDir::new(root))
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
