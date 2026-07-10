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
//! - `GET /auth/me` — the authenticated profile;
//!   `GET /auth/me/permissions` — the caller's effective permissions
//!
//! Administration endpoints are guarded by permissions (see
//! [`super::permission::names`]); registration seeds the static `Admin`
//! role, which implicitly holds every permission, and assigns it to the
//! registering user:
//!
//! - `/auth/users` — create and list team members
//! - `/auth/users/{id}/admin` — grant or revoke the Admin role
//! - `/auth/users/{id}/roles` — set a user's roles
//! - `/auth/users/{id}/permissions` — per-user grant/deny overrides
//! - `/auth/roles` + `/auth/roles/{id}` — role CRUD with grants
//! - `GET /auth/permissions` — the permission definition tree
//!
//! Tenant context comes from the tenant resolution middleware: the same
//! `X-Tenant` header used everywhere else selects whose user store a
//! request talks to.

use super::authz::Authz;
use super::jwt::{self, CurrentUser, TokenPurpose, TwoFactorUser};
use super::manager::{NewUser, TwoFactorSetup, UserManager};
use super::permission::{self, PermissionDef, Registry};
use super::role_manager::RoleManager;
use super::user::{self, Profile};
use crate::audit::Audit;
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
        ctx.add_permissions(super::permission::administration_tree());
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
                .route("/auth/users/{id}/roles", axum::routing::put(set_user_roles))
                .route(
                    "/auth/users/{id}/permissions",
                    get(user_permissions).put(set_user_permissions),
                )
                .route("/auth/roles", post(create_role).get(list_roles))
                .route(
                    "/auth/roles/{id}",
                    axum::routing::put(update_role).delete(delete_role),
                )
                .route("/auth/permissions", get(permission_tree))
                .route("/auth/me", get(me))
                .route("/auth/me/permissions", get(my_permissions))
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
/// tenant's admin account, seeded with the static `Admin` role.
async fn register(
    State(state): State<AuthState>,
    Extension(registry): Extension<Arc<Registry>>,
    audit: Audit,
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
            let user = state
                .users(Some(db.clone()))
                .create(admin(Some(tenant.id)))
                .await?;
            seed_admin(&db, &registry, Some(tenant.id), user.id).await?;
            let profile: Profile = user.into();
            let recorder = audit.0.with_tenant(Some(tenant.id));
            recorder
                .created(
                    "tenant",
                    tenant.id,
                    &serde_json::json!({
                        "name": tenant.name,
                        "display_name": tenant.display_name,
                    }),
                )
                .await;
            recorder.created("user", profile.id, &profile).await;
            Ok(Json(RegisterResponse {
                tenant_id: Some(tenant.id),
                user: profile,
            }))
        }
        None => {
            let user = state.users(None).create(admin(None)).await?;
            seed_admin(&state.main_db, &registry, None, user.id).await?;
            let profile: Profile = user.into();
            audit.0.created("user", profile.id, &profile).await;
            Ok(Json(RegisterResponse {
                tenant_id: None,
                user: profile,
            }))
        }
    }
}

