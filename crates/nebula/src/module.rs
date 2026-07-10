//! The module system: applications are composed from modules, each
//! contributing routes (and, over time, jobs, event handlers, entities).
//! Modules keep the framework open for extension without modification.

use crate::auth::permission::PermissionDef;
use crate::config::Config;
use crate::jobs::Jobs;
use crate::money::CurrencyRegistry;
use crate::tenancy::TenantManager;
use apalis::prelude::Monitor;
use axum::Router;
use sea_orm::DatabaseConnection;
use std::sync::Arc;

/// A deferred worker registration: applied to the kernel's apalis
/// monitor after every module has configured.
pub(crate) type WorkerRegistration = Box<dyn FnOnce(Monitor) -> Monitor + Send>;

/// A composable unit of application functionality.
///
/// Implementations register what they provide through the
/// [`ModuleContext`] passed to [`Module::configure`]; the kernel calls
/// each module once during boot, in registration order.
pub trait Module: Send + Sync + 'static {
    /// Unique, human-readable module name (used in boot logs).
    fn name(&self) -> &'static str;

    /// Register the module's contributions.
    fn configure(&self, ctx: &mut ModuleContext);
}

/// Collects module contributions during boot.
pub struct ModuleContext<'a> {
    config: &'a Config,
    database: Option<DatabaseConnection>,
    currencies: Arc<CurrencyRegistry>,
    tenants: Option<Arc<TenantManager>>,
    jobs: Option<Jobs>,
    router: Router,
    permissions: Vec<PermissionDef>,
    workers: Vec<WorkerRegistration>,
}

impl<'a> ModuleContext<'a> {
    pub(crate) fn new(
        config: &'a Config,
        database: Option<DatabaseConnection>,
        currencies: Arc<CurrencyRegistry>,
        tenants: Option<Arc<TenantManager>>,
        jobs: Option<Jobs>,
    ) -> Self {
        Self {
            config,
            database,
            currencies,
            tenants,
            jobs,
            router: Router::new(),
            permissions: Vec::new(),
            workers: Vec::new(),
        }
    }

    /// The configured currency table, shared application-wide.
    pub fn currencies(&self) -> Arc<CurrencyRegistry> {
        self.currencies.clone()
    }

    /// The tenant manager, when multitenancy is enabled.
    pub fn tenants(&self) -> Option<Arc<TenantManager>> {
        self.tenants.clone()
    }

    /// The fully-resolved application configuration.
    pub fn config(&self) -> &Config {
        self.config
    }

    /// The main database pool, when one is configured. Cloning a
    /// `DatabaseConnection` is cheap (it shares the underlying pool).
    pub fn db(&self) -> Option<&DatabaseConnection> {
        self.database.as_ref()
    }

    /// The main database pool, for modules that cannot function without
    /// one. Fails loudly at boot with the module-facing explanation.
    pub fn require_db(&self) -> DatabaseConnection {
        self.database.clone().expect(
            "this module requires a database; configure database.url in \
             nebula.{env}.toml or NEBULA__DATABASE__URL",
        )
    }

    /// Merge the given routes into the application router.
    pub fn add_routes(&mut self, routes: Router) {
        self.router = std::mem::take(&mut self.router).merge(routes);
    }

    /// Contribute a permission tree. The kernel validates all trees
    /// together after every module has configured; duplicate or malformed
    /// names fail the boot.
    pub fn add_permissions(&mut self, tree: PermissionDef) {
        self.permissions.push(tree);
    }

    /// The job client, when `jobs.enabled` is on — for enqueueing and
    /// for building worker backends via [`Jobs::storage`].
    pub fn jobs(&self) -> Option<Jobs> {
        self.jobs.clone()
    }

    /// Contribute a background worker. The registration runs against the
    /// kernel's apalis monitor after all modules configure; it is
    /// silently dropped when jobs are disabled.
    ///
    /// ```ignore
    /// let jobs = ctx.jobs().expect("this module requires jobs.enabled");
    /// let storage = jobs.storage::<SendEmail>("emails");
    /// ctx.add_worker(move |monitor| {
    ///     monitor.register(
    ///         WorkerBuilder::new("emails")
    ///             .backend(storage)
    ///             .build_fn(send_email),
    ///     )
    /// });
    /// ```
    pub fn add_worker(&mut self, register: impl FnOnce(Monitor) -> Monitor + Send + 'static) {
        self.workers.push(Box::new(register));
    }

    pub(crate) fn into_parts(self) -> (Router, Vec<PermissionDef>, Vec<WorkerRegistration>) {
        (self.router, self.permissions, self.workers)
    }
}
