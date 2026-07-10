//! Ready-made authentication endpoints. Add `AuthModule` to the kernel
//! and every application gets:
//!
//! - `POST /auth/register` — a company registers: creates the tenant and
//!   its admin account from the email + password given at registration
//!   (multitenant mode; in single-tenant mode it creates a host user)
//! - `POST /auth/login` — password step, no tenant header required: the
//!   tenant is resolved from the credentials via the login directory
//!   (`tenant_selection` answers an ambiguous login; retry with the
//!   header). Answers `success` with an access token,
//!   `two_factor_required` (user has an authenticator) or
//!   `two_factor_setup_required` (company mandates 2FA but the user has
//!   not set it up), the latter two with a short-lived bridge token;
//!   every answer names the resolved tenant for subsequent headers
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
use uuid::Uuid;
use utoipa::ToSchema;

pub struct AuthModule;

#[derive(Clone)]
struct AuthState {
    config: AuthConfig,
    files: crate::config::FilesConfig,
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
            "AuthModule requires auth.jwt_secret; set it in \
             config/{{env}}.local.yaml or NEBULA__AUTH__JWT_SECRET"
        );
        let state = AuthState {
            config: config.clone(),
            files: ctx.config().files.clone(),
            main_db: ctx.require_db(),
            tenants: ctx.tenants(),
        };
        ctx.add_permissions(super::permission::administration_tree());
        ctx.add_api(crate::module::build_openapi(|| {
            <AccountApiDoc as utoipa::OpenApi>::openapi()
        }));
        ctx.add_api(crate::module::build_openapi(|| {
            <AdminApiDoc as utoipa::OpenApi>::openapi()
        }));
        ctx.add_routes(
            Router::new()
                .route("/auth/register", post(register))
                .route("/auth/login", post(login))
                .route("/auth/login/two-factor", post(login_two_factor))
                .route("/auth/two-factor/setup", post(two_factor_setup))
                .route("/auth/two-factor/confirm", post(two_factor_confirm))
                .route("/auth/two-factor/disable", post(two_factor_disable))
                .route(
                    "/auth/tenant/two-factor",
                    post(tenant_two_factor).get(tenant_two_factor_get),
                )
                .route(
                    "/auth/tenant/profile",
                    get(tenant_profile_get).put(tenant_profile_update),
                )
                .route("/auth/tenant/logo", post(tenant_logo_upload))
                .route("/auth/tenant/migrate", post(tenant_migrate))
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

/// The auth module's OpenAPI contribution — the source client generators
/// (NSwag) build the `auth` service proxy from. Split into two documents
/// merged at boot: the derive expands each into one giant expression, and
/// keeping them moderate keeps unoptimized builds within stack limits.
#[derive(utoipa::OpenApi)]
#[openapi(paths(
    register,
    login,
    login_two_factor,
    two_factor_setup,
    two_factor_confirm,
    two_factor_disable,
    refresh,
    logout,
    change_password,
    me,
    my_permissions,
))]
struct AccountApiDoc;

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    tenant_two_factor,
    tenant_two_factor_get,
    tenant_profile_get,
    tenant_profile_update,
    tenant_logo_upload,
    tenant_migrate,
    create_user,
    list_users,
    set_user_admin,
    set_user_roles,
    user_permissions,
    set_user_permissions,
    create_role,
    list_roles,
    update_role,
    delete_role,
    permission_tree,
))]
struct AdminApiDoc;

