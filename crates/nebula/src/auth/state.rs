//! Shared handler state for the account and administration modules:
//! the auth configuration, the main database, and the tenant manager,
//! with the helpers both surfaces need to pick the right user store
//! and enforce tenant context.

use super::authz::Authz;
use super::manager::UserManager;
use super::policy::PasswordPolicy;
use crate::config::AuthConfig;
use crate::error::{Error, Result};
use crate::events::Events;
use crate::mail::Mailer;
use crate::module::ModuleContext;
use crate::storage::Storage;
use crate::tenancy::{TenantManager, TenantRef};
use axum::Extension;
use sea_orm::DatabaseConnection;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct AuthState {
    pub(crate) config: AuthConfig,
    pub(crate) storage: Storage,
    pub(crate) main_db: DatabaseConnection,
    pub(crate) tenants: Option<Arc<TenantManager>>,
    pub(crate) events: Events,
    pub(crate) mail: Mailer,
}

impl AuthState {
    pub(crate) fn from_ctx(ctx: &ModuleContext) -> Self {
        Self {
            config: ctx.config().auth.clone(),
            storage: ctx.storage(),
            main_db: ctx.require_db(),
            tenants: ctx.tenants(),
            events: ctx.events(),
            mail: Mailer::new(ctx.require_db(), ctx.config()),
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

    /// The user store for the current request, held to the company's
    /// password policy rather than the deployment default. Costs a tenant
    /// lookup, so it is for the handlers that actually weigh a password.
    pub(crate) async fn users_with_policy(
        &self,
        tenant_id: Option<uuid::Uuid>,
        db: Option<DatabaseConnection>,
    ) -> Result<UserManager> {
        let policy = self.policy_for(tenant_id).await?;
        Ok(self.users(db).with_policy(policy))
    }

    /// The password policy in force for a tenant: the deployment's, with
    /// the company's overrides laid over it. No tenant — a host user, or
    /// single-tenant mode — means the deployment's policy stands.
    pub(crate) async fn policy_for(&self, tenant_id: Option<uuid::Uuid>) -> Result<PasswordPolicy> {
        let (Some(manager), Some(id)) = (&self.tenants, tenant_id) else {
            return Ok(PasswordPolicy::from_config(&self.config));
        };
        let tenant = manager.find_by_id(id).await?;
        Ok(PasswordPolicy::resolve(&self.config, tenant.as_ref()))
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

    /// The directory name of a user's tenant — for responses that name
    /// the tenant a session belongs to. Answers from the user's own row,
    /// never the request header, so it cannot mislabel the session.
    pub(crate) async fn tenant_name_of(
        &self,
        tenant_id: Option<uuid::Uuid>,
    ) -> Result<Option<String>> {
        let (Some(manager), Some(id)) = (&self.tenants, tenant_id) else {
            return Ok(None);
        };
        Ok(manager.find_by_id(id).await?.map(|t| t.name))
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
