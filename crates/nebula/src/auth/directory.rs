//! The login directory: which company do a set of credentials belong to?
//!
//! Users live in per-tenant stores (possibly tenant-owned databases), so
//! signing in by credentials alone needs a main-database index mapping
//! normalized logins to tenants. Rows are written whenever a user is
//! created and removed on soft delete; resolution treats them only as
//! candidates — the actual password check always happens against the
//! tenant's own user store, so a stale row can never authenticate anyone.

use super::manager::normalize;
use super::user;
use crate::error::{Error, Result};
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, QuerySelect, Set};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "user_directory")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub tenant_id: i32,
    pub user_id: i32,
    pub normalized_user_name: String,
    pub normalized_email: String,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// Main-database index of every tenant user's login identifiers.
pub struct Directory {
    main: DatabaseConnection,
}

impl Directory {
    pub fn new(main: DatabaseConnection) -> Self {
        Self { main }
    }

    pub async fn add(&self, tenant_id: i32, user: &user::Model) -> Result<()> {
        ActiveModel {
            tenant_id: Set(tenant_id),
            user_id: Set(user.id),
            normalized_user_name: Set(user.normalized_user_name.clone()),
            normalized_email: Set(user.normalized_email.clone()),
            created_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(&self.main)
        .await
        .map(|_| ())
        .map_err(Error::from)
    }

    pub async fn remove(&self, tenant_id: i32, user_id: i32) -> Result<()> {
        Entity::delete_many()
            .filter(Column::TenantId.eq(tenant_id))
            .filter(Column::UserId.eq(user_id))
            .exec(&self.main)
            .await
            .map(|_| ())
            .map_err(Error::from)
    }

    /// Tenants that have a user whose username or email matches the
    /// login — the candidates credential-based sign-in verifies against.
    pub async fn tenants_matching(&self, login: &str) -> Result<Vec<i32>> {
        let needle = normalize(login);
        Entity::find()
            .select_only()
            .column(Column::TenantId)
            .filter(
                Column::NormalizedUserName
                    .eq(needle.clone())
                    .or(Column::NormalizedEmail.eq(needle)),
            )
            .distinct()
            .into_tuple::<i32>()
            .all(&self.main)
            .await
            .map_err(Error::from)
    }
}