impl AuthState {
    /// The user store for the current request's context. In multitenant
    /// mode it keeps the main-database login directory in sync so
    /// credential-based sign-in can resolve the tenant.
    fn users(&self, db: Option<DatabaseConnection>) -> UserManager {
        let users = UserManager::new(
            db.unwrap_or_else(|| self.main_db.clone()),
            self.config.clone(),
        );
        match &self.tenants {
            Some(_) => users.with_directory(self.main_db.clone()),
            None => users,
        }
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

    /// The caller's own tenant, for the tenant-scoped settings
    /// endpoints: requires multitenancy, a tenant context, and that the
    /// authenticated user belongs to it.
    fn tenant_context(
        &self,
        authz: &Authz,
        tenant: Option<Extension<TenantRef>>,
    ) -> Result<(Arc<TenantManager>, TenantRef)> {
        let manager = self.tenants.clone().ok_or_else(|| {
            Error::Validation("multitenancy is not enabled on this deployment".into())
        })?;
        let Some(Extension(tenant)) = tenant else {
            return Err(Error::Validation("a tenant context is required".into()));
        };
        if authz.user.tenant_id != Some(tenant.id) {
            return Err(Error::Forbidden);
        }
        Ok((manager, tenant))
    }

    /// Validate a currency code against the currency table.
    async fn known_currency(&self, code: &str) -> Result<()> {
        crate::money::currency::Store::new(self.main_db.clone())
            .find_by_code(code)
            .await?
            .map(|_| ())
            .ok_or_else(|| Error::Validation(format!("unknown currency {code:?}")))
    }
}

#[derive(Deserialize, ToSchema)]
pub struct RegisterRequest {
    /// Tenant (company) name: lowercase letters, digits, dashes.
    /// Ignored in single-tenant mode.
    pub tenant_name: Option<String>,
    pub company_display_name: Option<String>,
    /// The company's currency, a code from `GET /currencies`.
    /// Ignored in single-tenant mode.
    pub currency: Option<String>,
    pub email: String,
    pub password: String,
    pub first_name: String,
    pub last_name: String,
}

#[derive(Serialize, ToSchema)]
pub struct RegisterResponse {
    pub tenant_id: Option<Uuid>,
    pub user: Profile,
}

/// Company registration: the email and password provided here become the
/// tenant's admin account, seeded with the static `Admin` role.
#[utoipa::path(post, path = "/auth/register", tag = "auth",
    request_body = RegisterRequest,
    responses((status = 200, body = RegisterResponse)))]
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
            if let Some(code) = &req.currency {
                state.known_currency(code).await?;
            }
            let tenant = manager
                .create(NewTenant {
                    display_name: req
                        .company_display_name
                        .clone()
                        .unwrap_or_else(|| name.clone()),
                    name,
                    connection_string: None,
                    default_currency: req.currency.clone(),
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
                        "default_currency": tenant.default_currency,
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
    tenant_id: Option<Uuid>,
    user_id: Uuid,
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
        /// The tenant the session belongs to — send it as the tenant
        /// header on subsequent requests. Null in single-tenant mode.
        tenant: Option<String>,
    },
    /// Password accepted; finish with `POST /auth/login/two-factor`
    /// (send the tenant header there too).
    TwoFactorRequired {
        two_factor_token: String,
        tenant: Option<String>,
    },
    /// The company mandates 2FA and this account has none yet: use the
    /// token on the setup + confirm endpoints, then log in again.
    TwoFactorSetupRequired {
        two_factor_token: String,
        tenant: Option<String>,
    },
    /// The credentials matched accounts in more than one company: retry
    /// with the tenant header set to the chosen one.
    TenantSelection { tenants: Vec<TenantChoice> },
}

/// One of the companies a set of credentials belongs to.
#[derive(Serialize, ToSchema)]
pub struct TenantChoice {
    pub name: String,
    pub display_name: String,
}

/// Sign in with a username or email and password. No tenant header is
/// needed: the tenant is resolved from the credentials via the login
/// directory. A header (or single-tenant mode) skips resolution and
/// authenticates against that context directly — which is also how the
/// client answers a `tenant_selection` response.
#[utoipa::path(post, path = "/auth/login", tag = "auth",
    request_body = LoginRequest,
    responses((status = 200, body = LoginResponse)))]
