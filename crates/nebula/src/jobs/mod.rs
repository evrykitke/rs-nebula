//! Background jobs on apalis with Redis-backed queues.
//!
//! Enable with `jobs.enabled` (workers connect through `redis.url`).
//! The kernel then runs an apalis `Monitor` alongside the web host:
//!
//! - handlers enqueue through the [`Jobs`] client (available as a
//!   request extension): `jobs.enqueue(QUEUE, MyJob { .. }).await?`
//! - modules contribute workers in `configure` via
//!   `ModuleContext::add_worker`, capturing whatever state they need
//!
//! Built-in jobs: the audit retention pruner runs as a cron worker, and
//! [`MigrateTenants`] rolls framework + application migrations across
//! tenant databases without a restart — enqueued for one tenant from
//! `POST /auth/tenant/migrate`, or for all tenants by the host.

use crate::error::{Error, Result};
use crate::kernel::Migrations;
use crate::tenancy::TenantManager;
use apalis::prelude::*;
use apalis_redis::{ConnectionManager, RedisStorage};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Queue name for [`MigrateTenants`] jobs.
pub const TENANT_MIGRATION_QUEUE: &str = "tenant-migrations";

/// Enqueues jobs onto named Redis-backed queues. Cheap to clone; shared
/// through request extensions when `jobs.enabled` is on.
#[derive(Clone)]
pub struct Jobs {
    conn: ConnectionManager,
}

impl std::fmt::Debug for Jobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jobs").finish_non_exhaustive()
    }
}

impl Jobs {
    pub(crate) fn new(conn: ConnectionManager) -> Self {
        Self { conn }
    }

    /// The typed storage behind a queue. Enqueuers and workers must name
    /// the same queue to meet.
    pub fn storage<J>(&self, queue: &str) -> RedisStorage<J>
    where
        J: Serialize + DeserializeOwned + Send + Sync + Unpin + 'static,
    {
        let config = apalis_redis::Config::default().set_namespace(&format!("nebula:{queue}"));
        RedisStorage::new_with_config(self.conn.clone(), config)
    }

    /// Push a job onto a queue; answers the task id.
    pub async fn enqueue<J>(&self, queue: &str, job: J) -> Result<String>
    where
        J: Serialize + DeserializeOwned + Send + Sync + Unpin + 'static,
    {
        let mut storage = self.storage::<J>(queue);
        let parts = storage
            .push(job)
            .await
            .map_err(|e| Error::internal(format!("failed to enqueue job on {queue:?}: {e}")))?;
        Ok(parts.task_id.to_string())
    }
}

/// Roll framework and application migrations across tenant databases —
/// how existing tenants pick up new features without a redeploy-restart.
/// `tenant_id: None` migrates every active tenant with its own database;
/// `Some(id)` targets one tenant (tenants sharing the main database have
/// nothing of their own to migrate — the main database migrates at boot).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrateTenants {
    pub tenant_id: Option<uuid::Uuid>,
}

/// Worker state for [`run_tenant_migrations`].
#[derive(Clone)]
pub(crate) struct MigrationContext {
    pub tenants: Arc<TenantManager>,
    pub migrations: Migrations,
}

pub(crate) async fn run_tenant_migrations(
    job: MigrateTenants,
    ctx: Data<MigrationContext>,
) -> Result<()> {
    for tenant in ctx.tenants.find_all().await? {
        if job.tenant_id.is_some_and(|id| id != tenant.id) {
            continue;
        }
        let has_own_db = tenant
            .connection_string
            .as_deref()
            .is_some_and(|s| !s.is_empty());
        if !tenant.is_active || !has_own_db {
            continue;
        }
        tracing::info!(tenant = %tenant.name, "migration job: migrating tenant database");
        let db = ctx.tenants.connection_for(&tenant).await?;
        ctx.migrations.apply(&db).await?;
    }
    Ok(())
}
