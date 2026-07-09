//! Ready-made authentication endpoints. Add `AuthModule` to the kernel
//! and every application gets:
//!
//! - `POST /auth/register` — a company registers: creates the tenant and
//!   its admin account from the email + password given at registration
//!   (multitenant mode; in single-tenant mode it creates a host user)
//! - `POST /auth/login` — password step. Answers `success` with an
//!   access token, `two_factor_required` (user has an authenticator) or
//!   `two_factor_setup_required` (company mandates 2FA but the user has
//!   not set it up), the latter two with a short-lived bridge token
//! - `POST /auth/login/two-factor` — completes login with an
//!   authenticator or recovery code
//! - `POST /auth/two-factor/setup` + `/confirm` — enable an authenticator
//!   (from the profile, or during mandated setup with a bridge token);
//!   confirm returns the one-time recovery codes
//! - `POST /auth/two-factor/disable` — opt out (password required;
//!   refused while the company mandates 2FA)
//! - `POST /auth/tenant/two-factor` — tenant admin toggles the
//!   company-wide 2FA mandate
//! - `GET /auth/me` — the authenticated profile
//!
//! Tenant context comes from the tenant resolution middleware: the same
//! `X-Tenant` header used everywhere else selects whose user store a
//! request talks to.

use super::jwt::{self, CurrentUser, TokenPurpose, TwoFactorUser};
use super::manager::{NewUser, TwoFactorSetup, UserManager};
use super::user::{self, Profile};
use crate::config::AuthConfig;
use crate::error::{Error, Result};
use crate::module::{Module, ModuleContext};
use crate::tenancy::{NewTenant, TenantManager, TenantRef};
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::ToSchema;

pub struct AuthModule;

#[derive(Clone)]
struct AuthState {
    config: AuthConfig,
    main_db: DatabaseConnection,
    tenants: Option<Arc<TenantManager>>,
}

impl Module for AuthModule {
    fn name(&self) -> &'static str {
        "auth"
    }

    fn configure(&self, ctx: &mut ModuleContext) {
        let config = ctx.config().auth.clone();
        assert!(
            !config.jwt_secret.is_empty(),
            "AuthModule requires auth.jwt_secret; set it in {{env}}.local.yaml \
             or NEBULA__AUTH__JWT_SECRET"
        );
        let state = AuthState {
            config: config.clone(),
            main_db: ctx.require_db(),
            tenants: ctx.tenants(),
        };
        ctx.add_routes(
            Router::new()
                .route("/auth/register", post(register))
                .route("/auth/login", post(login))
                .route("/auth/login/two-factor", post(login_two_factor))
                .route("/auth/two-factor/setup", post(two_factor_setup))
                .route("/auth/two-factor/confirm", post(two_factor_confirm))
                .route("/auth/two-factor/disable", post(two_factor_disable))
                .route("/auth/tenant/two-factor", post(tenant_two_factor))
                .route("/auth/me", get(me))
                .with_state(state),
        );
    }
}

impl AuthState {
    /// The user store for the current request's context.
    fn users(&self, db: Option<DatabaseConnection>) -> UserManager {
        UserManager::new(
            db.unwrap_or_else(|| self.main_db.clone()),
            self.config.clone(),
        )
    }

    async fn tenant_requires_2fa(&self, tenant: Option<&TenantRef>) -> Result<bool> {
        let (Some(manager), Some(tenant)) = (&self.tenants, tenant) else {
            return Ok(false);
        };
        Ok(manager
            .find_by_id(tenant.id)
            .await?
            .is_some_and(|t| t.require_two_factor))
    }
}

#[derive(Deserialize, ToSchema)]
pub struct RegisterRequest {
    /// Tenant (company) name: lowercase letters, digits, dashes.
    /// Ignored in single-tenant mode.
    pub tenant_name: Option<String>,
    pub company_display_name: Option<String>,
    pub email: String,
    pub password: String,
    pub first_name: String,
    pub last_name: String,
}

