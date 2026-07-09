//! Unified framework error type with RFC 9457 problem-details responses.
//!
//! Handlers return [`Error`]; the web layer converts it into an
//! `application/problem+json` response. Internal errors (5xx) are logged
//! with full detail but never leak their message to the client.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::config::ConfigError;
use crate::logging::LoggingError;

/// Framework-wide result alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The framework error type. Domain and application code map their
/// failures into one of these variants; the web layer decides how each
/// variant is presented to clients.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Input failed validation (HTTP 400).
    #[error("{0}")]
    Validation(String),

    /// Caller is not authenticated (HTTP 401).
    #[error("authentication required")]
    Unauthorized,

    /// Caller lacks permission (HTTP 403).
    #[error("permission denied")]
    Forbidden,

    /// A referenced resource does not exist (HTTP 404).
    #[error("{0} was not found")]
    NotFound(String),

    /// State conflict, e.g. concurrent update or duplicate key (HTTP 409).
    #[error("{0}")]
    Conflict(String),

    /// Database failure (HTTP 500). Like all internal errors, the
    /// underlying cause is logged, never sent to the client.
    #[error("database error: {0}")]
    Database(#[from] sea_orm::DbErr),

    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    Logging(#[from] LoggingError),

    /// Unexpected failure (HTTP 500). The message is logged, not exposed.
    #[error("{0}")]
    Internal(String),
}

impl Error {
    /// Convenience constructor for unexpected failures.
    pub fn internal(err: impl std::fmt::Display) -> Self {
        Self::Internal(err.to_string())
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Error::Validation(_) => StatusCode::BAD_REQUEST,
            Error::Unauthorized => StatusCode::UNAUTHORIZED,
            Error::Forbidden => StatusCode::FORBIDDEN,
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Conflict(_) => StatusCode::CONFLICT,
            Error::Database(_) | Error::Config(_) | Error::Logging(_) | Error::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }
}

/// RFC 9457 problem details payload.
#[derive(Debug, Clone, Serialize)]
pub struct ProblemDetails {
    /// URI reference identifying the problem type.
    #[serde(rename = "type")]
    pub type_uri: String,
    pub title: String,
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ProblemDetails {
    pub fn from_status(status: StatusCode, detail: Option<String>) -> Self {
        Self {
            type_uri: format!(
                "https://httpstatuses.io/{}",
                status.as_u16()
            ),
            title: status
                .canonical_reason()
                .unwrap_or("Unknown Error")
                .to_string(),
            status: status.as_u16(),
            detail,
        }
    }

    pub fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = serde_json::to_string(&self)
            .unwrap_or_else(|_| r#"{"title":"Internal Server Error","status":500}"#.into());
        (
            status,
            [(header::CONTENT_TYPE, "application/problem+json")],
            body,
        )
            .into_response()
    }
}

impl From<&Error> for ProblemDetails {
    fn from(err: &Error) -> Self {
        let status = err.status();
        // Never leak internals: 5xx responses get a generic detail.
        let detail = if status.is_server_error() {
            None
        } else {
            Some(err.to_string())
        };
        ProblemDetails::from_status(status, detail)
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        if self.status().is_server_error() {
            tracing::error!(error = %self, "request failed");
        }
        ProblemDetails::from(&self).into_response()
    }
}
