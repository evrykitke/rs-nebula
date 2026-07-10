//! The tenant directory entity, stored in the main database.
//! `connection_string` empty/null means the tenant shares the main
//! database; otherwise it points at the tenant's own database.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "tenants")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    #[sea_orm(unique)]
    pub name: String,
    pub display_name: String,
    pub connection_string: Option<String>,
    pub is_active: bool,
    pub require_two_factor: bool,
    /// Override of `audit.retention_days`, capped by
    /// `audit.retention_max_days`; null uses the system default.
    pub audit_retention_days: Option<i32>,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
