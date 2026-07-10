//! The user entity. Exhaustive on purpose — identity, credentials,
//! confirmation state, lockout, two-factor, preferences and lifecycle —
//! so user management never needs ad-hoc schema surgery.
//!
//! Sensitive columns (`password_hash`, `totp_secret`, `recovery_codes`,
//! tokens, `security_stamp`) must never be serialized to clients; expose
//! users through [`Profile`] instead.

use sea_orm::entity::prelude::*;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "users")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    /// `None` = host user (no tenant context).
    pub tenant_id: Option<Uuid>,

    pub user_name: String,
    pub normalized_user_name: String,
    pub email: String,
    pub normalized_email: String,
    pub email_confirmed: bool,
    pub email_confirmation_token: Option<String>,

    pub password_hash: String,
    pub password_changed_at: Option<DateTimeUtc>,
    pub password_reset_token: Option<String>,
    pub password_reset_expires_at: Option<DateTimeUtc>,
    /// Rotated whenever credentials change; embedded in tokens so a
    /// password change invalidates every outstanding session.
    pub security_stamp: String,

    pub first_name: String,
    pub last_name: String,
    pub phone_number: Option<String>,
    pub phone_number_confirmed: bool,

    pub is_active: bool,
    pub is_tenant_admin: bool,

    pub lockout_enabled: bool,
    pub lockout_end_at: Option<DateTimeUtc>,
    pub access_failed_count: i32,

    pub two_factor_enabled: bool,
    /// Base32 TOTP secret; present from setup, trusted once confirmed.
    pub totp_secret: Option<String>,
    pub totp_confirmed_at: Option<DateTimeUtc>,
    /// JSON array of SHA-256 hashes of unused one-time recovery codes.
    pub recovery_codes: Option<String>,

    pub last_login_at: Option<DateTimeUtc>,
    pub language: Option<String>,
    pub time_zone: Option<String>,

    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
    pub deleted_at: Option<DateTimeUtc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// The client-safe view of a user.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Profile {
    pub id: uuid::Uuid,
    pub tenant_id: Option<uuid::Uuid>,
    pub user_name: String,
    pub email: String,
    pub email_confirmed: bool,
    pub first_name: String,
    pub last_name: String,
    pub phone_number: Option<String>,
    pub phone_number_confirmed: bool,
    pub is_active: bool,
    pub is_tenant_admin: bool,
    pub two_factor_enabled: bool,
    pub last_login_at: Option<chrono::DateTime<chrono::Utc>>,
    pub language: Option<String>,
    pub time_zone: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl From<Model> for Profile {
    fn from(user: Model) -> Self {
        Self {
            id: user.id,
            tenant_id: user.tenant_id,
            user_name: user.user_name,
            email: user.email,
            email_confirmed: user.email_confirmed,
            first_name: user.first_name,
            last_name: user.last_name,
            phone_number: user.phone_number,
            phone_number_confirmed: user.phone_number_confirmed,
            is_active: user.is_active,
            is_tenant_admin: user.is_tenant_admin,
            two_factor_enabled: user.two_factor_enabled,
            last_login_at: user.last_login_at,
            language: user.language,
            time_zone: user.time_zone,
            created_at: user.created_at,
        }
    }
}
