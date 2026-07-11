//! Authentication and user management — the domain services the
//! [`crate::account`] and [`crate::administration`] modules expose over
//! HTTP.
//!
//! - [`user`] — the exhaustive user entity ([`user::Profile`] is the
//!   client-safe view)
//! - [`UserManager`] — lifecycle, password auth with lockout, TOTP
//!   two-factor with recovery codes
//! - [`totp`] — RFC 6238 codes for authenticator apps
//! - [`jwt`] — signed access / two-factor tokens
//!
//! Two-factor policy: a company (tenant) can require it for everyone
//! (`tenants.require_two_factor` — users must set up an authenticator
//! before signing in), and any user can opt in from their profile.

pub mod authz;
pub mod directory;
pub mod jwt;
pub mod manager;
pub mod password;
pub mod permission;
pub mod refresh_token;
pub mod role;
pub mod role_manager;
pub(crate) mod state;
pub mod totp;
pub mod user;

pub use authz::Authz;
pub use jwt::{Claims, CurrentUser, TokenPurpose};
pub use manager::{NewUser, TwoFactorSetup, UserManager};
pub use permission::PermissionDef;
pub use role_manager::RoleManager;
