//! Role and permission management over one database (main or tenant),
//! plus the resolution rules, ASP.NET Zero style:
//!
//! 1. a per-user override row decides outright — an explicit deny beats
//!    every role grant, an explicit grant works without any role;
//! 2. otherwise membership in a static role (`Admin`) grants everything;
//! 3. otherwise the user's roles' grant rows are consulted.
//!
//! Grant mutations are validated against the boot-time permission
//! registry so a typo cannot silently grant nothing.

use super::permission::Registry;
use super::role::{self, permission_grant, user_role};
use crate::error::{Error, Result};
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Set,
    TransactionTrait,
};
use std::collections::HashSet;
use std::sync::Arc;

/// Name of the static role seeded for every tenant's first user.
pub const ADMIN_ROLE: &str = "Admin";

pub struct RoleManager {
    db: DatabaseConnection,
    registry: Arc<Registry>,
}

impl RoleManager {
    pub fn new(db: DatabaseConnection, registry: Arc<Registry>) -> Self {
        Self { db, registry }
    }

    pub async fn create_role(
        &self,
        tenant_id: Option<i32>,
        name: &str,
        display_name: &str,
        permissions: &[String],
    ) -> Result<role::Model> {
        let name = name.trim();
        let display_name = display_name.trim();
        if name.is_empty() || name.len() > 64 {
            return Err(Error::Validation(
                "role name must be 1-64 characters".into(),
            ));
        }
        if display_name.is_empty() || display_name.len() > 128 {
            return Err(Error::Validation(
                "role display name must be 1-128 characters".into(),
            ));
        }
        self.validate_names(permissions)?;
        if self.find_by_name(tenant_id, name).await?.is_some() {
            return Err(Error::Conflict(format!("role {name:?} already exists")));
        }
        let tx = self.db.begin().await?;
        let role = role::ActiveModel {
            tenant_id: Set(tenant_id),
            name: Set(name.to_string()),
            display_name: Set(display_name.to_string()),
            is_static: Set(false),
            created_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&tx)
        .await?;
        for permission in permissions {
            permission_grant::ActiveModel {
                permission: Set(permission.clone()),
                role_id: Set(Some(role.id)),
                user_id: Set(None),
                is_granted: Set(true),
                ..Default::default()
            }
            .insert(&tx)
            .await?;
        }
        tx.commit().await?;
        Ok(role)
    }

    /// Find or create the static `Admin` role for a tenant. Called at
    /// company registration so the first user has somewhere to sit.
    pub async fn ensure_admin_role(&self, tenant_id: Option<i32>) -> Result<role::Model> {
        if let Some(existing) = self.find_by_name(tenant_id, ADMIN_ROLE).await? {
            return Ok(existing);
        }
        Ok(role::ActiveModel {
            tenant_id: Set(tenant_id),
            name: Set(ADMIN_ROLE.to_string()),
            display_name: Set("Administrator".to_string()),
            is_static: Set(true),
            created_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&self.db)
        .await?)
    }

    pub async fn find_by_id(&self, id: i32) -> Result<role::Model> {
        role::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or_else(|| Error::NotFound("role".into()))
    }

    pub async fn find_by_name(
        &self,
        tenant_id: Option<i32>,
        name: &str,
    ) -> Result<Option<role::Model>> {
        Ok(role::Entity::find()
            .filter(tenant_filter(tenant_id))
            .filter(role::Column::Name.eq(name))
            .one(&self.db)
            .await?)
    }

    pub async fn find_all(&self, tenant_id: Option<i32>) -> Result<Vec<role::Model>> {
        Ok(role::Entity::find()
            .filter(tenant_filter(tenant_id))
            .order_by_asc(role::Column::Name)
            .all(&self.db)
            .await?)
    }

    pub async fn update_role(&self, id: i32, display_name: &str) -> Result<role::Model> {
        let display_name = display_name.trim();
        if display_name.is_empty() || display_name.len() > 128 {
            return Err(Error::Validation(
                "role display name must be 1-128 characters".into(),
            ));
        }
        let role = self.find_by_id(id).await?;
        let mut active: role::ActiveModel = role.into();
        active.display_name = Set(display_name.to_string());
        Ok(active.update(&self.db).await?)
    }

