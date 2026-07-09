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
                .route("/auth/token/refresh", post(refresh))
                .route("/auth/logout", post(logout))
                .route("/auth/password", post(change_password))
                .route("/auth/users", post(create_user).get(list_users))
                .route("/auth/users/{id}/admin", axum::routing::put(set_user_admin))
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
        /// Long-lived, single-use: exchange it at `POST /auth/token/refresh`
        /// for a fresh pair.
        refresh_token: String,
        user: Profile,
    },
    /// Password accepted; finish with `POST /auth/login/two-factor`.
    TwoFactorRequired { two_factor_token: String },
    /// The company mandates 2FA and this account has none yet: use the
    /// token on the setup + confirm endpoints, then log in again.
    TwoFactorSetupRequired { two_factor_token: String },
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
    let refresh_token = users.issue_refresh_token(user.id).await?;
    Ok(Json(LoginResponse::Success {
        access_token: token,
        refresh_token,
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
    let refresh_token = users.issue_refresh_token(user.id).await?;
    Ok(Json(LoginResponse::Success {
        access_token: token,
        refresh_token,
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

#[derive(Deserialize, ToSchema)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

/// Exchange a refresh token for a fresh access/refresh pair. The old
/// token is revoked; reusing it later revokes every session of the user.
async fn refresh(
    State(state): State<AuthState>,
    db: Option<Extension<DatabaseConnection>>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<LoginResponse>> {
    let users = state.users(db.map(|Extension(d)| d));
    let (user, refresh_token) = users.rotate_refresh_token(&req.refresh_token).await?;
    let access_token = jwt::issue(&state.config, &user, TokenPurpose::Access)?;
    Ok(Json(LoginResponse::Success {
        access_token,
        refresh_token,
        user: user.into(),
    }))
}

async fn logout(
    State(state): State<AuthState>,
    db: Option<Extension<DatabaseConnection>>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<serde_json::Value>> {
    let users = state.users(db.map(|Extension(d)| d));
    users.revoke_refresh_token(&req.refresh_token).await?;
    Ok(Json(serde_json::json!({ "status": "logged_out" })))
}

#[derive(Deserialize, ToSchema)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

/// Changing the password rotates the security stamp and revokes all
/// refresh tokens — every other session has to sign in again.
async fn change_password(
    State(state): State<AuthState>,
    CurrentUser(user): CurrentUser,
    db: Option<Extension<DatabaseConnection>>,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<Json<Profile>> {
    let users = state.users(db.map(|Extension(d)| d));
    let user = users
        .change_password(user, &req.current_password, &req.new_password)
        .await?;
    Ok(Json(user.into()))
}

#[derive(Deserialize, ToSchema)]
pub struct CreateUserRequest {
    pub user_name: String,
    pub email: String,
    pub password: String,
    pub first_name: String,
    pub last_name: String,
    #[serde(default)]
    pub is_tenant_admin: bool,
    pub phone_number: Option<String>,
    pub language: Option<String>,
    pub time_zone: Option<String>,
}

/// Tenant admins onboard their team here.
async fn create_user(
    State(state): State<AuthState>,
    CurrentUser(admin): CurrentUser,
    db: Option<Extension<DatabaseConnection>>,
    Json(req): Json<CreateUserRequest>,
) -> Result<Json<Profile>> {
    if !admin.is_tenant_admin {
        return Err(Error::Forbidden);
    }
    let users = state.users(db.map(|Extension(d)| d));
    let user = users
        .create(NewUser {
            tenant_id: admin.tenant_id,
            user_name: req.user_name,
            email: req.email,
            password: req.password,
            first_name: req.first_name,
            last_name: req.last_name,
            is_tenant_admin: req.is_tenant_admin,
            language: req.language,
            time_zone: req.time_zone,
            phone_number: req.phone_number,
        })
        .await?;
    Ok(Json(user.into()))
}

async fn list_users(
    State(state): State<AuthState>,
    CurrentUser(admin): CurrentUser,
    db: Option<Extension<DatabaseConnection>>,
) -> Result<Json<Vec<Profile>>> {
    if !admin.is_tenant_admin {
        return Err(Error::Forbidden);
    }
    let users = state.users(db.map(|Extension(d)| d));
    let all = users.find_all(admin.tenant_id).await?;
    Ok(Json(all.into_iter().map(Profile::from).collect()))
}

#[derive(Deserialize, ToSchema)]
pub struct SetAdminRequest {
    pub is_admin: bool,
}

/// Admin rights start with whoever registered the company; this is how
/// they are granted to or revoked from anyone later. Admins cannot
/// demote themselves, so a tenant can never lose its last admin by
/// accident.
async fn set_user_admin(
    State(state): State<AuthState>,
    CurrentUser(admin): CurrentUser,
    db: Option<Extension<DatabaseConnection>>,
    axum::extract::Path(user_id): axum::extract::Path<i32>,
    Json(req): Json<SetAdminRequest>,
) -> Result<Json<Profile>> {
    if !admin.is_tenant_admin {
        return Err(Error::Forbidden);
    }
    if admin.id == user_id && !req.is_admin {
        return Err(Error::Validation(
            "you cannot revoke your own admin rights; grant another admin first".into(),
        ));
    }
    let users = state.users(db.map(|Extension(d)| d));
    let target = users
        .find_by_id(user_id)
        .await?
        .filter(|u| u.tenant_id == admin.tenant_id)
        .ok_or_else(|| Error::NotFound(format!("user {user_id}")))?;
    let user = users.set_tenant_admin(target, req.is_admin).await?;
    Ok(Json(user.into()))
}
