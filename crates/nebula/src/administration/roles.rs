//! Role administration: role CRUD with permission grants, plus the
//! permission definition tree for admin UIs. Guarded by
//! `Pages.Administration.Roles.*` and scoped to the caller's tenant.

use crate::account::StatusResponse;
use crate::audit::Audit;
use crate::auth::authz::Authz;
use crate::auth::permission::{self, PermissionDef};
use crate::auth::role;
use crate::auth::role_manager::RoleManager;
use crate::error::{Error, Result};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

pub(super) fn routes() -> Router {
    Router::new()
        .route("/auth/roles", axum::routing::post(create_role).get(list_roles))
        .route(
            "/auth/roles/{id}",
            axum::routing::put(update_role).delete(delete_role),
        )
        .route("/auth/permissions", get(permission_tree))
}

pub(super) fn api() -> utoipa::openapi::OpenApi {
    crate::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    create_role,
    list_roles,
    update_role,
    delete_role,
    permission_tree,
))]
struct ApiDoc;

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

/// The full permission definition tree, for admin UIs.
#[utoipa::path(get, path = "/auth/permissions", tag = "auth",
    responses((status = 200, body = Vec<PermissionDef>)))]
async fn permission_tree(authz: Authz) -> Result<Json<Vec<PermissionDef>>> {
    authz.require(permission::names::ROLES_VIEW).await?;
    Ok(Json(authz.registry().tree().to_vec()))
}

pub(super) async fn role_response(
    roles: &RoleManager,
    role: role::Model,
) -> Result<RoleResponse> {
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
pub(super) async fn tenant_role(authz: &Authz, role_id: Uuid) -> Result<role::Model> {
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