    /// Delete a role together with its assignments and grants. Static
    /// roles are framework-managed and cannot be deleted.
    pub async fn delete_role(&self, id: i32) -> Result<()> {
        let role = self.find_by_id(id).await?;
        if role.is_static {
            return Err(Error::Validation(format!(
                "role {:?} is static and cannot be deleted",
                role.name
            )));
        }
        let tx = self.db.begin().await?;
        user_role::Entity::delete_many()
            .filter(user_role::Column::RoleId.eq(id))
            .exec(&tx)
            .await?;
        permission_grant::Entity::delete_many()
            .filter(permission_grant::Column::RoleId.eq(id))
            .exec(&tx)
            .await?;
        role::Entity::delete_by_id(id).exec(&tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn assign_role(&self, user_id: i32, role_id: i32) -> Result<()> {
        self.find_by_id(role_id).await?;
        let already = user_role::Entity::find_by_id((user_id, role_id))
            .one(&self.db)
            .await?;
        if already.is_none() {
            user_role::ActiveModel {
                user_id: Set(user_id),
                role_id: Set(role_id),
            }
            .insert(&self.db)
            .await?;
        }
        Ok(())
    }

    pub async fn unassign_role(&self, user_id: i32, role_id: i32) -> Result<()> {
        user_role::Entity::delete_by_id((user_id, role_id))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    pub async fn roles_of(&self, user_id: i32) -> Result<Vec<role::Model>> {
        let role_ids: Vec<i32> = user_role::Entity::find()
            .filter(user_role::Column::UserId.eq(user_id))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|r| r.role_id)
            .collect();
        if role_ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(role::Entity::find()
            .filter(role::Column::Id.is_in(role_ids))
            .order_by_asc(role::Column::Name)
            .all(&self.db)
            .await?)
    }

    /// Replace a role's grants with exactly the given permission names.
    /// Static roles implicitly hold everything, so editing them is refused.
    pub async fn set_role_permissions(&self, role_id: i32, permissions: &[String]) -> Result<()> {
        let role = self.find_by_id(role_id).await?;
        if role.is_static {
            return Err(Error::Validation(format!(
                "role {:?} is static and implicitly holds every permission",
                role.name
            )));
        }
        self.validate_names(permissions)?;
        let tx = self.db.begin().await?;
        permission_grant::Entity::delete_many()
            .filter(permission_grant::Column::RoleId.eq(role_id))
            .exec(&tx)
            .await?;
        for name in permissions {
            permission_grant::ActiveModel {
                permission: Set(name.clone()),
                role_id: Set(Some(role_id)),
                user_id: Set(None),
                is_granted: Set(true),
                ..Default::default()
            }
            .insert(&tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn role_permissions(&self, role_id: i32) -> Result<Vec<String>> {
        let mut names: Vec<String> = permission_grant::Entity::find()
            .filter(permission_grant::Column::RoleId.eq(role_id))
            .filter(permission_grant::Column::IsGranted.eq(true))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|g| g.permission)
            .collect();
        names.sort();
        Ok(names)
    }

    /// Set per-user overrides: `granted` rows add permissions the user's
    /// roles do not give, `denied` rows take away ones they do. Any
    /// permission not listed falls back to role resolution.
    pub async fn set_user_permissions(
        &self,
        user_id: i32,
        granted: &[String],
        denied: &[String],
    ) -> Result<()> {
        self.validate_names(granted)?;
        self.validate_names(denied)?;
        if let Some(dup) = granted.iter().find(|g| denied.contains(g)) {
            return Err(Error::Validation(format!(
                "permission {dup:?} is both granted and denied"
            )));
        }
        let tx = self.db.begin().await?;
        permission_grant::Entity::delete_many()
            .filter(permission_grant::Column::UserId.eq(user_id))
            .exec(&tx)
            .await?;
        for (names, is_granted) in [(granted, true), (denied, false)] {
            for name in names {
                permission_grant::ActiveModel {
                    permission: Set(name.clone()),
                    role_id: Set(None),
                    user_id: Set(Some(user_id)),
                    is_granted: Set(is_granted),
                    ..Default::default()
                }
                .insert(&tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn user_overrides(&self, user_id: i32) -> Result<Vec<permission_grant::Model>> {
        Ok(permission_grant::Entity::find()
            .filter(permission_grant::Column::UserId.eq(user_id))
            .all(&self.db)
            .await?)
    }

    /// The resolution rule: user override first (deny wins), then static
    /// role membership (grants all), then the user's roles' grants.
    pub async fn is_granted(&self, user_id: i32, permission: &str) -> Result<bool> {
        let user_row = permission_grant::Entity::find()
            .filter(permission_grant::Column::UserId.eq(user_id))
            .filter(permission_grant::Column::Permission.eq(permission))
            .one(&self.db)
            .await?;
        if let Some(row) = user_row {
            return Ok(row.is_granted);
        }

        let roles = self.roles_of(user_id).await?;
        if roles.iter().any(|r| r.is_static) {
            return Ok(true);
        }
        let role_ids: Vec<i32> = roles.iter().map(|r| r.id).collect();
        if role_ids.is_empty() {
            return Ok(false);
        }
        Ok(permission_grant::Entity::find()
            .filter(permission_grant::Column::RoleId.is_in(role_ids))
            .filter(permission_grant::Column::Permission.eq(permission))
            .filter(permission_grant::Column::IsGranted.eq(true))
            .one(&self.db)
            .await?
            .is_some())
    }

    /// Every permission the user effectively holds, for admin UIs and
    /// the profile endpoint.
    pub async fn granted_permissions(&self, user_id: i32) -> Result<HashSet<String>> {
        let roles = self.roles_of(user_id).await?;
        let mut effective: HashSet<String> = if roles.iter().any(|r| r.is_static) {
            self.registry.all_names().map(String::from).collect()
        } else {
            let role_ids: Vec<i32> = roles.iter().map(|r| r.id).collect();
            if role_ids.is_empty() {
                HashSet::new()
            } else {
                permission_grant::Entity::find()
                    .filter(permission_grant::Column::RoleId.is_in(role_ids))
                    .filter(permission_grant::Column::IsGranted.eq(true))
                    .all(&self.db)
                    .await?
                    .into_iter()
                    .map(|g| g.permission)
                    .collect()
            }
        };
        for row in self.user_overrides(user_id).await? {
            if row.is_granted {
                effective.insert(row.permission);
            } else {
                effective.remove(&row.permission);
            }
        }
        Ok(effective)
    }

    fn validate_names(&self, names: &[String]) -> Result<()> {
        for name in names {
            if !self.registry.contains(name) {
                return Err(Error::Validation(format!("unknown permission {name:?}")));
            }
        }
        Ok(())
    }
}

fn tenant_filter(tenant_id: Option<i32>) -> sea_orm::sea_query::SimpleExpr {
    match tenant_id {
        Some(id) => role::Column::TenantId.eq(id),
        None => role::Column::TenantId.is_null(),
    }
}
