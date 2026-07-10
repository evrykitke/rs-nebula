//! Ready-made audit trail endpoints, guarded by their own permission:
//!
//! - `GET /audit/logs` — the trail, newest first, with paging and
//!   filters (`action`, `entity_type`, `user_id`)
//! - `GET /audit/logs/{id}` — one row with its full snapshots
//! - `GET /audit/logs/{id}/diff` — the what-changed view: only the
//!   fields that differ between the before and after snapshots
//!
//! Rows are tenant-scoped: a tenant only ever sees its own trail.

use super::diff::{FieldChange, diff};
use super::log;
use crate::auth::Authz;
use crate::auth::permission::PermissionDef;
use crate::error::{Error, Result};
use crate::module::{Module, ModuleContext};
use axum::extract::{Path, Query};
use axum::routing::get;
use axum::{Extension, Json, Router};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use serde::{Deserialize, Serialize};

/// Permission names the audit module defines.
pub mod names {
    pub const AUDIT_LOGS: &str = "Pages.Administration.AuditLogs";
    pub const AUDIT_LOGS_VIEW: &str = "Pages.Administration.AuditLogs.View";
}

pub struct AuditModule;

impl Module for AuditModule {
    fn name(&self) -> &'static str {
        "audit"
    }

    fn configure(&self, ctx: &mut ModuleContext) {
        ctx.add_permissions(PermissionDef::new(names::AUDIT_LOGS, "Audit logs").child(
            PermissionDef::new(names::AUDIT_LOGS_VIEW, "View audit logs"),
        ));
        ctx.add_routes(
            Router::new()
                .route("/audit/logs", get(list_logs))
                .route("/audit/logs/{id}", get(get_log))
                .route("/audit/logs/{id}/diff", get(get_log_diff)),
        );
    }
}

#[derive(Deserialize)]
pub struct LogQuery {
    pub limit: Option<u64>,
    pub offset: Option<u64>,
    pub action: Option<String>,
    pub entity_type: Option<String>,
    pub user_id: Option<i32>,
}

async fn list_logs(
    authz: Authz,
    Extension(db): Extension<DatabaseConnection>,
    Query(query): Query<LogQuery>,
) -> Result<Json<Vec<log::Model>>> {
    authz.require(names::AUDIT_LOGS_VIEW).await?;
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

async fn get_log(
    authz: Authz,
    Extension(db): Extension<DatabaseConnection>,
    Path(id): Path<i64>,
) -> Result<Json<log::Model>> {
    authz.require(names::AUDIT_LOGS_VIEW).await?;
    tenant_log(&db, authz.user.tenant_id, id).await.map(Json)
}

#[derive(Serialize)]
pub struct LogDiff {
    pub id: i64,
    pub action: String,
    pub entity_type: Option<String>,
    pub entity_id: Option<String>,
    /// Only the fields that actually changed.
    pub changes: Vec<FieldChange>,
}

async fn get_log_diff(
    authz: Authz,
    Extension(db): Extension<DatabaseConnection>,
    Path(id): Path<i64>,
) -> Result<Json<LogDiff>> {
    authz.require(names::AUDIT_LOGS_VIEW).await?;
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
    tenant_id: Option<i32>,
    id: i64,
) -> Result<log::Model> {
    log::Entity::find_by_id(id)
        .filter(tenant_filter(tenant_id))
        .one(db)
        .await?
        .ok_or_else(|| Error::NotFound("audit log entry".into()))
}

fn tenant_filter(tenant_id: Option<i32>) -> sea_orm::sea_query::SimpleExpr {
    match tenant_id {
        Some(id) => log::Column::TenantId.eq(id),
        None => log::Column::TenantId.is_null(),
    }
}