async fn login(
    State(state): State<AuthState>,
    tenant: Option<Extension<TenantRef>>,
    db: Option<Extension<DatabaseConnection>>,
    audit: Audit,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>> {
    let tenant = tenant.map(|Extension(t)| t);
    if tenant.is_none() && state.tenants.is_some() {
        return resolve_and_login(&state, &req, audit).await.map(Json);
    }

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
    finish_login(&state, &users, tenant, user, audit)
        .await
        .map(Json)
}

/// No tenant given: find the tenants whose user stores know this login,
/// verify the password against each, and sign in where it matches. More
/// than one match hands the choice back to the user.
async fn resolve_and_login(
    state: &AuthState,
    req: &LoginRequest,
    audit: Audit,
) -> Result<LoginResponse> {
    let manager = state.tenants.as_ref().expect("caller checked multitenancy");
    let directory = super::directory::Directory::new(state.main_db.clone());
    let candidates = directory.tenants_matching(&req.login).await?;
    if candidates.is_empty() {
        // Hash anyway so the timing matches a real password check.
        let _ = super::password::hash(&req.password);
    }

    let mut matches = Vec::new();
    let mut locked = None;
    for tenant_id in candidates {
        let Some(tenant) = manager.find_by_id(tenant_id).await? else {
            continue;
        };
        if !tenant.is_active {
            continue;
        }
        let db = manager.connection_for(&tenant).await?;
        let users = state.users(Some(db));
        match users
            .authenticate(Some(tenant.id), &req.login, &req.password)
            .await
        {
            Ok(user) => matches.push((tenant, users, user)),
            Err(e @ Error::Locked(_)) => locked = Some(e),
            Err(_) => {}
        }
    }

    match matches.len() {
        0 => {
            if let Some(e) = locked {
                return Err(e);
            }
            audit
                .0
                .event(format!("failed login attempt for {:?}", req.login))
                .await;
            Err(Error::Unauthorized)
        }
        1 => {
            let (tenant, users, user) = matches.remove(0);
            let tenant_ref = TenantRef {
                id: tenant.id,
                name: tenant.name,
            };
            finish_login(state, &users, Some(tenant_ref), user, audit).await
        }
        _ => Ok(LoginResponse::TenantSelection {
            tenants: matches
                .into_iter()
                .map(|(tenant, ..)| TenantChoice {
                    name: tenant.name,
                    display_name: tenant.display_name,
                })
                .collect(),
        }),
    }
}

/// The password checked out — answer with a session or the applicable
/// two-factor branch, always naming the tenant the client should send
/// the tenant header for from here on.
async fn finish_login(
    state: &AuthState,
    users: &UserManager,
    tenant: Option<TenantRef>,
    user: user::Model,
    audit: Audit,
) -> Result<LoginResponse> {
    let tenant_name = tenant.as_ref().map(|t| t.name.clone());
    if user.two_factor_enabled && user.totp_confirmed_at.is_some() {
        let token = jwt::issue(&state.config, &user, TokenPurpose::TwoFactor)?;
        return Ok(LoginResponse::TwoFactorRequired {
            two_factor_token: token,
            tenant: tenant_name,
        });
    }
    if state.tenant_requires_2fa(tenant.as_ref()).await? {
        let token = jwt::issue(&state.config, &user, TokenPurpose::TwoFactor)?;
        return Ok(LoginResponse::TwoFactorSetupRequired {
            two_factor_token: token,
            tenant: tenant_name,
        });
    }

    let token = jwt::issue(&state.config, &user, TokenPurpose::Access)?;
    let refresh_token = users.issue_refresh_token(user.id).await?;
    audit
        .0
        .with_tenant(tenant.as_ref().map(|t| t.id))
        .with_user(Some(user.id))
        .event(format!("{} logged in", user.user_name))
        .await;
    Ok(LoginResponse::Success {
        access_token: token,
        refresh_token,
        user: user.into(),
        tenant: tenant_name,
    })
}

#[derive(Deserialize, ToSchema)]
pub struct TwoFactorLoginRequest {
    /// Authenticator code or a recovery code.
    pub code: String,
}

#[utoipa::path(post, path = "/auth/login/two-factor", tag = "auth",
    request_body = TwoFactorLoginRequest,
    responses((status = 200, body = LoginResponse)))]
async fn login_two_factor(
    State(state): State<AuthState>,
    TwoFactorUser(user): TwoFactorUser,
    tenant: Option<Extension<TenantRef>>,
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
        tenant: tenant.map(|Extension(t)| t.name),
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

#[utoipa::path(post, path = "/auth/two-factor/setup", tag = "auth",
    responses((status = 200, body = TwoFactorSetup)))]
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

#[utoipa::path(post, path = "/auth/two-factor/confirm", tag = "auth",
    request_body = ConfirmTwoFactorRequest,
    responses((status = 200, body = ConfirmTwoFactorResponse)))]
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

#[utoipa::path(post, path = "/auth/two-factor/disable", tag = "auth",
    request_body = DisableTwoFactorRequest,
    responses((status = 200, body = Profile)))]
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

#[derive(Serialize, ToSchema)]
pub struct TenantTwoFactorResponse {
    pub tenant: String,
    pub require_two_factor: bool,
}

