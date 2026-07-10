//! Writing audit rows. `RequestInfo` captures the who/where of a request
//! (user from the bearer token, tenant, ip, user agent, request id);
//! `Recorder` writes rows carrying that context; the `Audit` extractor
//! hands handlers a ready recorder for entity snapshots:
//!
//! ```ignore
//! async fn update_thing(audit: Audit, ...) -> Result<...> {
//!     let before = Snapshot::from(&thing);
//!     // ... mutate ...
//!     audit.updated("thing", thing.id, &before, &Snapshot::from(&thing)).await;
//! }
//! ```
//!
//! Snapshot what you would show a client (e.g. a user's `Profile`, never
//! the row with its password hash). Audit writes are failure-contained:
//! a broken audit store logs an error but never fails the mutation it
//! describes.

use super::log;
use crate::auth::jwt::{self, TokenPurpose};
use crate::config::AuthConfig;
use crate::error::Error;
use crate::tenancy::TenantRef;
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{Extensions, HeaderMap, Method, Uri, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};
use serde::Serialize;
use std::net::SocketAddr;

/// The request context stamped onto every audit row.
#[derive(Debug, Clone, Default)]
pub struct RequestInfo {
    pub tenant_id: Option<i32>,
    pub user_id: Option<i32>,
    pub request_id: Option<String>,
    pub method: String,
    pub path: String,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
}

impl RequestInfo {
    pub(crate) fn collect(
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        extensions: &Extensions,
    ) -> Self {
        let claims = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .zip(extensions.get::<AuthConfig>())
            .and_then(|(token, config)| jwt::verify(config, token).ok())
            .filter(|claims| claims.purpose == TokenPurpose::Access);
        let tenant_id = extensions
            .get::<TenantRef>()
            .map(|t| t.id)
            .or_else(|| claims.as_ref().and_then(|c| c.tenant_id));

        // Trust the proxy header when present, fall back to the socket.
        let ip_address = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(|v| v.trim().to_string())
            .or_else(|| {
                extensions
                    .get::<ConnectInfo<SocketAddr>>()
                    .map(|ConnectInfo(addr)| addr.ip().to_string())
            });

        Self {
            tenant_id,
            user_id: claims.map(|c| c.sub),
            request_id: header_string(headers, "x-request-id"),
            method: method.to_string(),
            path: uri.path().to_string(),
            ip_address,
            user_agent: header_string(headers, header::USER_AGENT.as_str()),
        }
    }
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

pub struct Recorder {
    db: DatabaseConnection,
    info: RequestInfo,
}

impl Recorder {
    pub fn new(db: DatabaseConnection, info: RequestInfo) -> Self {
        Self { db, info }
    }

    /// Stamp rows with a tenant the request context does not carry —
    /// e.g. registration, where the tenant is created mid-request.
    pub fn with_tenant(mut self, tenant_id: Option<i32>) -> Self {
        self.info.tenant_id = tenant_id;
        self
    }

    /// Stamp rows with a user the request context does not carry —
    /// e.g. login, where the user is only known after the password check.
    pub fn with_user(mut self, user_id: Option<i32>) -> Self {
        self.info.user_id = user_id;
        self
    }

    /// A plain human-readable event without entity snapshots —
    /// "boss logged in", "failed login attempt for x". The row still
    /// carries the full request context (ip, user agent, request id).
    pub async fn event(&self, message: impl Into<String>) {
        let mut row = self.row(log::ACTION_EVENT);
        row.message = Set(Some(message.into()));
        self.insert(row).await;
    }

    pub async fn created(
        &self,
        entity_type: &str,
        entity_id: impl ToString,
        after: &impl Serialize,
    ) {
        self.write(
            log::ACTION_CREATE,
            entity_type,
            entity_id,
            None,
            snapshot(after),
        )
        .await;
    }

    pub async fn updated(
        &self,
        entity_type: &str,
        entity_id: impl ToString,
        before: &impl Serialize,
        after: &impl Serialize,
    ) {
        self.write(
            log::ACTION_UPDATE,
            entity_type,
            entity_id,
            snapshot(before),
            snapshot(after),
        )
        .await;
    }

    pub async fn deleted(
        &self,
        entity_type: &str,
        entity_id: impl ToString,
        before: &impl Serialize,
    ) {
        self.write(
            log::ACTION_DELETE,
            entity_type,
            entity_id,
            snapshot(before),
            None,
        )
        .await;
    }

    /// Used by the middleware for `request` rows.
    pub(crate) async fn request_completed(&self, status_code: u16, duration_ms: i64) {
        let row = self.row(log::ACTION_REQUEST);
        let mut row = row;
        row.status_code = Set(Some(status_code as i32));
        row.duration_ms = Set(Some(duration_ms));
        self.insert(row).await;
    }

    async fn write(
        &self,
        action: &str,
        entity_type: &str,
        entity_id: impl ToString,
        old_values: Option<serde_json::Value>,
        new_values: Option<serde_json::Value>,
    ) {
        let mut row = self.row(action);
        row.entity_type = Set(Some(entity_type.to_string()));
        row.entity_id = Set(Some(entity_id.to_string()));
        row.old_values = Set(old_values);
        row.new_values = Set(new_values);
        self.insert(row).await;
    }

    fn row(&self, action: &str) -> log::ActiveModel {
        log::ActiveModel {
            tenant_id: Set(self.info.tenant_id),
            user_id: Set(self.info.user_id),
            request_id: Set(self.info.request_id.clone()),
            method: Set(self.info.method.clone()),
            path: Set(self.info.path.clone()),
            ip_address: Set(self.info.ip_address.clone()),
            user_agent: Set(self.info.user_agent.clone()),
            action: Set(action.to_string()),
            created_at: Set(Utc::now()),
            ..Default::default()
        }
    }

    async fn insert(&self, row: log::ActiveModel) {
        if let Err(e) = row.insert(&self.db).await {
            tracing::error!(error = %e, "failed to write audit log row");
        }
    }
}

fn snapshot(value: &impl Serialize) -> Option<serde_json::Value> {
    match serde_json::to_value(value) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::error!(error = %e, "audit snapshot failed to serialize");
            None
        }
    }
}

/// Extractor: a [`Recorder`] bound to the current request's database and
/// context, for handlers that record entity snapshots.
pub struct Audit(pub Recorder);

impl<S: Send + Sync> FromRequestParts<S> for Audit {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let db = parts
            .extensions
            .get::<DatabaseConnection>()
            .cloned()
            .ok_or_else(|| Error::internal("audit recorder requires a database").into_response())?;
        let info =
            RequestInfo::collect(&parts.method, &parts.uri, &parts.headers, &parts.extensions);
        Ok(Audit(Recorder::new(db, info)))
    }
}