#[derive(Serialize, ToSchema)]
pub struct RegisterResponse {
    pub tenant_id: Option<i32>,
    pub user: Profile,
}

/// Company registration: the email and password provided here become the
/// tenant's admin account.
async fn register(
    State(state): State<AuthState>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>> {
    let admin = |tenant_id| NewUser {
        tenant_id,
        user_name: req.email.clone(),
        email: req.email.clone(),
        password: req.password.clone(),
        first_name: req.first_name.clone(),
        last_name: req.last_name.clone(),
        is_tenant_admin: true,
        language: None,
        time_zone: None,
        phone_number: None,
    };

    match &state.tenants {
        Some(manager) => {
            let name = req
                .tenant_name
                .clone()
                .ok_or_else(|| Error::Validation("tenant_name is required".into()))?;
            let tenant = manager
                .create(NewTenant {
                    display_name: req
                        .company_display_name
                        .clone()
                        .unwrap_or_else(|| name.clone()),
                    name,
                    connection_string: None,
                })
                .await?;
            let db = manager.connection_for(&tenant).await?;
            let user = state.users(Some(db)).create(admin(Some(tenant.id))).await?;
            Ok(Json(RegisterResponse {
                tenant_id: Some(tenant.id),
                user: user.into(),
            }))
        }
        None => {
            let user = state.users(None).create(admin(None)).await?;
            Ok(Json(RegisterResponse {
                tenant_id: None,
                user: user.into(),
            }))
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub struct LoginRequest {
    /// Username or email.
    pub login: String,
    pub password: String,
}

#[derive(Serialize, ToSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LoginResponse {
    Success {
        access_token: String,
        user: Profile,
    },
    /// Password accepted; finish with `POST /auth/login/two-factor`.
    TwoFactorRequired {
        two_factor_token: String,
    },
    /// The company mandates 2FA and this account has none yet: use the
    /// token on the setup + confirm endpoints, then log in again.
    TwoFactorSetupRequired {
        two_factor_token: String,
    },
}

async fn login(
    State(state): State<AuthState>,
    tenant: Option<Extension<TenantRef>>,
    db: Option<Extension<DatabaseConnection>>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>> {
    let tenant = tenant.map(|Extension(t)| t);
    let users = state.users(db.map(|Extension(d)| d));
    let user = users
        .authenticate(tenant.as_ref().map(|t| t.id), &req.login, &req.password)
        .await?;

    if user.two_factor_enabled && user.totp_confirmed_at.is_some() {
        let token = jwt::issue(&state.config, &user, TokenPurpose::TwoFactor)?;
        return Ok(Json(LoginResponse::TwoFactorRequired {
            two_factor_token: token,
        }));
    }
    if state.tenant_requires_2fa(tenant.as_ref()).await? {
        let token = jwt::issue(&state.config, &user, TokenPurpose::TwoFactor)?;
        return Ok(Json(LoginResponse::TwoFactorSetupRequired {
            two_factor_token: token,
        }));
    }

    let token = jwt::issue(&state.config, &user, TokenPurpose::Access)?;
    Ok(Json(LoginResponse::Success {
        access_token: token,
        user: user.into(),
    }))
}

#[derive(Deserialize, ToSchema)]
pub struct TwoFactorLoginRequest {
    /// Authenticator code or a recovery code.
    pub code: String,
}

async fn login_two_factor(
    State(state): State<AuthState>,
    TwoFactorUser(user): TwoFactorUser,
    db: Option<Extension<DatabaseConnection>>,
    Json(req): Json<TwoFactorLoginRequest>,
) -> Result<Json<LoginResponse>> {
    let users = state.users(db.map(|Extension(d)| d));
    let user = users.verify_two_factor(user, &req.code).await?;
    let token = jwt::issue(&state.config, &user, TokenPurpose::Access)?;
    Ok(Json(LoginResponse::Success {
        access_token: token,
        user: user.into(),
    }))
}

/// Accepts either a full access token (profile opt-in) or a two-factor
/// bridge token (company-mandated setup during login).
async fn setup_user(
    parts_user: Result<CurrentUser, axum::response::Response>,
    bridge: Result<TwoFactorUser, axum::response::Response>,
) -> Result<user::Model> {
    if let Ok(CurrentUser(user)) = parts_user {
        return Ok(user);
    }
    if let Ok(TwoFactorUser(user)) = bridge {
        return Ok(user);
    }
    Err(Error::Unauthorized)
}

async fn two_factor_setup(
    State(state): State<AuthState>,
    current: Result<CurrentUser, axum::response::Response>,
    bridge: Result<TwoFactorUser, axum::response::Response>,
    db: Option<Extension<DatabaseConnection>>,
) -> Result<Json<TwoFactorSetup>> {
    let user = setup_user(current, bridge).await?;
    let users = state.users(db.map(|Extension(d)| d));
    let (_, setup) = users.begin_two_factor_setup(user).await?;
    Ok(Json(setup))
}

#[derive(Deserialize, ToSchema)]
pub struct ConfirmTwoFactorRequest {
    pub code: String,
}

#[derive(Serialize, ToSchema)]
pub struct ConfirmTwoFactorResponse {
    /// Shown exactly once — store them somewhere safe.
    pub recovery_codes: Vec<String>,
}

async fn two_factor_confirm(
    State(state): State<AuthState>,
    current: Result<CurrentUser, axum::response::Response>,
    bridge: Result<TwoFactorUser, axum::response::Response>,
    db: Option<Extension<DatabaseConnection>>,
    Json(req): Json<ConfirmTwoFactorRequest>,
) -> Result<Json<ConfirmTwoFactorResponse>> {
    let user = setup_user(current, bridge).await?;
    let users = state.users(db.map(|Extension(d)| d));
    let (_, recovery_codes) = users.confirm_two_factor(user, &req.code).await?;
    Ok(Json(ConfirmTwoFactorResponse { recovery_codes }))
}

#[derive(Deserialize, ToSchema)]
pub struct DisableTwoFactorRequest {
    pub password: String,
}

async fn two_factor_disable(
    State(state): State<AuthState>,
    CurrentUser(user): CurrentUser,
    tenant: Option<Extension<TenantRef>>,
    db: Option<Extension<DatabaseConnection>>,
    Json(req): Json<DisableTwoFactorRequest>,
) -> Result<Json<Profile>> {
    if !super::password::verify(&req.password, &user.password_hash) {
        return Err(Error::Unauthorized);
    }
    let tenant = tenant.map(|Extension(t)| t);
    if state.tenant_requires_2fa(tenant.as_ref()).await? {
        return Err(Error::Validation(
            "two-factor authentication is required by your company".into(),
        ));
    }
    let users = state.users(db.map(|Extension(d)| d));
    let user = users.disable_two_factor(user).await?;
    Ok(Json(user.into()))
}

#[derive(Deserialize, ToSchema)]
pub struct TenantTwoFactorRequest {
    pub required: bool,
}

/// Company-wide policy switch; tenant admins only.
async fn tenant_two_factor(
    State(state): State<AuthState>,
    CurrentUser(user): CurrentUser,
    tenant: Option<Extension<TenantRef>>,
    Json(req): Json<TenantTwoFactorRequest>,
) -> Result<Json<serde_json::Value>> {
    let Some(manager) = &state.tenants else {
        return Err(Error::Validation(
            "multitenancy is not enabled on this deployment".into(),
        ));
    };
    let Some(Extension(tenant)) = tenant else {
        return Err(Error::Validation("a tenant context is required".into()));
    };
    if !user.is_tenant_admin || user.tenant_id != Some(tenant.id) {
        return Err(Error::Forbidden);
    }
    let tenant = manager
        .set_require_two_factor(tenant.id, req.required)
        .await?;
    Ok(Json(serde_json::json!({
        "tenant": tenant.name,
        "require_two_factor": tenant.require_two_factor,
    })))
}

async fn me(CurrentUser(user): CurrentUser) -> Json<Profile> {
    Json(user.into())
}
