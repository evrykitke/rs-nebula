//! The company's SMTP settings.
//!
//! Unlike the two-factor mandate, reads are gated on
//! `Pages.Administration.Tenant.Settings` too: a mail host and username
//! describe the company's infrastructure, and no ordinary user has a
//! reason to read them.
//!
//! The password never travels back to the client — [`MailSettings`] says
//! only whether one is set. Submitting without a password leaves the
//! stored one alone, so a settings form can round-trip without ever
//! holding it.

use crate::audit::Audit;
use crate::auth::authz::Authz;
use crate::auth::permission;
use crate::auth::state::AuthState;
use crate::error::Result;
use crate::mail::{MailSettings, MailSettingsInput};
use crate::tenancy::TenantRef;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub(super) fn routes(state: AuthState) -> Router {
    Router::new()
        .route(
            "/auth/tenant/mail",
            get(mail_settings_get).put(mail_settings_update),
        )
        .route("/auth/tenant/mail/test", post(mail_settings_test))
        .with_state(state)
}

pub(super) fn api() -> utoipa::openapi::OpenApi {
    crate::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(mail_settings_get, mail_settings_update, mail_settings_test))]
struct ApiDoc;

/// The company's mail settings, or `configured: false` when it has none.
#[derive(Serialize, ToSchema)]
pub struct MailSettingsResponse {
    pub configured: bool,
    pub settings: Option<MailSettings>,
}

#[derive(Deserialize, ToSchema)]
pub struct MailTestRequest {
    /// Where to send the test message.
    pub to: String,
    /// The settings to test. Sent with the request rather than read from
    /// storage so an admin can prove a server works before saving it.
    pub settings: MailSettingsInput,
}

#[derive(Serialize, ToSchema)]
pub struct MailTestResponse {
    pub status: String,
}

#[utoipa::path(get, path = "/auth/tenant/mail", tag = "auth",
    responses((status = 200, body = MailSettingsResponse)))]
async fn mail_settings_get(
    State(state): State<AuthState>,
    authz: Authz,
    tenant: Option<Extension<TenantRef>>,
) -> Result<Json<MailSettingsResponse>> {
    let (_, tenant) = state.tenant_context(&authz, tenant)?;
    authz.require(permission::names::TENANT_SETTINGS).await?;
    let settings = state.mail.settings(tenant.id).await?;
    Ok(Json(MailSettingsResponse {
        configured: settings.is_some(),
        settings,
    }))
}

#[utoipa::path(put, path = "/auth/tenant/mail", tag = "auth",
    request_body = MailSettingsInput,
    responses((status = 200, body = MailSettingsResponse)))]
async fn mail_settings_update(
    State(state): State<AuthState>,
    authz: Authz,
    audit: Audit,
    tenant: Option<Extension<TenantRef>>,
    Json(req): Json<MailSettingsInput>,
) -> Result<Json<MailSettingsResponse>> {
    let (_, tenant) = state.tenant_context(&authz, tenant)?;
    authz.require(permission::names::TENANT_SETTINGS).await?;

    let before = state.mail.settings(tenant.id).await?;
    let saved = state.mail.save(tenant.id, req).await?;
    // `MailSettings` carries no password, so the audit trail cannot leak
    // one — which is the reason the diff is built from the view and not
    // from the request.
    audit
        .0
        .updated(
            "tenant_mail_settings",
            tenant.id,
            &serde_json::json!({ "mail": before }),
            &serde_json::json!({ "mail": saved }),
        )
        .await;
    Ok(Json(MailSettingsResponse {
        configured: true,
        settings: Some(saved),
    }))
}

/// Send a test message with the given settings. The mail server's own
/// complaint is passed back verbatim: "authentication failed" and
/// "connection refused" send an admin to entirely different places, and a
/// generic failure would tell them neither.
#[utoipa::path(post, path = "/auth/tenant/mail/test", tag = "auth",
    request_body = MailTestRequest,
    responses((status = 200, body = MailTestResponse)))]
async fn mail_settings_test(
    State(state): State<AuthState>,
    authz: Authz,
    audit: Audit,
    tenant: Option<Extension<TenantRef>>,
    Json(req): Json<MailTestRequest>,
) -> Result<Json<MailTestResponse>> {
    let (_, tenant) = state.tenant_context(&authz, tenant)?;
    authz.require(permission::names::TENANT_SETTINGS).await?;
    state.mail.send_test(tenant.id, &req.settings, &req.to).await?;
    audit
        .0
        .event(format!("sent a test email to {}", req.to))
        .await;
    Ok(Json(MailTestResponse {
        status: "sent".into(),
    }))
}