/// Generic acknowledgement for operations without a richer result.
#[derive(Serialize, ToSchema)]
pub struct StatusResponse {
    pub status: String,
}

/// A background job was accepted onto a queue.
#[derive(Serialize, ToSchema)]
pub struct QueuedJobResponse {
    pub status: String,
    pub task_id: String,
}

/// The current company-wide 2FA policy. Readable by any authenticated
/// user of the tenant — the mandate is what they experience at sign-in,
/// and the profile page needs it to know whether opting out is possible.
#[utoipa::path(get, path = "/auth/tenant/two-factor", tag = "auth",
    responses((status = 200, body = TenantTwoFactorResponse)))]
async fn tenant_two_factor_get(
    State(state): State<AuthState>,
    authz: Authz,
    tenant: Option<Extension<TenantRef>>,
) -> Result<Json<TenantTwoFactorResponse>> {
    let Some(Extension(tenant)) = tenant else {
        return Err(Error::Validation("a tenant context is required".into()));
    };
    if authz.user.tenant_id != Some(tenant.id) {
        return Err(Error::Forbidden);
    }
    let required = state.tenant_requires_2fa(Some(&tenant)).await?;
    Ok(Json(TenantTwoFactorResponse {
        tenant: tenant.name,
        require_two_factor: required,
    }))
}

/// Company-wide policy switch; requires the tenant-settings permission.
#[utoipa::path(post, path = "/auth/tenant/two-factor", tag = "auth",
    request_body = TenantTwoFactorRequest,
    responses((status = 200, body = TenantTwoFactorResponse)))]
async fn tenant_two_factor(
    State(state): State<AuthState>,
    authz: Authz,
    audit: Audit,
    tenant: Option<Extension<TenantRef>>,
    Json(req): Json<TenantTwoFactorRequest>,
) -> Result<Json<TenantTwoFactorResponse>> {
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
    Ok(Json(TenantTwoFactorResponse {
        tenant: tenant.name,
        require_two_factor: tenant.require_two_factor,
    }))
}

/// The tenant's company profile as shown to its users and edited in
/// tenant settings.
#[derive(Serialize, ToSchema)]
pub struct CompanyProfileResponse {
    pub tenant: String,
    pub display_name: String,
    /// A code from `GET /currencies`.
    pub default_currency: Option<String>,
    /// Tax registration PIN (e.g. a KRA PIN).
    pub tax_pin: Option<String>,
    pub vat_number: Option<String>,
    /// Where the uploaded company logo is served from, when one exists.
    pub logo_url: Option<String>,
}

fn company_profile(t: &crate::tenancy::tenant::Model) -> CompanyProfileResponse {
    CompanyProfileResponse {
        tenant: t.name.clone(),
        display_name: t.display_name.clone(),
        default_currency: t.default_currency.clone(),
        tax_pin: t.tax_pin.clone(),
        vat_number: t.vat_number.clone(),
        logo_url: t.logo_path.as_ref().map(|p| format!("/public/{p}")),
    }
}

/// Readable by any authenticated user of the tenant — the company name,
/// logo and currency are what its own screens display.
#[utoipa::path(get, path = "/auth/tenant/profile", tag = "auth",
    responses((status = 200, body = CompanyProfileResponse)))]
async fn tenant_profile_get(
    State(state): State<AuthState>,
    authz: Authz,
    tenant: Option<Extension<TenantRef>>,
) -> Result<Json<CompanyProfileResponse>> {
    let (manager, tenant) = state.tenant_context(&authz, tenant)?;
    let row = manager
        .find_by_id(tenant.id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("tenant {}", tenant.id)))?;
    Ok(Json(company_profile(&row)))
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateCompanyProfileRequest {
    pub display_name: String,
    /// A code from `GET /currencies`; null clears the default.
    pub default_currency: Option<String>,
    pub tax_pin: Option<String>,
    pub vat_number: Option<String>,
}

#[utoipa::path(put, path = "/auth/tenant/profile", tag = "auth",
    request_body = UpdateCompanyProfileRequest,
    responses((status = 200, body = CompanyProfileResponse)))]
