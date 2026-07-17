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
    /// The company's currency, a code from the `currencies` table.
    pub default_currency: Option<String>,
    /// Tax registration PIN (e.g. a KRA PIN).
    pub tax_pin: Option<String>,
    pub vat_number: Option<String>,
    /// Storage path of the uploaded company logo, relative to the
    /// public file root (`{tenant-id}/logo.png`).
    pub logo_path: Option<String>,
    /// Postal/street address, shown on report chrome and the profile.
    pub address: Option<String>,
    /// Contact email, shown on report chrome and the profile.
    pub email: Option<String>,
    /// Company website, shown on report chrome and the profile.
    pub website: Option<String>,
    /// Contact phone number, shown on report chrome and the profile.
    pub phone: Option<String>,

    // The company's password policy. Null means "use the deployment
    // default" from `auth.*`; see `crate::auth::policy::PasswordPolicy`,
    // which resolves the two into the rules actually enforced.
    pub password_min_length: Option<i32>,
    pub password_require_uppercase: Option<bool>,
    pub password_require_lowercase: Option<bool>,
    pub password_require_digit: Option<bool>,
    pub password_require_symbol: Option<bool>,
    pub password_expiry_days: Option<i32>,
    pub password_history_count: Option<i32>,
    pub lockout_max_failed: Option<i32>,
    pub lockout_secs: Option<i32>,

    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
