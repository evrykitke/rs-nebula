//! User administration: team onboarding, role assignment and per-user
//! permission overrides, all guarded by `Pages.Administration.Users.*`
//! permissions and strictly scoped to the caller's tenant.

use super::roles::{RoleResponse, role_response, tenant_role};
use crate::audit::Audit;
use crate::auth::authz::Authz;
use crate::auth::manager::NewUser;
use crate::auth::permission;
use crate::auth::state::AuthState;
use crate::auth::user::{self, Profile};
use crate::error::{Error, Result};
use axum::extract::State;
use axum::routing::get;
use axum::{Extension, Json, Router};
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

pub(super) fn routes(state: AuthState) -> Router {
    Router::new()
        .route(
            "/auth/users",
            axum::routing::post(create_user).get(list_users),
        )
        .route("/auth/users/{id}/admin", axum::routing::put(set_user_admin))
        .route("/auth/users/{id}/roles", axum::routing::put(set_user_roles))
        .route(
            "/auth/users/{id}/permissions",
            get(user_permissions).put(set_user_permissions),
        )
        .with_state(state)
}

pub(super) fn api() -> utoipa::openapi::OpenApi {
    crate::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    create_user,
    list_users,
    set_user_admin,
    set_user_roles,
    user_permissions,
    set_user_permissions,
))]
struct ApiDoc;

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
    state
        .events
        .publish(crate::account::events::UserCreated {
            tenant_id: authz.user.tenant_id,
            user_id: profile.id,
            email: profile.email.clone(),
        })
        .await;
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
    let role_names = |roles: &[crate::auth::role::Model]| {
        roles.iter().map(|r| r.name.clone()).collect::<Vec<_>>()
    };
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

async fn user_permissions_response(
    authz: &Authz,
    user_id: Uuid,
) -> Result<UserPermissionsResponse> {
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
