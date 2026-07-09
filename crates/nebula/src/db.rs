//! Database connectivity built on SeaORM/sqlx.
//!
//! The kernel connects during [`Kernel::init`](crate::kernel::Kernel::init)
//! when `database.url` is configured, verifies the connection with a ping
//! (fail fast at boot, not on the first request), and hands the pool to
//! modules through `ModuleContext::db()`.

use crate::config::DatabaseConfig;
use crate::error::{Error, Result};
use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use std::time::Duration;

/// Open a connection pool according to configuration and verify it works.
pub async fn connect(config: &DatabaseConfig) -> Result<DatabaseConnection> {
    if config.url.is_empty() {
        return Err(Error::internal(
            "database.url is not configured; set it in nebula.{env}.toml or NEBULA__DATABASE__URL",
        ));
    }

    let mut options = ConnectOptions::new(config.url.expose());
    options
        .max_connections(config.max_connections)
        .min_connections(config.min_connections)
        .connect_timeout(Duration::from_secs(config.connect_timeout_secs))
        .acquire_timeout(Duration::from_secs(config.acquire_timeout_secs))
        .idle_timeout(Duration::from_secs(config.idle_timeout_secs))
        .sqlx_logging_level(tracing::log::LevelFilter::Debug);

    let db = Database::connect(options).await?;
    ping(&db).await?;
    Ok(db)
}

/// Verify the database answers. Used at boot and by the readiness check.
pub async fn ping(db: &DatabaseConnection) -> Result<()> {
    db.ping().await.map_err(Error::from)
}
