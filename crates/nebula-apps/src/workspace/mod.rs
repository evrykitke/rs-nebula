//! The workspace app: the default surface every user lands on. It is the
//! proving ground for framework features — right now, the reporting engine.

mod reports;

use nebula::{AdministrationModule, Module, ModuleContext};
use std::sync::Arc;

pub struct WorkspaceApp;

impl Module for WorkspaceApp {
    fn name(&self) -> &'static str {
        "workspace"
    }

    fn depends_on(&self) -> Vec<Box<dyn Module>> {
        // Workspace is used by signed-in people of a tenant, so it builds
        // on administration (which pulls in account).
        vec![Box::new(AdministrationModule)]
    }

    fn configure(&self, ctx: &mut ModuleContext) {
        ctx.declare_report(Arc::new(reports::WorkspaceOverview));
        ctx.declare_report(Arc::new(reports::SampleRegister));
    }
}
