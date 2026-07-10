//! Retention pruning: the audit trail grows fast, so a background job
//! deletes rows past their retention window. The system default is
//! `audit.retention_days` (30); each tenant may override it via
//! `PUT /audit/retention`, capped at `audit.retention_max_days` (six
//! months). The job runs every `audit.prune_interval_secs` while the
//! application serves; [`prune_once`] is public so tests and operators
//! can trigger a pass directly.

use super::log;
use crate::config::AuditConfig;
use crate::error::Result;
use crate::tenancy::TenantManager;
use chrono::{Duration, Utc};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use std::sync::Arc;

/// The effective retention for a tenant override, clamped to the cap.
pub fn effective_retention(config: &AuditConfig, tenant_override: Option<i32>) -> i64 {
    let days = match tenant_override {
        Some(days) if days > 0 => (days as u32).min(config.retention_max_days),
        _ => config.retention_days,
    };
    days.max(1) as i64
}

/// One pruning pass over the host trail and every active tenant's trail
/// (in the tenant's own database when it has one). Returns rows deleted.
pub async fn prune_once(
    db: &DatabaseConnection,
    tenants: Option<&Arc<TenantManager>>,
    config: &AuditConfig,
) -> Result<u64> {
    let mut deleted = 0;

    let host_cutoff = Utc::now() - Duration::days(effective_retention(config, None));
    deleted += log::Entity::delete_many()
        .filter(log::Column::TenantId.is_null())
        .filter(log::Column::CreatedAt.lt(host_cutoff))
        .exec(db)
        .await?
        .rows_affected;

    if let Some(manager) = tenants {
        for tenant in manager.find_all().await? {
            if !tenant.is_active {
                continue;
            }
            let cutoff = Utc::now()
                - Duration::days(effective_retention(config, tenant.audit_retention_days));
            let tenant_db = manager.connection_for(&tenant).await?;
            deleted += log::Entity::delete_many()
                .filter(log::Column::TenantId.eq(tenant.id))
                .filter(log::Column::CreatedAt.lt(cutoff))
                .exec(&tenant_db)
                .await?
                .rows_affected;
        }
    }
    Ok(deleted)
}

/// Spawn the recurring job. Failures are logged and the job keeps going.
pub(crate) fn spawn(
    db: DatabaseConnection,
    tenants: Option<Arc<TenantManager>>,
    config: AuditConfig,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(
            config.prune_interval_secs.max(60),
        ));
        loop {
            interval.tick().await;
            match prune_once(&db, tenants.as_ref(), &config).await {
                Ok(0) => {}
                Ok(deleted) => tracing::info!(deleted, "pruned expired audit log rows"),
                Err(e) => tracing::error!(error = %e, "audit pruning pass failed"),
            }
        }
    });
}
