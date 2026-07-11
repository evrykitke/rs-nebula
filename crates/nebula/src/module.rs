//! The module system: applications are composed from modules, each
//! contributing routes (and, over time, jobs, event handlers, entities).
//! Modules keep the framework open for extension without modification.
//!
//! A module is a bounded context (Administration, Accounting, Sales) —
//! not an individual feature. Modules declare what they build on via
//! [`Module::depends_on`]; the kernel walks the graph, deduplicates by
//! name, and configures dependencies first, so an application registers
//! only its top-level modules and `main.rs` stays a one-liner.

use crate::auth::permission::PermissionDef;
use crate::config::Config;
use crate::events::Events;
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
    /// Unique, human-readable module name (used in boot logs and for
    /// deduplication when several modules depend on the same one).
    fn name(&self) -> &'static str;

    /// Modules this one requires. The kernel registers them
    /// automatically and configures them before this module, so an
    /// application never has to know a module's transitive needs.
    fn depends_on(&self) -> Vec<Box<dyn Module>> {
        Vec::new()
    }

    /// Register the module's contributions.
    fn configure(&self, ctx: &mut ModuleContext);
}

/// Resolve the registration list into configuration order: dependencies
/// first, duplicates (by name) dropped, cycles rejected at boot.
pub(crate) fn resolve(
    modules: Vec<Box<dyn Module>>,
) -> crate::error::Result<Vec<Box<dyn Module>>> {
    fn visit(
        module: Box<dyn Module>,
        seen: &mut std::collections::HashSet<&'static str>,
        path: &mut Vec<&'static str>,
        ordered: &mut Vec<Box<dyn Module>>,
    ) -> crate::error::Result<()> {
        let name = module.name();
        if seen.contains(name) {
            return Ok(());
        }
        if path.contains(&name) {
            return Err(crate::error::Error::internal(format!(
                "module dependency cycle: {} -> {name}",
                path.join(" -> ")
            )));
        }
        path.push(name);
        for dependency in module.depends_on() {
            visit(dependency, seen, path, ordered)?;
        }
        path.pop();
        seen.insert(name);
        ordered.push(module);
        Ok(())
    }

    let mut ordered = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut path = Vec::new();
    for module in modules {
        visit(module, &mut seen, &mut path, &mut ordered)?;
    }
    Ok(ordered)
}

/// Collects module contributions during boot.
pub struct ModuleContext<'a> {
    config: &'a Config,
    database: Option<DatabaseConnection>,
    currencies: Arc<CurrencyRegistry>,
    tenants: Option<Arc<TenantManager>>,
    jobs: Option<Jobs>,
    events: Events,
    router: Router,
    permissions: Vec<PermissionDef>,
    workers: Vec<WorkerRegistration>,
    api_docs: Vec<utoipa::openapi::OpenApi>,
}

impl<'a> ModuleContext<'a> {
    pub(crate) fn new(
        config: &'a Config,
        database: Option<DatabaseConnection>,
        currencies: Arc<CurrencyRegistry>,
        tenants: Option<Arc<TenantManager>>,
        jobs: Option<Jobs>,
        events: Events,
    ) -> Self {
        Self {
            config,
            database,
            currencies,
            tenants,
            jobs,
            events,
            router: Router::new(),
            permissions: Vec::new(),
            workers: Vec::new(),
            api_docs: Vec::new(),
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

    /// The event bus: subscribe to other contexts' events here in
    /// `configure`, keep the (cheap) clone for publishing at runtime.
    pub fn events(&self) -> Events {
        self.events.clone()
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

    /// Contribute an OpenAPI document describing the module's endpoints;
    /// merged into the application document served at
    /// `/api-docs/openapi.json` (the input for client generators).
    pub fn add_api(&mut self, api: utoipa::openapi::OpenApi) {
        self.api_docs.push(api);
    }

    pub(crate) fn into_parts(self) -> ModuleParts {
        ModuleParts {
            router: self.router,
            permissions: self.permissions,
            workers: self.workers,
            api_docs: self.api_docs,
        }
    }
}

/// Build an OpenAPI document on a dedicated thread with a large stack.
/// The `OpenApi` derive expands to a single deeply-nested expression, and
/// evaluating it for a module with many endpoints can overflow the
/// default stack in unoptimized builds.
pub fn build_openapi<F>(build: F) -> utoipa::openapi::OpenApi
where
    F: FnOnce() -> utoipa::openapi::OpenApi + Send + 'static,
{
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(build)
        .expect("failed to spawn the openapi builder thread")
        .join()
        .expect("openapi construction panicked")
}

/// Everything the modules contributed, handed back to the kernel.
pub(crate) struct ModuleParts {
    pub router: Router,
    pub permissions: Vec<PermissionDef>,
    pub workers: Vec<WorkerRegistration>,
    pub api_docs: Vec<utoipa::openapi::OpenApi>,
}
