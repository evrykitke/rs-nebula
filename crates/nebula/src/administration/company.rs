//! Company settings: the tenant's own profile (display name, default
//! currency, tax ids), logo upload, the company-wide two-factor mandate
//! and on-demand database migration. Reads are open to any user of the
//! tenant; writes require `Pages.Administration.Tenant.Settings`.

use crate::audit::Audit;
use crate::auth::authz::Authz;
use crate::auth::permission;
use crate::auth::state::AuthState;
use crate::error::{Error, Result};
use crate::tenancy::TenantRef;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub(super) fn routes(state: AuthState) -> Router {
    Router::new()
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
        .with_state(state)
}

pub(super) fn api() -> utoipa::openapi::OpenApi {
    crate::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    tenant_two_factor,
    tenant_two_factor_get,
    tenant_profile_get,
    tenant_profile_update,
    tenant_logo_upload,
    tenant_migrate,
))]
struct ApiDoc;

#[derive(Deserialize, ToSchema)]
pub struct TenantTwoFactorRequest {
    pub required: bool,
}

#[derive(Serialize, ToSchema)]
pub struct TenantTwoFactorResponse {
    pub tenant: String,
    pub require_two_factor: bool,
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
    let (manager, tenant) = state.tenant_context(&authz, tenant)?;
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
    /// Postal/street address (may be multi-line).
    pub address: Option<String>,
    pub email: Option<String>,
    pub website: Option<String>,
    pub phone: Option<String>,
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
        address: t.address.clone(),
        email: t.email.clone(),
        website: t.website.clone(),
        phone: t.phone.clone(),
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
    /// Postal/street address (may be multi-line); blank clears it.
    pub address: Option<String>,
    pub email: Option<String>,
    pub website: Option<String>,
    pub phone: Option<String>,
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
    let none_if_blank =
        |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
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
                address: none_if_blank(req.address),
                email: none_if_blank(req.email),
                website: none_if_blank(req.website),
                phone: none_if_blank(req.phone),
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
    /// The image file: png, jpg or webp, at most 1 MiB. SVG is refused —
    /// it is a script container, and `/public` serves it same-origin.
    #[schema(value_type = String, format = Binary)]
    pub file: String,
}

/// Stores the logo at `{files.root}/{slug}/{id}/logo.{ext}`; it is then
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

    const MAX_BYTES: usize = 1024 * 1024;
    let mut data: Option<axum::body::Bytes> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| Error::Validation(format!("invalid multipart body: {e}")))?
    {
        if field.name() != Some("file") {
            continue;
        }
        data = Some(
            field
                .bytes()
                .await
                .map_err(|e| Error::Validation(format!("failed to read the upload: {e}")))?,
        );
        break;
    }
    let Some(data) = data else {
        return Err(Error::Validation(
            "a multipart field named \"file\" is required".into(),
        ));
    };
    // Trust the content, not the file name: the bytes must actually be an
    // allowed raster image, and we store it under the format's own
    // extension. This is what stops a `logo.png` that is really an SVG or
    // HTML from becoming stored XSS on same-origin `/public`.
    let format = crate::storage::guard_image(&data, MAX_BYTES)?;

    let before = manager
        .find_by_id(tenant.id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("tenant {}", tenant.id)))?;
    let stored = state
        .storage
        .tenant(&tenant)
        .store(&format!("logo.{}", format.extension()), &data)
        .await?;
    // Every upload lands under a fresh id, so the previous file (possibly
    // still on the pre-slug layout) is always stale.
    if let Some(old) = before.logo_path.as_deref() {
        let _ = state.storage.remove(old).await;
    }
    let updated = manager.set_logo_path(tenant.id, Some(stored.path)).await?;
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
