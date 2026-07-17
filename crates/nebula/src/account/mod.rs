//! The account module: self-service endpoints for signing up, signing
//! in and managing one's own credentials. No permission guards — these
//! are the doors into the system, not administration.
//!
//! - `POST /auth/register` — a company registers: creates the tenant and
//!   its admin account from the email + password given at registration
//!   (multitenant mode; in single-tenant mode it creates a host user)
//! - `POST /auth/login` — password step, no tenant header required: the
//!   tenant is resolved from the credentials via the login directory
//!   (`tenant_selection` answers an ambiguous login; retry with the
//!   header). Answers `success` with an access token,
//!   `two_factor_required` (user has an authenticator),
//!   `two_factor_setup_required` (company mandates 2FA but the user has
//!   not set it up) or `password_expired` (the password is older than the
//!   company's policy allows), the latter three with a short-lived bridge
//!   token; every answer names the resolved tenant for subsequent headers
//! - `POST /auth/login/two-factor` — completes login with an
//!   authenticator or recovery code
//! - `POST /auth/two-factor/setup` + `/confirm` — enable an authenticator
//!   (from the profile, or during mandated setup with a bridge token);
//!   confirm returns the one-time recovery codes
//! - `POST /auth/two-factor/disable` — opt out (password required;
//!   refused while the company mandates 2FA)
//! - `POST /auth/token/refresh` + `POST /auth/logout` — session lifecycle
//! - `POST /auth/password` — change the password (revokes all sessions)
//! - `POST /auth/password/expired` — replace an expired password with a
//!   bridge token, when login refused to issue a session over it
//! - `GET /auth/me` — the authenticated profile;
//!   `GET /auth/me/permissions` — the caller's effective permissions
//!
//! Tenant context comes from the tenant resolution middleware: the same
//! `X-Tenant` header used everywhere else selects whose user store a
//! request talks to.
//!
//! Facts announced on the event bus: [`events::UserRegistered`] and,
//! as an integration event, [`crate::tenancy::TenantCreated`].

pub mod events;

use crate::audit::Audit;
use crate::auth::authz::Authz;
use crate::auth::jwt::{self, CurrentUser, PasswordChangeUser, TokenPurpose, TwoFactorUser};
use crate::auth::manager::{NewUser, TwoFactorSetup, UserManager};
use crate::auth::permission::Registry;
use crate::auth::policy::PasswordPolicy;
use crate::auth::role_manager::RoleManager;
use crate::auth::state::AuthState;
use crate::auth::user::{self, Profile};
use crate::error::{Error, Result};
use crate::module::{Module, ModuleContext};
use crate::tenancy::{NewTenant, TenantRef};
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

pub struct AccountModule;

impl Module for AccountModule {
    fn name(&self) -> &'static str {
        "account"
    }

    fn configure(&self, ctx: &mut ModuleContext) {
        assert!(
            !ctx.config().auth.jwt_secret.is_empty(),
            "AccountModule requires auth.jwt_secret; set it in \
             config/{{env}}.local.yaml or NEBULA__AUTH__JWT_SECRET"
        );
        let state = AuthState::from_ctx(ctx);
        ctx.add_api(crate::module::build_openapi(|| {
            <ApiDoc as utoipa::OpenApi>::openapi()
        }));
        ctx.add_routes(
            Router::new()
                .route("/auth/register", post(register))
                .route("/auth/login", post(login))
                .route("/auth/login/two-factor", post(login_two_factor))
                .route("/auth/two-factor/setup", post(two_factor_setup))
                .route("/auth/two-factor/confirm", post(two_factor_confirm))
                .route("/auth/two-factor/disable", post(two_factor_disable))
                .route("/auth/token/refresh", post(refresh))
                .route("/auth/logout", post(logout))
                .route("/auth/password", post(change_password))
                .route("/auth/password/expired", post(change_expired_password))
                .route("/auth/me", get(me))
                .route("/auth/me/permissions", get(my_permissions))
                .with_state(state),
        );
    }
}

/// The account module's OpenAPI contribution — the source client
/// generators (NSwag) build the account service proxy from.
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
    change_expired_password,
    me,
    my_permissions,
))]
struct ApiDoc;

