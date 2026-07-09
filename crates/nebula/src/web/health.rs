//! Liveness and readiness endpoints.
//!
//! `/health` answers as long as the process serves requests — no
//! dependencies, so it stays honest. `/health/ready` additionally checks
//! the subsystems a request would need (today: the database), returning
//! 503 problem+json when the application should not receive traffic yet.

use crate::config::Config;
use crate::db;
use crate::error::ProblemDetails;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use sea_orm::DatabaseConnection;
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
pub struct Health {
    /// Always `healthy` when the host is serving.
    pub status: &'static str,
    /// Active configuration environment.
    pub environment: String,
    /// Framework version.
    pub version: &'static str,
}

#[derive(Serialize, ToSchema)]
pub struct Readiness {
    pub status: &'static str,
    /// Per-dependency states: `up`, `down`, or `not_configured`.
    pub database: &'static str,
}

#[derive(Clone)]
pub(crate) struct HealthState {
    environment: String,
    database: Option<DatabaseConnection>,
}

#[utoipa::path(
    get,
    path = "/health",
    tag = "health",
    responses((status = 200, description = "Host is alive", body = Health))
)]
pub(crate) async fn health(State(state): State<HealthState>) -> Json<Health> {
    Json(Health {
        status: "healthy",
        environment: state.environment,
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[utoipa::path(
    get,
    path = "/health/ready",
    tag = "health",
    responses(
        (status = 200, description = "Application can serve traffic", body = Readiness),
        (status = 503, description = "A required dependency is down")
    )
)]
pub(crate) async fn ready(State(state): State<HealthState>) -> Response {
    let database = match &state.database {
        None => "not_configured",
        Some(db) => match db::ping(db).await {
            Ok(()) => "up",
            Err(e) => {
                tracing::error!(error = %e, "readiness check: database is down");
                "down"
            }
        },
    };

    if database == "down" {
        return ProblemDetails::from_status(
            StatusCode::SERVICE_UNAVAILABLE,
            Some("database is unreachable".into()),
        )
        .into_response();
    }

    Json(Readiness {
        status: "ready",
        database,
    })
    .into_response()
}

pub(crate) fn routes(config: &Config, database: Option<DatabaseConnection>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/health/ready", get(ready))
        .with_state(HealthState {
            environment: config.environment.clone(),
            database,
        })
}
