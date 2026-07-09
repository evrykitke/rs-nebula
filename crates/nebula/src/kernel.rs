//! The kernel bootstraps everything: configuration, logging, database,
//! modules, and the web host. `main.rs` stays a one-liner:
//!
//! ```no_run
//! use nebula::kernel::Kernel;
//!
//! #[tokio::main]
//! async fn main() -> nebula::Result<()> {
//!     Kernel::builder().build()?.run().await
//! }
//! ```

use crate::config::Config;
use crate::db;
use crate::error::{Error, Result};
use crate::logging::{self, LoggingError};
use crate::module::{Module, ModuleContext};
use crate::money::CurrencyRegistry;
use std::sync::Arc;
use axum::Router;
use sea_orm::DatabaseConnection;
use sea_orm_migration::MigratorTrait;
use std::future::Future;
use std::pin::Pin;
use tokio::net::TcpListener;

/// Type-erased migration runner so the kernel does not carry the
/// application's migrator type around.
type MigrationRunner = Box<
    dyn Fn(DatabaseConnection) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync,
>;

/// Composes and boots a Nebula application.
pub struct Kernel {
    config: Config,
    modules: Vec<Box<dyn Module>>,
    migrations: Option<MigrationRunner>,
}

impl Kernel {
    pub fn builder() -> KernelBuilder {
        KernelBuilder::default()
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Asynchronous boot phase: connect to the database when configured
    /// (verifying it answers, so a bad connection fails at boot rather
    /// than on the first request), then let every module configure itself.
    pub async fn init(self) -> Result<App> {
        let database = if self.config.database.url.is_empty() {
            tracing::info!("no database configured; booting without one");
            None
        } else {
            let db = db::connect(&self.config.database).await?;
            tracing::info!("database connection established");
            Some(db)
        };

        match (&self.migrations, &database, self.config.database.auto_migrate) {
            (Some(run), Some(db), true) => {
                tracing::info!("applying pending migrations");
                run(db.clone()).await?;
            }
            (Some(_), Some(_), false) => {
                tracing::info!("auto_migrate is off; skipping registered migrations");
            }
            (Some(_), None, _) => {
                tracing::warn!("migrations registered but no database configured");
            }
            (None, _, true) => {
                tracing::warn!("auto_migrate is on but no migrations were registered");
            }
            (None, _, false) => {}
        }

        let currencies = Arc::new(CurrencyRegistry::from_config(&self.config.currencies)?);

        let mut ctx = ModuleContext::new(&self.config, database.clone(), currencies.clone());
        for module in &self.modules {
            tracing::info!(module = module.name(), "configuring module");
            module.configure(&mut ctx);
        }
        let router = crate::web::finalize(ctx.into_router(), &self.config, database.clone());

        Ok(App {
            config: self.config,
            router,
            database,
            currencies,
        })
    }

    /// Boot and serve until shutdown.
    pub async fn run(self) -> Result<()> {
        self.init().await?.serve().await
    }
}

/// A fully booted application, ready to serve (or to be driven directly
/// in tests via [`App::router`]).
pub struct App {
    config: Config,
    router: Router,
    database: Option<DatabaseConnection>,
    currencies: Arc<CurrencyRegistry>,
}

impl App {
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The composed router — lets tests exercise the full stack without
    /// binding a socket.
    pub fn router(&self) -> Router {
        self.router.clone()
    }

    pub fn database(&self) -> Option<&DatabaseConnection> {
        self.database.as_ref()
    }

    pub fn currencies(&self) -> &CurrencyRegistry {
        &self.currencies
    }

    /// Serve until ctrl-c, then shut down gracefully so in-flight
    /// requests complete instead of being severed.
    pub async fn serve(self) -> Result<()> {
        let addr = format!("{}:{}", self.config.server.host, self.config.server.port);

        let listener = TcpListener::bind(&addr)
            .await
            .map_err(|e| Error::internal(format!("failed to bind {addr}: {e}")))?;

        tracing::info!(
            environment = %self.config.environment,
            multitenancy = self.config.multitenancy.enabled,
            "nebula listening on http://{addr}"
        );

        axum::serve(listener, self.router)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .map_err(Error::internal)
    }
}

async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!("failed to listen for shutdown signal: {e}");
        return;
    }
    tracing::info!("shutdown signal received, draining in-flight requests");
}

/// Builds a [`Kernel`]: collects modules, loads configuration and
/// initializes logging.
#[derive(Default)]
pub struct KernelBuilder {
    modules: Vec<Box<dyn Module>>,
    config: Option<Config>,
    migrations: Option<MigrationRunner>,
}

impl KernelBuilder {
    /// Register a module. Modules are configured in registration order.
    pub fn add_module(mut self, module: impl Module) -> Self {
        self.modules.push(Box::new(module));
        self
    }

    /// Register the application's migrations (a SeaORM `MigratorTrait`).
    /// They run during [`Kernel::init`] when `database.auto_migrate` is on.
    pub fn with_migrations<M: MigratorTrait + 'static>(mut self) -> Self {
        self.migrations = Some(Box::new(|db| {
            Box::pin(async move { M::up(&db, None).await.map_err(Error::from) })
        }));
        self
    }

    /// Use an explicit configuration instead of loading it from files and
    /// environment (useful in tests).
    pub fn with_config(mut self, config: Config) -> Self {
        self.config = Some(config);
        self
    }

    /// Load configuration, initialize logging and produce a ready kernel.
    /// Fails fast on invalid configuration.
    pub fn build(self) -> Result<Kernel> {
        let config = match self.config {
            Some(config) => config,
            None => Config::load()?,
        };

        // A second kernel in the same process (tests) is fine; any other
        // logging failure is a real boot error.
        match logging::init(&config.logging) {
            Ok(()) | Err(LoggingError::AlreadyInitialized) => {}
            Err(e) => return Err(e.into()),
        }

        Ok(Kernel {
            config,
            modules: self.modules,
            migrations: self.migrations,
        })
    }
}
