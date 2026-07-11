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

use crate::auth::permission;
use crate::config::Config;
use crate::db;
use crate::error::{Error, Result};
use crate::jobs::Jobs;
use crate::logging::{self, LoggingError};
use crate::module::{Module, ModuleContext};
use crate::money::CurrencyRegistry;
use crate::tenancy::TenantManager;
use apalis::prelude::{Monitor, WorkerBuilder, WorkerFactoryFn};
use axum::Router;
use sea_orm::DatabaseConnection;
use sea_orm_migration::MigratorTrait;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::net::TcpListener;

/// Type-erased migration runner so the kernel does not carry the
/// application's migrator type around. Shared so the same migrations
/// can run on the main and each tenant database.
pub(crate) type MigrationRunner = Arc<
    dyn Fn(DatabaseConnection) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync,
>;

/// The full migration set applied to a database: the framework's in-house
/// SeaORM schema first, then the application's registered SeaORM migrator
/// (if any), then the module SQL migrations. The same set runs on the
/// main database and on every tenant database, so a freshly provisioned
/// or existing tenant lands on an identical schema.
#[derive(Clone)]
pub(crate) struct Migrations {
    app: Option<MigrationRunner>,
    sql: Arc<crate::sql_migrations::SqlMigrator>,
}

impl Migrations {
    pub(crate) fn new(app: Option<MigrationRunner>, config: &crate::config::MigrationsConfig) -> Self {
        Self {
            app,
            sql: Arc::new(crate::sql_migrations::SqlMigrator::from_config(config)),
        }
    }

