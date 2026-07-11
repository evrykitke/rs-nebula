//! Audit trail endpoints, guarded by `Pages.Administration.AuditLogs.View`:
//!
//! - `GET /audit/logs` — the trail, newest first, with paging and
//!   filters (`action`, `entity_type`, `user_id`)
//! - `GET /audit/logs/{id}` — one row with its full snapshots
//! - `GET /audit/logs/{id}/diff` — the what-changed view: only the
//!   fields that differ between the before and after snapshots
//! - `GET`/`PUT /audit/retention` — the tenant's retention override
//!   (days), capped at `audit.retention_max_days`
//!
//! Rows are tenant-scoped: a tenant only ever sees its own trail.

use crate::audit::diff::{FieldChange, diff};
use crate::audit::log;
use crate::auth::Authz;
use crate::auth::permission;
use crate::config::AuditConfig;
use crate::error::{Error, Result};
use crate::tenancy::TenantManager;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Extension, Json, Router};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone)]
struct AuditState {
    config: AuditConfig,
    tenants: Option<Arc<TenantManager>>,
}

pub(super) fn routes(config: AuditConfig, tenants: Option<Arc<TenantManager>>) -> Router {
    Router::new()
        .route("/audit/logs", get(list_logs))
        .route("/audit/logs/{id}", get(get_log))
        .route("/audit/logs/{id}/diff", get(get_log_diff))
        .route("/audit/retention", get(get_retention).put(set_retention))
        .with_state(AuditState { config, tenants })
}

pub(super) fn api() -> utoipa::openapi::OpenApi {
    crate::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

/// The audit endpoints' OpenAPI contribution — the source client
/// generators (NSwag) build the `audit` service proxy from.
#[derive(utoipa::OpenApi)]
#[openapi(paths(list_logs, get_log, get_log_diff, get_retention, set_retention))]
struct ApiDoc;

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LogQuery {
    pub limit: Option<u64>,
    pub offset: Option<u64>,
    pub action: Option<String>,
    pub entity_type: Option<String>,
    pub user_id: Option<uuid::Uuid>,
}

#[utoipa::path(get, path = "/audit/logs", tag = "audit",
    params(LogQuery),
    responses((status = 200, body = Vec<log::Model>)))]
async fn list_logs(
    authz: Authz,
    Extension(db): Extension<DatabaseConnection>,
    Query(query): Query<LogQuery>,
) -> Result<Json<Vec<log::Model>>> {
    authz.require(permission::names::AUDIT_LOGS_VIEW).await?;
    let mut select = log::Entity::find()
        .filter(tenant_filter(authz.user.tenant_id))
        .order_by_desc(log::Column::Id)
        .limit(query.limit.unwrap_or(50).min(500))
        .offset(query.offset.unwrap_or(0));
    if let Some(action) = &query.action {
        select = select.filter(log::Column::Action.eq(action));
    }
    if let Some(entity_type) = &query.entity_type {
        select = select.filter(log::Column::EntityType.eq(entity_type));
    }
    if let Some(user_id) = query.user_id {
        select = select.filter(log::Column::UserId.eq(user_id));
    }
    Ok(Json(select.all(&db).await?))
}

#[utoipa::path(get, path = "/audit/logs/{id}", tag = "audit",
    params(("id" = i64, Path, description = "Audit log entry id")),
    responses((status = 200, body = log::Model)))]
async fn get_log(
    authz: Authz,
    Extension(db): Extension<DatabaseConnection>,
    Path(id): Path<i64>,
) -> Result<Json<log::Model>> {
    authz.require(permission::names::AUDIT_LOGS_VIEW).await?;
    tenant_log(&db, authz.user.tenant_id, id).await.map(Json)
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct LogDiff {
    pub id: i64,
    pub action: String,
    pub entity_type: Option<String>,
    pub entity_id: Option<String>,
    /// Only the fields that actually changed.
    pub changes: Vec<FieldChange>,
}

#[utoipa::path(get, path = "/audit/logs/{id}/diff", tag = "audit",
    params(("id" = i64, Path, description = "Audit log entry id")),
    responses((status = 200, body = LogDiff)))]