async fn seed_admin(
    db: &DatabaseConnection,
    registry: &Arc<Registry>,
    tenant_id: Option<i32>,
    user_id: i32,
) -> Result<()> {
    let roles = RoleManager::new(db.clone(), registry.clone());
    let admin_role = roles.ensure_admin_role(tenant_id).await?;
    roles.assign_role(user_id, admin_role.id).await
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
    audit: Audit,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>> {
    let tenant = tenant.map(|Extension(t)| t);
    let users = state.users(db.map(|Extension(d)| d));
    let user = match users
        .authenticate(tenant.as_ref().map(|t| t.id), &req.login, &req.password)
        .await
    {
        Ok(user) => user,
        Err(e) => {
            audit
                .0
                .event(format!("failed login attempt for {:?}", req.login))
                .await;
            return Err(e);
        }
    };

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
    audit
        .0
        .with_user(Some(user.id))
        .event(format!("{} logged in", user.user_name))
        .await;
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
    audit: Audit,
    Json(req): Json<TwoFactorLoginRequest>,
) -> Result<Json<LoginResponse>> {
    let users = state.users(db.map(|Extension(d)| d));
    let user = users.verify_two_factor(user, &req.code).await?;
    let token = jwt::issue(&state.config, &user, TokenPurpose::Access)?;
    let refresh_token = users.issue_refresh_token(user.id).await?;
    audit
        .0
        .with_user(Some(user.id))
        .event(format!("{} logged in with two-factor", user.user_name))
        .await;
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
    audit: Audit,
    Json(req): Json<ConfirmTwoFactorRequest>,
) -> Result<Json<ConfirmTwoFactorResponse>> {
    let user = setup_user(current, bridge).await?;
    let users = state.users(db.map(|Extension(d)| d));
    let (user, recovery_codes) = users.confirm_two_factor(user, &req.code).await?;
    audit
        .0
        .with_user(Some(user.id))
        .event(format!(
            "{} enabled two-factor authentication",
            user.user_name
        ))
        .await;
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
    audit: Audit,
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
    audit
        .0
        .with_user(Some(user.id))
        .event(format!(
            "{} disabled two-factor authentication",
            user.user_name
        ))
        .await;
    Ok(Json(user.into()))
}

#[derive(Deserialize, ToSchema)]
pub struct TenantTwoFactorRequest {
    pub required: bool,
}

/// Company-wide policy switch; requires the tenant-settings permission.
async fn tenant_two_factor(
    State(state): State<AuthState>,
    authz: Authz,
    audit: Audit,
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
    if authz.user.tenant_id != Some(tenant.id) {
        return Err(Error::Forbidden);
    }
    authz.require(permission::names::TENANT_SETTINGS).await?;
    let before = state.tenant_requires_2fa(Some(&tenant)).await?;
    let tenant = manager
        .set_require_two_factor(tenant.id, req.required)
        .await?;
    audit
        .0
        .updated(
            "tenant",
            tenant.id,
            &serde_json::json!({ "require_two_factor": before }),
            &serde_json::json!({ "require_two_factor": tenant.require_two_factor }),
        )
        .await;
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
    audit: Audit,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<serde_json::Value>> {
    let users = state.users(db.map(|Extension(d)| d));
    users.revoke_refresh_token(&req.refresh_token).await?;
    audit.0.event("a session logged out").await;
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
    audit: Audit,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<Json<Profile>> {
    let users = state.users(db.map(|Extension(d)| d));
    let user = users
        .change_password(user, &req.current_password, &req.new_password)
        .await?;
    audit
        .0
        .event(format!("{} changed their password", user.user_name))
        .await;
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

/// Team onboarding; requires the user-creation permission.
async fn create_user(
    State(state): State<AuthState>,
    authz: Authz,
    audit: Audit,
    db: Option<Extension<DatabaseConnection>>,
    Json(req): Json<CreateUserRequest>,
) -> Result<Json<Profile>> {
    authz.require(permission::names::USERS_CREATE).await?;
    let users = state.users(db.map(|Extension(d)| d));
    let user = users
        .create(NewUser {
            tenant_id: authz.user.tenant_id,
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
    let profile: Profile = user.into();
    audit.0.created("user", profile.id, &profile).await;
    Ok(Json(profile))
}

async fn list_users(
    State(state): State<AuthState>,
    authz: Authz,
    db: Option<Extension<DatabaseConnection>>,
) -> Result<Json<Vec<Profile>>> {
    authz.require(permission::names::USERS_VIEW).await?;
    let users = state.users(db.map(|Extension(d)| d));
    let all = users.find_all(authz.user.tenant_id).await?;
    Ok(Json(all.into_iter().map(Profile::from).collect()))
}

#[derive(Deserialize, ToSchema)]
pub struct SetAdminRequest {
    pub is_admin: bool,
}

/// Admin rights start with whoever registered the company; this grants
/// or revokes the static `Admin` role (which implicitly holds every
/// permission) for anyone later. Admins cannot demote themselves, so a
/// tenant can never lose its last admin by accident.
async fn set_user_admin(
    State(state): State<AuthState>,
    authz: Authz,
    audit: Audit,
    db: Option<Extension<DatabaseConnection>>,
    axum::extract::Path(user_id): axum::extract::Path<i32>,
    Json(req): Json<SetAdminRequest>,
) -> Result<Json<Profile>> {
    authz.require(permission::names::USERS_PERMISSIONS).await?;
    if authz.user.id == user_id && !req.is_admin {
        return Err(Error::Validation(
            "you cannot revoke your own admin rights; grant another admin first".into(),
        ));
    }
    let users = state.users(db.map(|Extension(d)| d));
    let target = users
        .find_by_id(user_id)
        .await?
        .filter(|u| u.tenant_id == authz.user.tenant_id)
        .ok_or_else(|| Error::NotFound(format!("user {user_id}")))?;
    let before: Profile = target.clone().into();
    let admin_role = authz
        .roles()
        .ensure_admin_role(authz.user.tenant_id)
        .await?;
    if req.is_admin {
        authz.roles().assign_role(target.id, admin_role.id).await?;
    } else {
        authz
            .roles()
            .unassign_role(target.id, admin_role.id)
            .await?;
    }
    let user = users.set_tenant_admin(target, req.is_admin).await?;
    let after: Profile = user.into();
    audit.0.updated("user", after.id, &before, &after).await;
    Ok(Json(after))
}

#[derive(Deserialize, ToSchema)]
pub struct SetUserRolesRequest {
    pub role_ids: Vec<i32>,
}

#[derive(Serialize, ToSchema)]
pub struct RoleResponse {
    pub id: i32,
    pub name: String,
    pub display_name: String,
    pub is_static: bool,
    /// Explicit grants; static roles implicitly hold every permission.
    pub permissions: Vec<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateRoleRequest {
    pub name: String,
    pub display_name: String,
    #[serde(default)]
    pub permissions: Vec<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateRoleRequest {
    pub display_name: Option<String>,
    pub permissions: Option<Vec<String>>,
}

#[derive(Deserialize, ToSchema)]
pub struct SetUserPermissionsRequest {
    /// Permissions granted beyond what the user's roles give.
    #[serde(default)]
    pub granted: Vec<String>,
    /// Permissions denied even when a role grants them.
    #[serde(default)]
    pub denied: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct UserPermissionsResponse {
    pub roles: Vec<RoleResponse>,
    pub granted: Vec<String>,
    pub denied: Vec<String>,
    /// Fully resolved: roles unioned, overrides applied, deny wins.
    pub effective: Vec<String>,
}

/// The full permission definition tree, for admin UIs.
async fn permission_tree(authz: Authz) -> Result<Json<Vec<PermissionDef>>> {
    authz.require(permission::names::ROLES_VIEW).await?;
    Ok(Json(authz.registry().tree().to_vec()))
}

/// The caller's own effective permissions — any authenticated user.
async fn my_permissions(authz: Authz) -> Result<Json<Vec<String>>> {
    let mut names: Vec<String> = authz.granted_permissions().await?.into_iter().collect();
    names.sort();
    Ok(Json(names))
}

async fn role_response(roles: &RoleManager, role: super::role::Model) -> Result<RoleResponse> {
    let permissions = roles.role_permissions(role.id).await?;
    Ok(RoleResponse {
        id: role.id,
        name: role.name,
        display_name: role.display_name,
        is_static: role.is_static,
        permissions,
    })
}

/// A role in the caller's tenant, or 404 — one tenant must never see or
/// touch another tenant's roles.
async fn tenant_role(authz: &Authz, role_id: i32) -> Result<super::role::Model> {
    let role = authz.roles().find_by_id(role_id).await?;
    if role.tenant_id != authz.user.tenant_id {
        return Err(Error::NotFound("role".into()));
    }
    Ok(role)
}

async fn list_roles(authz: Authz) -> Result<Json<Vec<RoleResponse>>> {
    authz.require(permission::names::ROLES_VIEW).await?;
    let mut out = Vec::new();
    for role in authz.roles().find_all(authz.user.tenant_id).await? {
        out.push(role_response(authz.roles(), role).await?);
    }
    Ok(Json(out))
}

async fn create_role(
    authz: Authz,
    audit: Audit,
    Json(req): Json<CreateRoleRequest>,
) -> Result<Json<RoleResponse>> {
    authz.require(permission::names::ROLES_CREATE).await?;
    let role = authz
        .roles()
        .create_role(
            authz.user.tenant_id,
            &req.name,
            &req.display_name,
            &req.permissions,
        )
        .await?;
    let response = role_response(authz.roles(), role).await?;
    audit.0.created("role", response.id, &response).await;
    Ok(Json(response))
}

async fn update_role(
    authz: Authz,
    audit: Audit,
    axum::extract::Path(role_id): axum::extract::Path<i32>,
    Json(req): Json<UpdateRoleRequest>,
) -> Result<Json<RoleResponse>> {
    authz.require(permission::names::ROLES_EDIT).await?;
    let mut role = tenant_role(&authz, role_id).await?;
    let before = role_response(authz.roles(), role.clone()).await?;
    if let Some(display_name) = &req.display_name {
        role = authz.roles().update_role(role.id, display_name).await?;
    }
    if let Some(permissions) = &req.permissions {
        authz
            .roles()
            .set_role_permissions(role.id, permissions)
            .await?;
    }
    let after = role_response(authz.roles(), role).await?;
    audit.0.updated("role", after.id, &before, &after).await;
    Ok(Json(after))
}

async fn delete_role(
    authz: Authz,
    audit: Audit,
    axum::extract::Path(role_id): axum::extract::Path<i32>,
) -> Result<Json<serde_json::Value>> {
    authz.require(permission::names::ROLES_DELETE).await?;
    let role = tenant_role(&authz, role_id).await?;
    let before = role_response(authz.roles(), role.clone()).await?;
    authz.roles().delete_role(role.id).await?;
    audit.0.deleted("role", before.id, &before).await;
    Ok(Json(serde_json::json!({ "status": "deleted" })))
}

/// Replace a user's role set. Changing your own roles is refused — an
/// admin locking themselves out is never one call away.
async fn set_user_roles(
    State(state): State<AuthState>,
    authz: Authz,
    audit: Audit,
    db: Option<Extension<DatabaseConnection>>,
    axum::extract::Path(user_id): axum::extract::Path<i32>,
    Json(req): Json<SetUserRolesRequest>,
) -> Result<Json<Vec<RoleResponse>>> {
    authz.require(permission::names::USERS_PERMISSIONS).await?;
    if authz.user.id == user_id {
        return Err(Error::Validation("you cannot change your own roles".into()));
    }
    let target = tenant_user(&state, &authz, db, user_id).await?;
    for role_id in &req.role_ids {
        tenant_role(&authz, *role_id).await?;
    }
    let current = authz.roles().roles_of(target.id).await?;
    for role in &current {
        if !req.role_ids.contains(&role.id) {
            authz.roles().unassign_role(target.id, role.id).await?;
        }
    }
    for role_id in &req.role_ids {
        authz.roles().assign_role(target.id, *role_id).await?;
    }
    let mut out = Vec::new();
    for role in authz.roles().roles_of(target.id).await? {
        out.push(role_response(authz.roles(), role).await?);
    }
    let role_names =
        |roles: &[super::role::Model]| roles.iter().map(|r| r.name.clone()).collect::<Vec<_>>();
    audit
        .0
        .updated(
            "user",
            target.id,
            &serde_json::json!({ "roles": role_names(&current) }),
            &serde_json::json!({
                "roles": out.iter().map(|r| r.name.clone()).collect::<Vec<_>>(),
            }),
        )
        .await;
    Ok(Json(out))
}

async fn user_permissions(
    State(state): State<AuthState>,
    authz: Authz,
    db: Option<Extension<DatabaseConnection>>,
    axum::extract::Path(user_id): axum::extract::Path<i32>,
) -> Result<Json<UserPermissionsResponse>> {
    authz.require(permission::names::USERS_PERMISSIONS).await?;
    let target = tenant_user(&state, &authz, db, user_id).await?;
    user_permissions_response(&authz, target.id).await.map(Json)
}

/// Replace a user's per-user overrides. Like roles, never on yourself.
async fn set_user_permissions(
    State(state): State<AuthState>,
    authz: Authz,
    audit: Audit,
    db: Option<Extension<DatabaseConnection>>,
    axum::extract::Path(user_id): axum::extract::Path<i32>,
    Json(req): Json<SetUserPermissionsRequest>,
) -> Result<Json<UserPermissionsResponse>> {
    authz.require(permission::names::USERS_PERMISSIONS).await?;
    if authz.user.id == user_id {
        return Err(Error::Validation(
            "you cannot change your own permissions".into(),
        ));
    }
    let target = tenant_user(&state, &authz, db, user_id).await?;
    let before = user_permissions_response(&authz, target.id).await?;
    authz
        .roles()
        .set_user_permissions(target.id, &req.granted, &req.denied)
        .await?;
    let after = user_permissions_response(&authz, target.id).await?;
    audit
        .0
        .updated(
            "user",
            target.id,
            &serde_json::json!({ "granted": before.granted, "denied": before.denied }),
            &serde_json::json!({ "granted": after.granted, "denied": after.denied }),
        )
        .await;
    Ok(Json(after))
}

/// A user in the caller's tenant, or 404.
async fn tenant_user(
    state: &AuthState,
    authz: &Authz,
    db: Option<Extension<DatabaseConnection>>,
    user_id: i32,
) -> Result<user::Model> {
    state
        .users(db.map(|Extension(d)| d))
        .find_by_id(user_id)
        .await?
        .filter(|u| u.tenant_id == authz.user.tenant_id)
        .ok_or_else(|| Error::NotFound(format!("user {user_id}")))
}

async fn user_permissions_response(authz: &Authz, user_id: i32) -> Result<UserPermissionsResponse> {
    let mut roles = Vec::new();
    for role in authz.roles().roles_of(user_id).await? {
        roles.push(role_response(authz.roles(), role).await?);
    }
    let (mut granted, mut denied) = (Vec::new(), Vec::new());
    for row in authz.roles().user_overrides(user_id).await? {
        if row.is_granted {
            granted.push(row.permission);
        } else {
            denied.push(row.permission);
        }
    }
    granted.sort();
    denied.sort();
    let mut effective: Vec<String> = authz
        .roles()
        .granted_permissions(user_id)
        .await?
        .into_iter()
        .collect();
    effective.sort();
    Ok(UserPermissionsResponse {
        roles,
        granted,
        denied,
        effective,
    })
}
