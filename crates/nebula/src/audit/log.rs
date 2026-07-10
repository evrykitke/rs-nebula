//! The audit trail entity. One table carries both kinds of rows:
//! `request` rows written by the middleware for every mutating HTTP
//! request, and `create`/`update`/`delete` rows written by handlers with
//! before/after entity snapshots (jsonb). Every row carries the full
//! request context — who, from which address, with which user agent.

use sea_orm::entity::prelude::*;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, utoipa::ToSchema)]
#[sea_orm(table_name = "audit_logs")]
#[schema(as = AuditLog)]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub tenant_id: Option<i32>,
    pub user_id: Option<i32>,
    /// The `x-request-id` of the request, linking audit rows to traces.
    pub request_id: Option<String>,
    pub method: String,
    pub path: String,
    pub status_code: Option<i32>,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    pub duration_ms: Option<i64>,
    /// `request`, `create`, `update`, `delete` or `event`.
    pub action: String,
    pub entity_type: Option<String>,
    pub entity_id: Option<String>,
    /// Human-readable line for `event` rows ("boss logged in").
    pub message: Option<String>,
    /// Snapshot before the change (`update`/`delete` rows).
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub old_values: Option<Json>,
    /// Snapshot after the change (`create`/`update` rows).
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub new_values: Option<Json>,
    /// The sea_orm alias hides the chrono type from utoipa's derive, so
    /// the schema type is spelled out.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

pub const ACTION_REQUEST: &str = "request";
pub const ACTION_CREATE: &str = "create";
pub const ACTION_UPDATE: &str = "update";
pub const ACTION_DELETE: &str = "delete";
pub const ACTION_EVENT: &str = "event";