/// Generic acknowledgement for operations without a richer result.
#[derive(Serialize, ToSchema)]
pub struct StatusResponse {
    pub status: String,
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
            // A bad admin account (weak password, invalid email) must fail
            // here, before a tenant — and possibly a whole database — is
            // provisioned for it. The company does not exist yet, so the
            // deployment's policy is the only one there is to apply.
            crate::auth::manager::validate_new_user(
                &admin(None),
                &PasswordPolicy::from_config(&state.config),
            )?;
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
            let seeded = async {
                let user = state
                    .users(Some(db.clone()))
                    .create(admin(Some(tenant.id)))
                    .await?;
                seed_admin(&db, &registry, Some(tenant.id), user.id).await?;
                Ok::<_, Error>(user)
            }
            .await;
            let user = match seeded {
                Ok(user) => user,
                Err(e) => {
                    // The tenant exists but its admin never did: remove
                    // it — and the database cut for it (registration
                    // never passes a connection string, so one on the row
                    // is ours) — rather than strand a workspace no one
                    // can sign into and burn its name.
                    manager
                        .remove(&tenant, tenant.connection_string.is_some())
                        .await;
                    return Err(e);
                }
            };
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
            // The registration itself succeeded; a lost announcement is
            // the broker's problem to surface, not a reason to 500.
            if let Err(e) = state
                .events
                .broadcast(crate::tenancy::TenantCreated {
                    tenant_id: tenant.id,
                    name: tenant.name.clone(),
                    display_name: tenant.display_name.clone(),
                })
                .await
            {
                tracing::error!(tenant = %tenant.name, error = %e, "failed to broadcast tenant_created");
            }
            state
                .events
                .publish(events::UserRegistered {
                    tenant_id: Some(tenant.id),
                    user_id: profile.id,
                    email: profile.email.clone(),
                })
                .await;
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
            state
                .events
                .publish(events::UserRegistered {
                    tenant_id: None,
                    user_id: profile.id,
                    email: profile.email.clone(),
                })
                .await;
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
    /// The password is older than the company's policy allows: set a new
    /// one at `POST /auth/password/expired` with this token, then log in
    /// again. No session is issued until it is changed.
    PasswordExpired {
        password_token: String,
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

    let users = state
        .users_with_policy(tenant.as_ref().map(|t| t.id), db.map(|Extension(d)| d))
        .await?;
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
    let directory = crate::auth::directory::Directory::new(state.main_db.clone());
    let candidates = directory.tenants_matching(&req.login).await?;
    if candidates.is_empty() {
        // Hash anyway so the timing matches a real password check.
        let _ = crate::auth::password::hash(&req.password);
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
        // The tenant row is already in hand, so its policy costs nothing
        // to resolve here — each candidate company is judged by its own
        // lockout rules.
        let policy = PasswordPolicy::resolve(&state.config, Some(&tenant));
        let users = state.users(Some(db)).with_policy(policy);
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

    issue_session(
        state,
        users,
        tenant.as_ref().map(|t| t.id),
        tenant_name,
        user,
        audit,
        "",
    )
    .await
}

/// Identity is fully proved — issue the session, unless the password has
/// aged past the company's policy and must be replaced first.
///
/// The expiry check sits *after* the two-factor branches deliberately.
/// The forced change accepts the bridge token as its proof, so running
/// this before the second factor would let someone holding nothing but a
/// stolen password set a new one and take the account outright — turning
/// a policy meant to harden accounts into a way through the very control
/// protecting them.
async fn issue_session(
    state: &AuthState,
    users: &UserManager,
    tenant_id: Option<Uuid>,
    tenant_name: Option<String>,
    user: user::Model,
    audit: Audit,
    how: &str,
) -> Result<LoginResponse> {
    if users.password_expired(&user) {
        let token = jwt::issue(&state.config, &user, TokenPurpose::PasswordChange)?;
        audit
            .0
            .with_tenant(tenant_id)
            .with_user(Some(user.id))
            .event(format!(
                "{}'s password has expired and must be changed",
                user.user_name
            ))
            .await;
        return Ok(LoginResponse::PasswordExpired {
            password_token: token,
            tenant: tenant_name,
        });
    }

    let token = jwt::issue(&state.config, &user, TokenPurpose::Access)?;
    let refresh_token = users.issue_refresh_token(user.id).await?;
    audit
        .0
        .with_tenant(tenant_id)
        .with_user(Some(user.id))
        .event(format!("{} logged in{how}", user.user_name))
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
    let tenant = tenant.map(|Extension(t)| t);
    let users = state
        .users_with_policy(tenant.as_ref().map(|t| t.id), db.map(|Extension(d)| d))
        .await?;
    let user = users.verify_two_factor(user, &req.code).await?;
    issue_session(
        &state,
        &users,
        tenant.as_ref().map(|t| t.id),
        tenant.map(|t| t.name),
        user,
        audit,
        " with two-factor",
    )
    .await
    .map(Json)
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
    if !crate::auth::password::verify(&req.password, &user.password_hash) {
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
    db: Option<Extension<DatabaseConnection>>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<LoginResponse>> {
    let users = state.users(db.map(|Extension(d)| d));
    let (user, refresh_token) = users.rotate_refresh_token(&req.refresh_token).await?;
    let access_token = jwt::issue(&state.config, &user, TokenPurpose::Access)?;
    // Named from the user's own row, not the request header — a header
    // naming the wrong tenant must not mislabel the session it refreshed.
    let tenant = state.tenant_name_of(user.tenant_id).await?;
    Ok(Json(LoginResponse::Success {
        access_token,
        refresh_token,
        user: user.into(),
        tenant,
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
    tenant: Option<Extension<TenantRef>>,
    db: Option<Extension<DatabaseConnection>>,
    audit: Audit,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<Json<Profile>> {
    let tenant = tenant.map(|Extension(t)| t);
    let users = state
        .users_with_policy(tenant.as_ref().map(|t| t.id), db.map(|Extension(d)| d))
        .await?;
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
pub struct ExpiredPasswordRequest {
    pub new_password: String,
}

/// Replace a password login refused as expired, using the bridge token
/// from the `password_expired` answer. No session is issued: the user
/// signs in again with the new password, the same way mandated two-factor
/// setup ends.
///
/// The current password is not asked for again — the bridge token is
/// issued only to someone who just proved it (and cleared two-factor, if
/// the company mandates it), and re-typing a password the system has
/// already refused is not a check, only a chore.
#[utoipa::path(post, path = "/auth/password/expired", tag = "auth",
    request_body = ExpiredPasswordRequest,
    responses((status = 200, body = StatusResponse)))]
async fn change_expired_password(
    State(state): State<AuthState>,
    PasswordChangeUser(user): PasswordChangeUser,
    tenant: Option<Extension<TenantRef>>,
    db: Option<Extension<DatabaseConnection>>,
    audit: Audit,
    Json(req): Json<ExpiredPasswordRequest>,
) -> Result<Json<StatusResponse>> {
    let tenant = tenant.map(|Extension(t)| t);
    let users = state
        .users_with_policy(tenant.as_ref().map(|t| t.id), db.map(|Extension(d)| d))
        .await?;
    let user = users.replace_password(user, &req.new_password).await?;
    audit
        .0
        .with_tenant(tenant.as_ref().map(|t| t.id))
        .with_user(Some(user.id))
        .event(format!("{} replaced an expired password", user.user_name))
        .await;
    Ok(Json(StatusResponse {
        status: "password_changed".into(),
    }))
}

#[utoipa::path(get, path = "/auth/me", tag = "auth",
    responses((status = 200, body = Profile)))]
async fn me(CurrentUser(user): CurrentUser) -> Json<Profile> {
    Json(user.into())
}

/// The caller's own effective permissions — any authenticated user.
#[utoipa::path(get, path = "/auth/me/permissions", tag = "auth",
    responses((status = 200, body = Vec<String>)))]
async fn my_permissions(authz: Authz) -> Result<Json<Vec<String>>> {
    let mut names: Vec<String> = authz.granted_permissions().await?.into_iter().collect();
    names.sort();
    Ok(Json(names))
}
