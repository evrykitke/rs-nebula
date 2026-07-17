//! The company-wide password policy: length, character classes, expiry,
//! reuse and lockout.
//!
//! Reads are open to any user of the tenant — the policy is what they meet
//! when they change their password, and a form that cannot state the rules
//! can only let people guess at them. Writes require
//! `Pages.Administration.Tenant.Settings`.
//!
//! Every field is an override. Null means the rule follows the
//! deployment's `auth.*` default, which is also its floor: a company may
//! tighten a rule, never loosen it below what the deployment set. See
//! [`crate::auth::policy::PasswordPolicy`].

use crate::audit::Audit;
use crate::auth::authz::Authz;
use crate::auth::permission;
use crate::auth::policy::PasswordPolicy;
use crate::auth::state::AuthState;
use crate::error::{Error, Result};
use crate::tenancy::{PasswordPolicyOverrides, TenantRef};
use axum::extract::State;
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub(super) fn routes(state: AuthState) -> Router {
    Router::new()
        .route(
            "/auth/tenant/password-policy",
            get(password_policy_get).put(password_policy_update),
        )
        .with_state(state)
}

pub(super) fn api() -> utoipa::openapi::OpenApi {
    crate::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(password_policy_get, password_policy_update))]
struct ApiDoc;

#[derive(Serialize, ToSchema)]
pub struct PasswordPolicyResponse {
    pub tenant: String,
    /// The rules actually enforced, after the company's overrides.
    pub policy: PasswordPolicy,
    /// The deployment's own settings. A company may tighten past these,
    /// never below them — so a settings page can show what is not on offer
    /// instead of letting an admin discover it by being refused.
    pub floor: PasswordPolicy,
    /// The company's overrides as stored: null means the rule follows
    /// `floor` and will keep following it if the deployment changes.
    pub overrides: PasswordPolicyOverridesView,
}

/// The stored overrides, echoed back so a form can tell "the company chose
/// 12" from "the company chose nothing and the deployment says 12".
#[derive(Serialize, Deserialize, ToSchema, Default)]
pub struct PasswordPolicyOverridesView {
    pub min_length: Option<i32>,
    pub require_uppercase: Option<bool>,
    pub require_lowercase: Option<bool>,
    pub require_digit: Option<bool>,
    pub require_symbol: Option<bool>,
    pub expiry_days: Option<i32>,
    pub history_count: Option<i32>,
    pub lockout_max_failed: Option<i32>,
    pub lockout_secs: Option<i32>,
}

/// The company's password policy. Any authenticated user of the tenant may
/// read it.
#[utoipa::path(get, path = "/auth/tenant/password-policy", tag = "auth",
    responses((status = 200, body = PasswordPolicyResponse)))]
async fn password_policy_get(
    State(state): State<AuthState>,
    authz: Authz,
    tenant: Option<Extension<TenantRef>>,
) -> Result<Json<PasswordPolicyResponse>> {
    let Some(Extension(tenant)) = tenant else {
        return Err(Error::Validation("a tenant context is required".into()));
    };
    if authz.user.tenant_id != Some(tenant.id) {
        return Err(Error::Forbidden);
    }
    let manager = state.tenants.clone().ok_or_else(|| {
        Error::Validation("multitenancy is not enabled on this deployment".into())
    })?;
    let row = manager.find_by_id(tenant.id).await?;
    Ok(Json(response(&state, tenant.name, row.as_ref())))
}

/// Replace the company's overrides. Fields left null revert to the
/// deployment default.
#[utoipa::path(put, path = "/auth/tenant/password-policy", tag = "auth",
    request_body = PasswordPolicyOverridesView,
    responses((status = 200, body = PasswordPolicyResponse)))]
async fn password_policy_update(
    State(state): State<AuthState>,
    authz: Authz,
    audit: Audit,
    tenant: Option<Extension<TenantRef>>,
    Json(req): Json<PasswordPolicyOverridesView>,
) -> Result<Json<PasswordPolicyResponse>> {
    let (manager, tenant) = state.tenant_context(&authz, tenant)?;
    authz.require(permission::names::TENANT_SETTINGS).await?;

    let before = manager.find_by_id(tenant.id).await?;
    let before_policy = PasswordPolicy::resolve(&state.config, before.as_ref());

    // Judge the request by what it would actually mean once resolved: an
    // override of null is not a weak setting, it is the deployment's own.
    let candidate = apply(before.clone(), &req);
    let requested = PasswordPolicy::resolve(&state.config, candidate.as_ref());
    requested.check_override(&state.config)?;

    let updated = manager
        .set_password_policy(tenant.id, overrides(&req))
        .await?;
    audit
        .0
        .updated(
            "tenant",
            tenant.id,
            &serde_json::json!({ "password_policy": before_policy }),
            &serde_json::json!({ "password_policy": requested }),
        )
        .await;
    Ok(Json(response(&state, updated.name.clone(), Some(&updated))))
}

fn response(
    state: &AuthState,
    tenant_name: String,
    row: Option<&crate::tenancy::tenant::Model>,
) -> PasswordPolicyResponse {
    PasswordPolicyResponse {
        tenant: tenant_name,
        policy: PasswordPolicy::resolve(&state.config, row),
        floor: PasswordPolicy::from_config(&state.config),
        overrides: row.map(view).unwrap_or_default(),
    }
}

fn view(row: &crate::tenancy::tenant::Model) -> PasswordPolicyOverridesView {
    PasswordPolicyOverridesView {
        min_length: row.password_min_length,
        require_uppercase: row.password_require_uppercase,
        require_lowercase: row.password_require_lowercase,
        require_digit: row.password_require_digit,
        require_symbol: row.password_require_symbol,
        expiry_days: row.password_expiry_days,
        history_count: row.password_history_count,
        lockout_max_failed: row.lockout_max_failed,
        lockout_secs: row.lockout_secs,
    }
}

fn overrides(req: &PasswordPolicyOverridesView) -> PasswordPolicyOverrides {
    PasswordPolicyOverrides {
        min_length: req.min_length,
        require_uppercase: req.require_uppercase,
        require_lowercase: req.require_lowercase,
        require_digit: req.require_digit,
        require_symbol: req.require_symbol,
        expiry_days: req.expiry_days,
        history_count: req.history_count,
        lockout_max_failed: req.lockout_max_failed,
        lockout_secs: req.lockout_secs,
    }
}

/// The tenant row as it would be with the request applied, so the policy
/// can be resolved and vetted before anything is written.
fn apply(
    row: Option<crate::tenancy::tenant::Model>,
    req: &PasswordPolicyOverridesView,
) -> Option<crate::tenancy::tenant::Model> {
    let mut row = row?;
    row.password_min_length = req.min_length;
    row.password_require_uppercase = req.require_uppercase;
    row.password_require_lowercase = req.require_lowercase;
    row.password_require_digit = req.require_digit;
    row.password_require_symbol = req.require_symbol;
    row.password_expiry_days = req.expiry_days;
    row.password_history_count = req.history_count;
    row.lockout_max_failed = req.lockout_max_failed;
    row.lockout_secs = req.lockout_secs;
    Some(row)
}
