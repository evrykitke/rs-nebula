//! Request tracing: one span per HTTP request carrying the method, path
//! and request id, so every log line emitted while handling it — module
//! code and SQL alike — is stamped with the request it belongs to.
//!
//! At the default `info` level each request produces one completion line
//! (status + latency). Client errors surface at `warn` and server errors
//! at `error`, so failures are visible without turning anything up.
//! `logging.http: debug` adds a request-start line; `logging.http: off`
//! silences per-request logs entirely.

use axum::http::{Request, Response};
use std::time::Duration;
use tower_http::classify::ServerErrorsFailureClass;
use tower_http::request_id::RequestId;
use tracing::Span;

/// The span every request runs in. The id comes from `SetRequestIdLayer`,
/// which sits outside this layer.
pub(crate) fn make_span<B>(request: &Request<B>) -> Span {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .and_then(|id| id.header_value().to_str().ok())
        .unwrap_or("-")
        .to_owned();
    tracing::info_span!(
        "request",
        method = %request.method(),
        path = %request.uri().path(),
        request_id = %request_id,
    )
}

pub(crate) fn on_request<B>(_request: &Request<B>, _span: &Span) {
    tracing::debug!("request started");
}

/// One line per finished request; the level carries the outcome.
pub(crate) fn on_response<B>(response: &Response<B>, latency: Duration, _span: &Span) {
    let status = response.status().as_u16();
    let latency_ms = latency.as_millis() as u64;
    if response.status().is_server_error() {
        tracing::error!(status, latency_ms, "request failed");
    } else if response.status().is_client_error() {
        tracing::warn!(status, latency_ms, "request rejected");
    } else {
        tracing::info!(status, latency_ms, "request completed");
    }
}

/// 5xx statuses are already reported by `on_response`; this only covers
/// failures that never produced a response (middleware errors).
pub(crate) fn on_failure(class: ServerErrorsFailureClass, latency: Duration, _span: &Span) {
    if let ServerErrorsFailureClass::Error(error) = class {
        tracing::error!(error = %error, latency_ms = latency.as_millis() as u64, "request errored");
    }
}
