//! A tenant's SMTP server, stored in the main database beside the tenant
//! directory. `password_encrypted` is AES-GCM ciphertext, never the
//! password — see [`crate::crypto`].

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "tenant_mail_settings")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    pub host: String,
    pub port: i32,
    pub username: Option<String>,
    pub password_encrypted: Option<String>,
    /// `none`, `starttls` or `tls` — see [`super::Encryption`].
    pub encryption: String,
    pub from_address: String,
    pub from_name: Option<String>,
    /// Off keeps the settings but refuses to send, so an admin can stop
    /// outbound mail without losing the configuration.
    pub enabled: bool,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