async fn tenant_profile_update(
    State(state): State<AuthState>,
    authz: Authz,
    audit: Audit,
    tenant: Option<Extension<TenantRef>>,
    Json(req): Json<UpdateCompanyProfileRequest>,
) -> Result<Json<CompanyProfileResponse>> {
    let (manager, tenant) = state.tenant_context(&authz, tenant)?;
    authz.require(permission::names::TENANT_SETTINGS).await?;
    let display_name = req.display_name.trim().to_string();
    if display_name.is_empty() {
        return Err(Error::Validation("display_name must not be empty".into()));
    }
    if let Some(code) = &req.default_currency {
        state.known_currency(code).await?;
    }
    let none_if_blank = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let before = manager
        .find_by_id(tenant.id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("tenant {}", tenant.id)))?;
    let updated = manager
        .update_profile(
            tenant.id,
            crate::tenancy::CompanyProfile {
                display_name,
                default_currency: req.default_currency,
                tax_pin: none_if_blank(req.tax_pin),
                vat_number: none_if_blank(req.vat_number),
            },
        )
        .await?;
    audit
        .0
        .updated(
            "tenant",
            tenant.id,
            &company_profile(&before),
            &company_profile(&updated),
        )
        .await;
    Ok(Json(company_profile(&updated)))
}

/// Multipart body of the logo upload.
#[derive(ToSchema)]
#[allow(dead_code)]
pub struct LogoUpload {
    /// The image file: png, jpg, svg or webp, at most 1 MiB.
    #[schema(value_type = String, format = Binary)]
    pub file: String,
}

/// Stores the logo at `{files.root}/{tenant-id}/logo.{ext}`; it is then
/// served from the `logo_url` in the profile response.
#[utoipa::path(post, path = "/auth/tenant/logo", tag = "auth",
    request_body(content = LogoUpload, content_type = "multipart/form-data"),
    responses((status = 200, body = CompanyProfileResponse)))]
async fn tenant_logo_upload(
    State(state): State<AuthState>,
    authz: Authz,
    audit: Audit,
    tenant: Option<Extension<TenantRef>>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<CompanyProfileResponse>> {
    let (manager, tenant) = state.tenant_context(&authz, tenant)?;
    authz.require(permission::names::TENANT_SETTINGS).await?;

    const ALLOWED: [&str; 5] = ["png", "jpg", "jpeg", "svg", "webp"];
    const MAX_BYTES: usize = 1024 * 1024;
    let mut upload: Option<(String, axum::body::Bytes)> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| Error::Validation(format!("invalid multipart body: {e}")))?
    {
        if field.name() != Some("file") {
            continue;
        }
        let ext = field
            .file_name()
            .and_then(|n| n.rsplit('.').next())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if !ALLOWED.contains(&ext.as_str()) {
            return Err(Error::Validation(
                "the logo must be a png, jpg, svg or webp file".into(),
            ));
        }
        let data = field
            .bytes()
            .await
            .map_err(|e| Error::Validation(format!("failed to read the upload: {e}")))?;
        if data.is_empty() {
            return Err(Error::Validation("the uploaded file is empty".into()));
        }
        if data.len() > MAX_BYTES {
            return Err(Error::Validation("the logo must be at most 1 MiB".into()));
        }
        upload = Some((ext, data));
        break;
    }
    let Some((ext, data)) = upload else {
        return Err(Error::Validation(
            "a multipart field named \"file\" is required".into(),
        ));
    };

    let root = std::path::Path::new(&state.files.root);
    let dir = root.join(tenant.id.to_string());
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| Error::internal(format!("could not create the upload directory: {e}")))?;
    let before = manager
        .find_by_id(tenant.id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("tenant {}", tenant.id)))?;
    let relative = format!("{}/logo.{ext}", tenant.id);
    tokio::fs::write(dir.join(format!("logo.{ext}")), &data)
        .await
        .map_err(|e| Error::internal(format!("could not store the logo: {e}")))?;
    // A previous logo with a different extension would otherwise linger.
    if let Some(old) = before.logo_path.as_deref()
        && old != relative
    {
        let _ = tokio::fs::remove_file(root.join(old)).await;
    }
    let updated = manager.set_logo_path(tenant.id, Some(relative)).await?;
    audit
        .0
        .updated(
            "tenant",
            tenant.id,
            &serde_json::json!({ "logo_path": before.logo_path }),
            &serde_json::json!({ "logo_path": updated.logo_path }),
        )
        .await;
    Ok(Json(company_profile(&updated)))
}

