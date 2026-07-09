//! Per-request tenant resolution.
//!
//! Reads the tenant header (`multitenancy.header`, default `X-Tenant`).
//! No header means host context: no tenant, main database. A named
//! tenant is looked up in the directory — unknown is a 404, inactive a
//! 403 — and its connection replaces the main one in request extensions,
//! where [`TenantDb`] and [`CurrentTenant`] pick them up.

use super::{TenantManager, TenantRef};
use crate::error::{Error, ProblemDetails};
use axum::extract::{FromRequestParts, Request, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sea_orm::DatabaseConnection;
use std::sync::Arc;

pub async fn resolve_tenant(
    State(manager): State<Arc<TenantManager>>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(value) = req.headers().get(manager.header_name()) else {
        return next.run(req).await;
    };

    let Ok(name) = value.to_str() else {
        return Error::Validation("tenant header is not valid UTF-8".into()).into_response();
    };

    let tenant = match manager.find_by_name(name).await {
        Ok(Some(tenant)) => tenant,
        Ok(None) => return Error::NotFound(format!("tenant {name:?}")).into_response(),
        Err(e) => return e.into_response(),
    };
    if !tenant.is_active {
        return Error::Forbidden.into_response();
    }

    let db = match manager.connection_for(&tenant).await {
        Ok(db) => db,
        Err(e) => return e.into_response(),
    };

    req.extensions_mut().insert(TenantRef {
        id: tenant.id,
        name: tenant.name,
    });
    req.extensions_mut().insert(db);
    next.run(req).await
}

/// Extractor: the tenant of the current request, `None` in host context.
pub struct CurrentTenant(pub Option<TenantRef>);

impl<S: Send + Sync> FromRequestParts<S> for CurrentTenant {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(CurrentTenant(parts.extensions.get::<TenantRef>().cloned()))
    }
}

/// Extractor: the database for the current request — the tenant's own
/// pool when one is resolved, otherwise the main database.
pub struct TenantDb(pub DatabaseConnection);

impl<S: Send + Sync> FromRequestParts<S> for TenantDb {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<DatabaseConnection>()
            .cloned()
            .map(TenantDb)
            .ok_or_else(|| {
                ProblemDetails::from_status(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Some("no database is configured".into()),
                )
                .into_response()
            })
    }
}
