//! Liveness endpoint. Kept dependency-free so it stays honest: it answers
//! as long as the process serves requests. Readiness checks (database,
//! broker) will be a separate endpoint once those subsystems exist.

use crate::config::Config;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
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

#[derive(Clone)]
struct HealthState {
    environment: String,
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

pub(crate) fn routes(config: &Config) -> Router {
    Router::new().route("/health", get(health)).with_state(HealthState {
        environment: config.environment.clone(),
    })
}
