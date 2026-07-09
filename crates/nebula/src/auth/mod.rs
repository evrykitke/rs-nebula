//! Authentication and user management.
//!
//! - [`user`] — the exhaustive user entity ([`user::Profile`] is the
//!   client-safe view)
//! - [`UserManager`] — lifecycle, password auth with lockout, TOTP
//!   two-factor with recovery codes
//! - [`totp`] — RFC 6238 codes for authenticator apps
//! - [`jwt`] — signed access / two-factor tokens
//! - [`AuthModule`] — ready-made HTTP endpoints: company registration,
//!   login with two-factor challenge and setup flows, profile
//!
//! Two-factor policy: a company (tenant) can require it for everyone
//! (`tenants.require_two_factor` — users must set up an authenticator
//! before signing in), and any user can opt in from their profile.

pub mod jwt;
pub mod manager;
pub mod module;
pub mod password;
pub mod totp;
pub mod user;

pub use jwt::{Claims, CurrentUser, TokenPurpose};
pub use manager::{NewUser, TwoFactorSetup, UserManager};
pub use module::AuthModule;
