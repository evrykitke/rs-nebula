//! The module system: applications are composed from modules, each
//! contributing routes (and, over time, jobs, event handlers, entities).
//! Modules keep the framework open for extension without modification.

use crate::config::Config;
use axum::Router;

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
    router: Router,
}

impl<'a> ModuleContext<'a> {
    pub(crate) fn new(config: &'a Config) -> Self {
        Self {
            config,
            router: Router::new(),
        }
    }

    /// The fully-resolved application configuration.
    pub fn config(&self) -> &Config {
        self.config
    }

    /// Merge the given routes into the application router.
    pub fn add_routes(&mut self, routes: Router) {
        self.router = std::mem::take(&mut self.router).merge(routes);
    }

    pub(crate) fn into_router(self) -> Router {
        self.router
    }
}
