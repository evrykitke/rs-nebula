//! Role entity plus the user-role assignment and permission-grant rows.
//! A grant row belongs to exactly one of a role or a user; user rows
//! carry `is_granted = false` to express a deny that beats role grants.

use sea_orm::entity::prelude::*;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, utoipa::ToSchema)]
#[sea_orm(table_name = "roles")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub tenant_id: Option<Uuid>,
    pub name: String,
    pub display_name: String,
    /// Static roles are framework-managed (e.g. `Admin`) and cannot be
    /// deleted; the static Admin role implicitly holds every permission.
    pub is_static: bool,
    #[serde(skip)]
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

pub mod user_role {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "user_roles")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub user_id: Uuid,
        #[sea_orm(primary_key, auto_increment = false)]
        pub role_id: Uuid,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod permission_grant {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "permission_grants")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub permission: String,
        /// Exactly one of `role_id` / `user_id` is set.
        pub role_id: Option<Uuid>,
        pub user_id: Option<Uuid>,
        /// `false` only makes sense on user rows: an explicit deny that
        /// overrides whatever the user's roles grant.
        pub is_granted: bool,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}