    /// Bring `db` fully up to date: framework, then application, then
    /// module SQL migrations.
    pub(crate) async fn apply(&self, db: &DatabaseConnection) -> Result<()> {
        crate::migrations::Migrator::up(db, None)
            .await
            .map_err(Error::from)?;
        if let Some(run) = &self.app {
            run(db.clone()).await?;
        }
        self.sql.run(db).await?;
        Ok(())
    }
}

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

        let migrations = Migrations::new(self.migrations.clone(), &self.config.migrations);

        let tenants = if self.config.multitenancy.enabled {
            let Some(db) = &database else {
                return Err(Error::internal(
                    "multitenancy is enabled but no database is configured",
                ));
            };
            Some(Arc::new(TenantManager::new(
                db.clone(),
                self.config.database.clone(),
                self.config.multitenancy.clone(),
                migrations.clone(),
            )))
        } else {
            None
        };

        let auto_migrate = self.config.database.auto_migrate;

        match (&database, auto_migrate) {
            (Some(db), true) => {
                tracing::info!("applying migrations to the main database");
                migrations.apply(db).await?;
            }
            (Some(_), false) => tracing::info!("auto_migrate is off; skipping migrations"),
            (None, _) => {}
        }

        if let (Some(manager), true) = (&tenants, auto_migrate) {
            for tenant in manager.find_all().await? {
                let has_own_db = tenant
                    .connection_string
                    .as_deref()
                    .is_some_and(|s| !s.is_empty());
                if tenant.is_active && has_own_db {
                    tracing::info!(tenant = %tenant.name, "applying migrations to tenant database");
                    let db = manager.connection_for(&tenant).await?;
                    migrations.apply(&db).await?;
                }
            }
        }

        // The currency table (seeded by migrations) plus the app's
        // configured units; config entries win so an application can
        // re-declare a code. A missing table (auto_migrate off on a
        // fresh database) degrades to config-only with a warning.
        let mut registry = CurrencyRegistry::default();
        if let Some(db) = &database {
            use sea_orm::EntityTrait;
            match crate::money::currency::Entity::find().all(db).await {
                Ok(rows) => {
                    for row in rows {
                        match crate::money::Currency::new(&row.code, row.minor_units as u8) {
                            Ok(currency) => registry.insert(currency),
                            Err(e) => tracing::warn!(code = %row.code, error = %e,
                                "skipping unusable currency row"),
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e,
                    "could not load the currency table; using configured currencies only"),
            }
        }
        for entry in &self.config.currencies {
            registry.insert(crate::money::Currency::new(&entry.code, entry.minor_units)?);
        }
        let currencies = Arc::new(registry);
        tracing::info!(count = currencies.len(), "currency registry built");

        let jobs = if self.config.jobs.enabled {
            let conn = apalis_redis::connect(self.config.redis.url.expose())
                .await
                .map_err(|e| {
                    Error::internal(format!("jobs are enabled but Redis is unreachable: {e}"))
                })?;
            tracing::info!("job queue connected to redis");
            Some(Jobs::new(conn))
        } else {
            None
        };

        let events = crate::events::Events::new();
        let storage = crate::storage::Storage::new(&self.config.files);

        let mut ctx = ModuleContext::new(
            &self.config,
            database.clone(),
            currencies.clone(),
            tenants.clone(),
            jobs.clone(),
            events.clone(),
            storage.clone(),
        );
        for module in &self.modules {
            tracing::info!(module = module.name(), "configuring module");
            module.configure(&mut ctx);
        }
        let parts = ctx.into_parts();

        // Subscriptions are in place; attach the broker so queue
        // bindings cover every subscribed event. The consumer itself
        // starts with `serve` (or `start_events` in tests).
        if self.config.events.distributed {
            events.connect(&self.config.rabbitmq, &self.config.events).await?;
        }

        let permissions = Arc::new(permission::Registry::build(parts.permissions)?);
        tracing::info!(count = permissions.len(), "permission registry built");
        let router = crate::web::finalize(
            parts.router,
            &self.config,
            database.clone(),
            tenants.clone(),
            permissions.clone(),
            jobs.clone(),
            events.clone(),
            storage.clone(),
            parts.api_docs,
        );

        let monitor = match &jobs {
            Some(client) => Some(self.build_monitor(
                client,
                &database,
                &tenants,
                &migrations,
                parts.workers,
            )),
            None => None,
        };

        Ok(App {
            config: self.config,
            router,
            database,
            currencies,
            tenants,
            permissions,
            jobs,
            events,
            storage,
            monitor,
        })
    }

    /// Boot and serve until shutdown.
    pub async fn run(self) -> Result<()> {
        self.init().await?.serve().await
    }

    /// Assemble the apalis monitor: built-in workers (audit pruner cron,
    /// tenant migrations) plus every module-contributed registration.
    fn build_monitor(
        &self,
        jobs: &Jobs,
        database: &Option<DatabaseConnection>,
        tenants: &Option<Arc<TenantManager>>,
        migrations: &Migrations,
        worker_regs: Vec<crate::module::WorkerRegistration>,
    ) -> Monitor {
        let mut monitor = Monitor::new();

        if let Some(manager) = tenants {
            let ctx = crate::jobs::MigrationContext {
                tenants: manager.clone(),
                migrations: migrations.clone(),
            };
            monitor = monitor.register(
                WorkerBuilder::new("nebula-tenant-migrations")
                    .data(ctx)
                    .backend(jobs.storage::<crate::jobs::MigrateTenants>(
                        crate::jobs::TENANT_MIGRATION_QUEUE,
                    ))
                    .build_fn(crate::jobs::run_tenant_migrations),
            );
        }

        if let (Some(db), true) = (database, self.config.audit.enabled) {
            let ctx = crate::audit::pruner::PruneContext {
                db: db.clone(),
                tenants: tenants.clone(),
                config: self.config.audit.clone(),
            };
            let schedule =
                crate::audit::pruner::interval_schedule(self.config.audit.prune_interval_secs);
            monitor = monitor.register(
                WorkerBuilder::new("nebula-audit-pruner")
                    .data(ctx)
                    .backend(apalis_cron::CronStream::new(schedule))
                    .build_fn(crate::audit::pruner::prune_tick),
            );
        }

        for register in worker_regs {
            monitor = register(monitor);
        }
        monitor
    }
}