async fn get_log_diff(
    authz: Authz,
    Extension(db): Extension<DatabaseConnection>,
    Path(id): Path<i64>,
) -> Result<Json<LogDiff>> {
    authz.require(permission::names::AUDIT_LOGS_VIEW).await?;
    let row = tenant_log(&db, authz.user.tenant_id, id).await?;
    Ok(Json(LogDiff {
        id: row.id,
        action: row.action,
        entity_type: row.entity_type,
        entity_id: row.entity_id,
        changes: diff(row.old_values.as_ref(), row.new_values.as_ref()),
    }))
}

async fn tenant_log(
    db: &DatabaseConnection,
    tenant_id: Option<uuid::Uuid>,
    id: i64,
) -> Result<log::Model> {
    log::Entity::find_by_id(id)
        .filter(tenant_filter(tenant_id))
        .one(db)
        .await?
        .ok_or_else(|| Error::NotFound("audit log entry".into()))
}

fn tenant_filter(tenant_id: Option<uuid::Uuid>) -> sea_orm::sea_query::SimpleExpr {
    match tenant_id {
        Some(id) => log::Column::TenantId.eq(id),
        None => log::Column::TenantId.is_null(),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct RetentionRequest {
    /// Days to keep audit rows; null reverts to the system default.
    pub retention_days: Option<i32>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct RetentionResponse {
    /// The tenant's override, when one is set.
    pub retention_days: Option<i32>,
    pub system_default_days: u32,
    pub max_days: u32,
    /// What the pruner actually applies.
    pub effective_days: i64,
}

impl AuditState {
    async fn tenant_retention(&self, authz: &Authz) -> Result<(Arc<TenantManager>, uuid::Uuid)> {
        let manager = self.tenants.clone().ok_or_else(|| {
            Error::Validation("multitenancy is not enabled on this deployment".into())
        })?;
        let tenant_id = authz
            .user
            .tenant_id
            .ok_or_else(|| Error::Validation("a tenant context is required".into()))?;
        Ok((manager, tenant_id))
    }

    fn response(&self, retention_days: Option<i32>) -> RetentionResponse {
        RetentionResponse {
            retention_days,
            system_default_days: self.config.retention_days,
            max_days: self.config.retention_max_days,
            effective_days: crate::audit::pruner::effective_retention(
                &self.config,
                retention_days,
            ),
        }
    }
}

#[utoipa::path(get, path = "/audit/retention", tag = "audit",
    responses((status = 200, body = RetentionResponse)))]
async fn get_retention(
    State(state): State<AuditState>,
    authz: Authz,
) -> Result<Json<RetentionResponse>> {
    authz.require(permission::names::TENANT_SETTINGS).await?;
    let (manager, tenant_id) = state.tenant_retention(&authz).await?;
    let tenant = manager
        .find_by_id(tenant_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("tenant {tenant_id}")))?;
    Ok(Json(state.response(tenant.audit_retention_days)))
}

#[utoipa::path(put, path = "/audit/retention", tag = "audit",
    request_body = RetentionRequest,
    responses((status = 200, body = RetentionResponse)))]
async fn set_retention(
    State(state): State<AuditState>,
    authz: Authz,
    Json(req): Json<RetentionRequest>,
) -> Result<Json<RetentionResponse>> {
    authz.require(permission::names::TENANT_SETTINGS).await?;
    let (manager, tenant_id) = state.tenant_retention(&authz).await?;
    if let Some(days) = req.retention_days {
        if days < 1 || days as u32 > state.config.retention_max_days {
            return Err(Error::Validation(format!(
                "retention_days must be between 1 and {}",
                state.config.retention_max_days
            )));
        }
    }
    let tenant = manager
        .set_audit_retention(tenant_id, req.retention_days)
        .await?;
    Ok(Json(state.response(tenant.audit_retention_days)))
}
