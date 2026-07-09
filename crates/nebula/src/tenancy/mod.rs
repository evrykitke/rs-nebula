//! Multitenancy: each tenant may have its own database, with the main
//! database acting as the tenant directory. Fully toggleable — with
//! `multitenancy.enabled: false` the application runs single-tenant
//! against the main database (self-hosted mode).
//!
//! Pieces:
//! - [`tenant`] — the directory entity (`tenants` table in the main db)
//! - [`migrations`] — framework-owned schema for the directory, tracked
//!   in its own `nebula_migrations` table
//! - [`TenantManager`] — directory lookups, tenant creation, and a lazy
//!   cache of per-tenant connection pools
//! - request resolution middleware and extractors live in `middleware`
//!   (wired by the web layer when multitenancy is enabled)

pub mod middleware;
pub mod migrations;
pub mod tenant;

use crate::config::{DatabaseConfig, MultitenancyConfig};
use crate::db;
use crate::error::{Error, Result};
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use std::collections::HashMap;
use tokio::sync::RwLock;

/// The resolved tenant of the current request, inserted into request
/// extensions by the resolution middleware.
#[derive(Clone, Debug, serde::Serialize)]
pub struct TenantRef {
    pub id: i32,
    pub name: String,
}

pub struct NewTenant {
    pub name: String,
    pub display_name: String,
    /// `None` shares the main database.
    pub connection_string: Option<String>,
}

/// Directory lookups and per-tenant connection pooling. One instance is
/// created by the kernel when multitenancy is enabled and shared
/// application-wide.
pub struct TenantManager {
    main: DatabaseConnection,
    db_config: DatabaseConfig,
    config: MultitenancyConfig,
    pools: RwLock<HashMap<i32, DatabaseConnection>>,
}

impl TenantManager {
    pub(crate) fn new(
        main: DatabaseConnection,
        db_config: DatabaseConfig,
        config: MultitenancyConfig,
    ) -> Self {
        Self {
            main,
            db_config,
            config,
            pools: RwLock::new(HashMap::new()),
        }
    }

    pub fn header_name(&self) -> &str {
        &self.config.header
    }

    pub fn main_db(&self) -> &DatabaseConnection {
        &self.main
    }

    pub async fn find_by_name(&self, name: &str) -> Result<Option<tenant::Model>> {
        tenant::Entity::find()
            .filter(tenant::Column::Name.eq(name))
            .one(&self.main)
            .await
            .map_err(Error::from)
    }

    pub async fn find_all(&self) -> Result<Vec<tenant::Model>> {
        tenant::Entity::find()
            .all(&self.main)
            .await
            .map_err(Error::from)
    }

    pub async fn create(&self, new: NewTenant) -> Result<tenant::Model> {
        validate_name(&new.name)?;
        if self.find_by_name(&new.name).await?.is_some() {
            return Err(Error::Conflict(format!(
                "tenant {:?} already exists",
                new.name
            )));
        }
        tenant::ActiveModel {
            name: Set(new.name),
            display_name: Set(new.display_name),
            connection_string: Set(new.connection_string),
            is_active: Set(true),
            created_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(&self.main)
        .await
        .map_err(Error::from)
    }

    /// The connection to use for this tenant: its own pool (created
    /// lazily, cached by tenant id) or the shared main database.
    pub async fn connection_for(&self, tenant: &tenant::Model) -> Result<DatabaseConnection> {
        let Some(url) = tenant
            .connection_string
            .as_deref()
            .filter(|s| !s.is_empty())
        else {
            return Ok(self.main.clone());
        };

        if let Some(db) = self.pools.read().await.get(&tenant.id) {
            return Ok(db.clone());
        }

        let db = db::connect(&DatabaseConfig {
            url: url.into(),
            ..self.db_config.clone()
        })
        .await?;
        self.pools.write().await.insert(tenant.id, db.clone());
        Ok(db)
    }
}

fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !ok {
        return Err(Error::Validation(format!(
            "tenant name must be 1-64 lowercase letters, digits or dashes, got {name:?}"
        )));
    }
    Ok(())
}