/// A fully booted application, ready to serve (or to be driven directly
/// in tests via [`App::router`]).
pub struct App {
    config: Config,
    router: Router,
    database: Option<DatabaseConnection>,
    currencies: Arc<CurrencyRegistry>,
    tenants: Option<Arc<TenantManager>>,
    permissions: Arc<permission::Registry>,
    jobs: Option<Jobs>,
    events: crate::events::Events,
    storage: crate::storage::Storage,
    monitor: Option<Monitor>,
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

    pub fn tenants(&self) -> Option<Arc<TenantManager>> {
        self.tenants.clone()
    }

    pub fn permissions(&self) -> Arc<permission::Registry> {
        self.permissions.clone()
    }

    /// The job client, when `jobs.enabled` is on.
    pub fn jobs(&self) -> Option<Jobs> {
        self.jobs.clone()
    }

    /// The event bus.
    pub fn events(&self) -> crate::events::Events {
        self.events.clone()
    }

    /// The public file store.
    pub fn storage(&self) -> crate::storage::Storage {
        self.storage.clone()
    }

    /// Start the integration-event consumer (idempotent; `serve` calls
    /// this). A no-op unless `events.distributed` connected a broker.
    pub fn start_events(&self) -> bool {
        self.events.start_consumer()
    }

    /// Start the job workers (idempotent; `serve` calls this). Answers
    /// whether a monitor was started — tests drive workers with this
    /// without binding a socket.
    pub fn start_jobs(&mut self) -> bool {
        let Some(monitor) = self.monitor.take() else {
            return false;
        };
        tracing::info!("starting job workers");
        tokio::spawn(async move {
            if let Err(e) = monitor.run_with_signal(tokio::signal::ctrl_c()).await {
                tracing::error!(error = %e, "job monitor exited with an error");
            }
        });
        true
    }

    /// Serve until ctrl-c, then shut down gracefully so in-flight
    /// requests complete instead of being severed.
    pub async fn serve(mut self) -> Result<()> {
        self.start_events();
        let jobs_started = self.start_jobs();
        // Without the job system the audit pruner falls back to a plain
        // in-process interval.
        if !jobs_started && let (Some(db), true) = (&self.database, self.config.audit.enabled) {
            crate::audit::pruner::spawn(
                db.clone(),
                self.tenants.clone(),
                self.config.audit.clone(),
            );
        }

        let addr = format!("{}:{}", self.config.server.host, self.config.server.port);

        let listener = TcpListener::bind(&addr)
            .await
            .map_err(|e| Error::internal(format!("failed to bind {addr}: {e}")))?;

        tracing::info!(
            environment = %self.config.environment,
            multitenancy = self.config.multitenancy.enabled,
            "nebula listening on http://{addr}"
        );

        // Connect info feeds the audit trail's ip fallback when no
        // proxy header is present.
        axum::serve(
            listener,
            self.router
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
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
    /// Register a module. Declared dependencies are registered
    /// automatically; modules configure dependencies-first, then in
    /// registration order.
    pub fn add_module(mut self, module: impl Module) -> Self {
        self.modules.push(Box::new(module));
        self
    }

    /// Register the application's migrations (a SeaORM `MigratorTrait`).
    /// They run during [`Kernel::init`] when `database.auto_migrate` is on.
    pub fn with_migrations<M: MigratorTrait + 'static>(mut self) -> Self {
        self.migrations = Some(Arc::new(|db| {
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

        // Pull in declared dependencies and order them ahead of their
        // dependents; a registration mistake fails here, not at runtime.
        let modules = crate::module::resolve(self.modules)?;

        Ok(Kernel {
            config,
            modules,
            migrations: self.migrations,
        })
    }
}