/// Queue a background migration of the caller's tenant database — how a
/// tenant picks up newly deployed features without waiting for the next
/// restart. Needs `jobs.enabled`.
#[utoipa::path(post, path = "/auth/tenant/migrate", tag = "auth",
    responses((status = 200, body = QueuedJobResponse)))]
async fn tenant_migrate(
    authz: Authz,
    audit: Audit,
    jobs: Option<Extension<crate::jobs::Jobs>>,
) -> Result<Json<QueuedJobResponse>> {
    authz.require(permission::names::TENANT_SETTINGS).await?;
    let Some(Extension(jobs)) = jobs else {
        return Err(Error::Validation(
            "background jobs are not enabled on this deployment".into(),
        ));
    };
    let task_id = jobs
        .enqueue(
            crate::jobs::TENANT_MIGRATION_QUEUE,
            crate::jobs::MigrateTenants {
                tenant_id: authz.user.tenant_id,
            },
        )
        .await?;
    audit
        .0
        .event(format!(
            "{} queued a tenant database migration",
            authz.user.user_name
        ))
        .await;
    Ok(Json(QueuedJobResponse {
        status: "queued".into(),
        task_id,
    }))
}

#[utoipa::path(get, path = "/auth/me", tag = "auth",
    responses((status = 200, body = Profile)))]
async fn me(CurrentUser(user): CurrentUser) -> Json<Profile> {
    Json(user.into())
}

#[derive(Deserialize, ToSchema)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

/// Exchange a refresh token for a fresh access/refresh pair. The old
/// token is revoked; reusing it later revokes every session of the user.
#[utoipa::path(post, path = "/auth/token/refresh", tag = "auth",
    request_body = RefreshRequest,
    responses((status = 200, body = LoginResponse)))]
async fn refresh(
    State(state): State<AuthState>,
    tenant: Option<Extension<TenantRef>>,
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
        tenant: tenant.map(|Extension(t)| t.name),
    }))
}

#[utoipa::path(post, path = "/auth/logout", tag = "auth",
    request_body = RefreshRequest,
    responses((status = 200, body = StatusResponse)))]
async fn logout(
    State(state): State<AuthState>,
    db: Option<Extension<DatabaseConnection>>,
    audit: Audit,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<StatusResponse>> {
    let users = state.users(db.map(|Extension(d)| d));
    users.revoke_refresh_token(&req.refresh_token).await?;
    audit.0.event("a session logged out").await;
    Ok(Json(StatusResponse {
        status: "logged_out".into(),
    }))
}

#[derive(Deserialize, ToSchema)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

/// Changing the password rotates the security stamp and revokes all
/// refresh tokens — every other session has to sign in again.
#[utoipa::path(post, path = "/auth/password", tag = "auth",
    request_body = ChangePasswordRequest,
    responses((status = 200, body = Profile)))]
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
#[utoipa::path(post, path = "/auth/users", tag = "auth",
    request_body = CreateUserRequest,
    responses((status = 200, body = Profile)))]
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

#[utoipa::path(get, path = "/auth/users", tag = "auth",
    responses((status = 200, body = Vec<Profile>)))]
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
#[utoipa::path(put, path = "/auth/users/{id}/admin", tag = "auth",
    params(("id" = Uuid, Path, description = "User id")),
    request_body = SetAdminRequest,
    responses((status = 200, body = Profile)))]
async fn set_user_admin(
    State(state): State<AuthState>,
    authz: Authz,
    audit: Audit,
    db: Option<Extension<DatabaseConnection>>,
    axum::extract::Path(user_id): axum::extract::Path<Uuid>,
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
    pub role_ids: Vec<Uuid>,
}

#[derive(Serialize, ToSchema)]
pub struct RoleResponse {
    pub id: Uuid,
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
#[utoipa::path(get, path = "/auth/permissions", tag = "auth",
    responses((status = 200, body = Vec<PermissionDef>)))]
async fn permission_tree(authz: Authz) -> Result<Json<Vec<PermissionDef>>> {
    authz.require(permission::names::ROLES_VIEW).await?;
    Ok(Json(authz.registry().tree().to_vec()))
}

/// The caller's own effective permissions — any authenticated user.
#[utoipa::path(get, path = "/auth/me/permissions", tag = "auth",
    responses((status = 200, body = Vec<String>)))]
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
async fn tenant_role(authz: &Authz, role_id: Uuid) -> Result<super::role::Model> {
    let role = authz.roles().find_by_id(role_id).await?;
    if role.tenant_id != authz.user.tenant_id {
        return Err(Error::NotFound("role".into()));
    }
    Ok(role)
}

