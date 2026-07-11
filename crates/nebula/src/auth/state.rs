//! Shared handler state for the account and administration modules:
//! the auth configuration, the main database, and the tenant manager,
//! with the helpers both surfaces need to pick the right user store
//! and enforce tenant context.

use super::authz::Authz;
use super::manager::UserManager;
use crate::config::{AuthConfig, FilesConfig};
use crate::error::{Error, Result};
use crate::events::Events;
use crate::module::ModuleContext;
use crate::tenancy::{TenantManager, TenantRef};
use axum::Extension;
use sea_orm::DatabaseConnection;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct AuthState {
    pub(crate) config: AuthConfig,
    pub(crate) files: FilesConfig,
    pub(crate) main_db: DatabaseConnection,
    pub(crate) tenants: Option<Arc<TenantManager>>,
    pub(crate) events: Events,
}

impl AuthState {
    pub(crate) fn from_ctx(ctx: &ModuleContext) -> Self {
        Self {
            config: ctx.config().auth.clone(),
            files: ctx.config().files.clone(),
            main_db: ctx.require_db(),
            tenants: ctx.tenants(),
            events: ctx.events(),
        }
    }

    /// The user store for the current request's context. In multitenant
    /// mode it keeps the main-database login directory in sync so
    /// credential-based sign-in can resolve the tenant.
    pub(crate) fn users(&self, db: Option<DatabaseConnection>) -> UserManager {
        let users = UserManager::new(
            db.unwrap_or_else(|| self.main_db.clone()),
            self.config.clone(),
        );
        match &self.tenants {
            Some(_) => users.with_directory(self.main_db.clone()),
            None => users,
        }
    }

    pub(crate) async fn tenant_requires_2fa(&self, tenant: Option<&TenantRef>) -> Result<bool> {
        let (Some(manager), Some(tenant)) = (&self.tenants, tenant) else {
            return Ok(false);
        };
        Ok(manager
            .find_by_id(tenant.id)
            .await?
            .is_some_and(|t| t.require_two_factor))
    }

    /// The caller's own tenant, for the tenant-scoped settings
    /// endpoints: requires multitenancy, a tenant context, and that the
    /// authenticated user belongs to it.
    pub(crate) fn tenant_context(
        &self,
        authz: &Authz,
        tenant: Option<Extension<TenantRef>>,
    ) -> Result<(Arc<TenantManager>, TenantRef)> {
        let manager = self.tenants.clone().ok_or_else(|| {
            Error::Validation("multitenancy is not enabled on this deployment".into())
        })?;
        let Some(Extension(tenant)) = tenant else {
            return Err(Error::Validation("a tenant context is required".into()));
        };
        if authz.user.tenant_id != Some(tenant.id) {
            return Err(Error::Forbidden);
        }
        Ok((manager, tenant))
    }

    /// Validate a currency code against the currency table.
    pub(crate) async fn known_currency(&self, code: &str) -> Result<()> {
        crate::money::currency::Store::new(self.main_db.clone())
            .find_by_code(code)
            .await?
            .map(|_| ())
            .ok_or_else(|| Error::Validation(format!("unknown currency {code:?}")))
    }
}
