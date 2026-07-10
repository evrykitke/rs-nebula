//! Request auditing: every mutating request (POST/PUT/PATCH/DELETE —
//! plus reads when `audit.include_reads` is on) gets a `request` row
//! with user, tenant, ip, user agent, request id, status and duration.
//! Bodies are never read or stored. Runs inside the tenant middleware,
//! so rows land in the same database as the data the request touched.

use super::recorder::{Recorder, RequestInfo};
use crate::config::AuditConfig;
use axum::extract::{Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;
use sea_orm::DatabaseConnection;
use std::time::Instant;

pub(crate) async fn record(
    State(config): State<AuditConfig>,
    request: Request,
    next: Next,
) -> Response {
    let audited = match *request.method() {
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE => true,
        Method::GET | Method::HEAD => config.include_reads,
        _ => false,
    };
    let recorder = if audited {
        request
            .extensions()
            .get::<DatabaseConnection>()
            .cloned()
            .map(|db| {
                let info = RequestInfo::collect(
                    request.method(),
                    request.uri(),
                    request.headers(),
                    request.extensions(),
                );
                Recorder::new(db, info)
            })
    } else {
        None
    };

    let start = Instant::now();
    let response = next.run(request).await;

    if let Some(recorder) = recorder {
        let duration_ms = start.elapsed().as_millis() as i64;
        recorder
            .request_completed(response.status().as_u16(), duration_ms)
            .await;
    }
    response
}