#[utoipa::path(get, path = "/auth/roles", tag = "auth",
    responses((status = 200, body = Vec<RoleResponse>)))]
async fn list_roles(authz: Authz) -> Result<Json<Vec<RoleResponse>>> {
    authz.require(permission::names::ROLES_VIEW).await?;
    let mut out = Vec::new();
    for role in authz.roles().find_all(authz.user.tenant_id).await? {
        out.push(role_response(authz.roles(), role).await?);
    }
    Ok(Json(out))
}

#[utoipa::path(post, path = "/auth/roles", tag = "auth",
    request_body = CreateRoleRequest,
    responses((status = 200, body = RoleResponse)))]
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

#[utoipa::path(put, path = "/auth/roles/{id}", tag = "auth",
    params(("id" = Uuid, Path, description = "Role id")),
    request_body = UpdateRoleRequest,
    responses((status = 200, body = RoleResponse)))]
async fn update_role(
    authz: Authz,
    audit: Audit,
    axum::extract::Path(role_id): axum::extract::Path<Uuid>,
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

#[utoipa::path(delete, path = "/auth/roles/{id}", tag = "auth",
    params(("id" = Uuid, Path, description = "Role id")),
    responses((status = 200, body = StatusResponse)))]
async fn delete_role(
    authz: Authz,
    audit: Audit,
    axum::extract::Path(role_id): axum::extract::Path<Uuid>,
) -> Result<Json<StatusResponse>> {
    authz.require(permission::names::ROLES_DELETE).await?;
    let role = tenant_role(&authz, role_id).await?;
    let before = role_response(authz.roles(), role.clone()).await?;
    authz.roles().delete_role(role.id).await?;
    audit.0.deleted("role", before.id, &before).await;
    Ok(Json(StatusResponse {
        status: "deleted".into(),
    }))
}

/// Replace a user's role set. Changing your own roles is refused — an
/// admin locking themselves out is never one call away.
#[utoipa::path(put, path = "/auth/users/{id}/roles", tag = "auth",
    params(("id" = Uuid, Path, description = "User id")),
    request_body = SetUserRolesRequest,
    responses((status = 200, body = Vec<RoleResponse>)))]
async fn set_user_roles(
    State(state): State<AuthState>,
    authz: Authz,
    audit: Audit,
    db: Option<Extension<DatabaseConnection>>,
    axum::extract::Path(user_id): axum::extract::Path<Uuid>,
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

#[utoipa::path(get, path = "/auth/users/{id}/permissions", tag = "auth",
    params(("id" = Uuid, Path, description = "User id")),
    responses((status = 200, body = UserPermissionsResponse)))]
async fn user_permissions(
    State(state): State<AuthState>,
    authz: Authz,
    db: Option<Extension<DatabaseConnection>>,
    axum::extract::Path(user_id): axum::extract::Path<Uuid>,
) -> Result<Json<UserPermissionsResponse>> {
    authz.require(permission::names::USERS_PERMISSIONS).await?;
    let target = tenant_user(&state, &authz, db, user_id).await?;
    user_permissions_response(&authz, target.id).await.map(Json)
}

/// Replace a user's per-user overrides. Like roles, never on yourself.
#[utoipa::path(put, path = "/auth/users/{id}/permissions", tag = "auth",
    params(("id" = Uuid, Path, description = "User id")),
    request_body = SetUserPermissionsRequest,
    responses((status = 200, body = UserPermissionsResponse)))]
async fn set_user_permissions(
    State(state): State<AuthState>,
    authz: Authz,
    audit: Audit,
    db: Option<Extension<DatabaseConnection>>,
    axum::extract::Path(user_id): axum::extract::Path<Uuid>,
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
    user_id: Uuid,
) -> Result<user::Model> {
    state
        .users(db.map(|Extension(d)| d))
        .find_by_id(user_id)
        .await?
        .filter(|u| u.tenant_id == authz.user.tenant_id)
        .ok_or_else(|| Error::NotFound(format!("user {user_id}")))
}

async fn user_permissions_response(authz: &Authz, user_id: Uuid) -> Result<UserPermissionsResponse> {
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
