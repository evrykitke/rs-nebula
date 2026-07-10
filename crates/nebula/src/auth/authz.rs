//! Authorization extractor. A handler that takes [`Authz`] gets the
//! authenticated user (same validation as `CurrentUser`) plus permission
//! checks against the request's database:
//!
//! ```ignore
//! async fn delete_user(authz: Authz, ...) -> Result<...> {
//!     authz.require(names::USERS_DELETE).await?;
//!     ...
//! }
//! ```
//!
//! `require` answers 403 when the permission is not granted, and treats
//! a permission name missing from the registry as a programming error
//! (500) — a typo must never silently deny or allow.

use super::jwt::CurrentUser;
use super::permission::Registry;
use super::role_manager::RoleManager;
use super::user;
use crate::error::{Error, Result};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use sea_orm::DatabaseConnection;
use std::collections::HashSet;
use std::sync::Arc;

pub struct Authz {
    pub user: user::Model,
    roles: RoleManager,
    registry: Arc<Registry>,
}

impl Authz {
    pub async fn require(&self, permission: &str) -> Result<()> {
        if self.is_granted(permission).await? {
            Ok(())
        } else {
            Err(Error::Forbidden)
        }
    }

    pub async fn is_granted(&self, permission: &str) -> Result<bool> {
        if !self.registry.contains(permission) {
            return Err(Error::internal(format!(
                "permission {permission:?} is not defined by any module"
            )));
        }
        self.roles.is_granted(self.user.id, permission).await
    }

    pub async fn granted_permissions(&self) -> Result<HashSet<String>> {
        self.roles.granted_permissions(self.user.id).await
    }

    pub fn roles(&self) -> &RoleManager {
        &self.roles
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Authz {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let CurrentUser(user) = CurrentUser::from_request_parts(parts, state).await?;
        let db = parts
            .extensions
            .get::<DatabaseConnection>()
            .cloned()
            .ok_or_else(|| Error::Unauthorized.into_response())?;
        let registry = parts
            .extensions
            .get::<Arc<Registry>>()
            .cloned()
            .ok_or_else(|| {
                Error::internal("permission registry missing from request extensions")
                    .into_response()
            })?;
        Ok(Authz {
            user,
            roles: RoleManager::new(db, registry.clone()),
            registry,
        })
    }
}
