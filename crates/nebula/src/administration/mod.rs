//! The administration module: the bounded context every ERP deployment
//! starts with — managing the people, permissions and reference data of
//! a company. One module, one `Pages.Administration` permission tree:
//!
//! - [`users`] — team onboarding, roles per user, per-user permission
//!   overrides, admin grants
//! - [`roles`] — role CRUD with permission grants
//! - [`company`] — the tenant's own settings: company profile, logo,
//!   two-factor mandate, database migration
//! - [`password_policy`] — the company password policy: length, character
//!   classes, expiry, reuse, lockout
//! - [`mail`] — the company's SMTP server
//! - [`currencies`] — the deployment-wide currency table
//! - [`audit`] — the audit trail with what-changed diffs and retention
//!
//! Depends on [`AccountModule`]: administration is done by signed-in
//! people, so registering this module pulls the account endpoints in
//! automatically.

pub mod audit;
pub mod company;
pub mod currencies;
pub mod mail;
pub mod password_policy;
pub mod roles;
pub mod users;

use crate::account::AccountModule;
use crate::auth::permission;
use crate::auth::state::AuthState;
use crate::module::{Module, ModuleContext};

pub struct AdministrationModule;

impl Module for AdministrationModule {
    fn name(&self) -> &'static str {
        "administration"
    }

    fn depends_on(&self) -> Vec<Box<dyn Module>> {
        vec![Box::new(AccountModule)]
    }

    fn configure(&self, ctx: &mut ModuleContext) {
        let state = AuthState::from_ctx(ctx);
        ctx.add_permissions(permission::administration_tree());
        ctx.add_api(users::api());
        ctx.add_api(roles::api());
        ctx.add_api(company::api());
        ctx.add_api(password_policy::api());
        ctx.add_api(mail::api());
        ctx.add_api(currencies::api());
        ctx.add_api(audit::api());
        ctx.add_routes(
            users::routes(state.clone())
                .merge(roles::routes())
                .merge(company::routes(state.clone()))
                .merge(password_policy::routes(state.clone()))
                .merge(mail::routes(state))
                .merge(currencies::routes(ctx.require_db()))
                .merge(audit::routes(
                    ctx.config().audit.clone(),
                    ctx.tenants(),
                )),
        );
    }
}
