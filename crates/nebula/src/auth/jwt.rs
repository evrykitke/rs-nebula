//! Signed tokens (HS256) and the request extractor.
//!
//! Three purposes: `access` grants API access; `two_factor` only bridges
//! a successful password check to the TOTP step (or setup, when the
//! company mandates 2FA); `password_change` only bridges a fully proved
//! sign-in to the forced change an expired password demands. The bridge
//! purposes are rejected everywhere else. Tokens embed
//! the user's security stamp, and [`CurrentUser`] re-checks it against
//! the database — changing a password or disabling a user kills every
//! outstanding token immediately.
//!
//! A token also names the tenant it was issued for, and [`CurrentUser`]
//! refuses to spend it in any other — see [`ensure_token_matches_tenant`].

use super::user;
use crate::config::AuthConfig;
use crate::error::{Error, ProblemDetails, Result};
use crate::tenancy::TenantRef;
use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenPurpose {
    Access,
    TwoFactor,
    PasswordChange,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// User id.
    pub sub: uuid::Uuid,
    pub tenant_id: Option<uuid::Uuid>,
    pub purpose: TokenPurpose,
    /// Security stamp at issue time.
    pub stamp: String,
    pub exp: i64,
    pub iat: i64,
}

pub fn issue(config: &AuthConfig, user: &user::Model, purpose: TokenPurpose) -> Result<String> {
    let ttl = match purpose {
        TokenPurpose::Access => config.access_token_ttl_secs,
        // Both bridges are the same kind of thing — a few minutes to
        // finish signing in — so they share the one lifetime.
        TokenPurpose::TwoFactor | TokenPurpose::PasswordChange => config.two_factor_token_ttl_secs,
    };
    let now = Utc::now().timestamp();
    let claims = Claims {
        sub: user.id,
        tenant_id: user.tenant_id,
        purpose,
        stamp: user.security_stamp.clone(),
        exp: now + ttl as i64,
        iat: now,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(config.jwt_secret.expose().as_bytes()),
    )
    .map_err(|e| Error::internal(format!("token signing failed: {e}")))
}

pub fn verify(config: &AuthConfig, token: &str) -> Result<Claims> {
    decode::<Claims>(
        token,
        &DecodingKey::from_secret(config.jwt_secret.expose().as_bytes()),
        &Validation::default(),
    )
    .map(|data| data.claims)
    .map_err(|_| Error::Unauthorized)
}

/// Extractor: the authenticated user of the current request. Requires a
/// `Bearer` access token whose security stamp still matches the user row
/// in the request's database (tenant or main).
pub struct CurrentUser(pub user::Model);

impl<S: Send + Sync> FromRequestParts<S> for CurrentUser {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let claims = claims_from_parts(parts)?;
        if claims.purpose != TokenPurpose::Access {
            return Err(Error::Unauthorized.into_response());
        }
        let user = load_stamped_user(parts, &claims).await?;
        Ok(CurrentUser(user))
    }
}

/// Extractor for the two-factor bridge endpoints: accepts only
/// `two_factor`-purpose tokens.
pub struct TwoFactorUser(pub user::Model);

impl<S: Send + Sync> FromRequestParts<S> for TwoFactorUser {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let claims = claims_from_parts(parts)?;
        if claims.purpose != TokenPurpose::TwoFactor {
            return Err(Error::Unauthorized.into_response());
        }
        let user = load_stamped_user(parts, &claims).await?;
        Ok(TwoFactorUser(user))
    }
}

/// Extractor for the forced password change: accepts only
/// `password_change`-purpose tokens, which login issues in place of a
/// session when the password has aged out.
pub struct PasswordChangeUser(pub user::Model);

impl<S: Send + Sync> FromRequestParts<S> for PasswordChangeUser {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let claims = claims_from_parts(parts)?;
        if claims.purpose != TokenPurpose::PasswordChange {
            return Err(Error::Unauthorized.into_response());
        }
        let user = load_stamped_user(parts, &claims).await?;
        Ok(PasswordChangeUser(user))
    }
}

fn claims_from_parts(parts: &Parts) -> Result<Claims, Response> {
    let config = parts
        .extensions
        .get::<AuthConfig>()
        .cloned()
        .ok_or_else(|| {
            ProblemDetails::from_status(StatusCode::INTERNAL_SERVER_ERROR, None).into_response()
        })?;
    let token = parts
        .headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| Error::Unauthorized.into_response())?;
    verify(&config, token).map_err(|e| e.into_response())
}

async fn load_stamped_user(parts: &Parts, claims: &Claims) -> Result<user::Model, Response> {
    ensure_token_matches_tenant(parts, claims)?;
    let db = parts
        .extensions
        .get::<DatabaseConnection>()
        .cloned()
        .ok_or_else(|| Error::Unauthorized.into_response())?;
    use sea_orm::EntityTrait;
    let user = user::Entity::find_by_id(claims.sub)
        .one(&db)
        .await
        .map_err(|e| Error::from(e).into_response())?;
    let Some(user) = user else {
        return Err(Error::Unauthorized.into_response());
    };
    let valid = user.deleted_at.is_none()
        && user.is_active
        && user.security_stamp == claims.stamp
        && user.tenant_id == claims.tenant_id;
    if !valid {
        return Err(Error::Unauthorized.into_response());
    }
    Ok(user)
}

/// The token must be spent in the tenant it was issued for.
///
/// Tenant resolution trusts the request header, so without this a token
/// from tenant A presented with `X-Tenant: B` would be served against B's
/// data. Business modules carry no `tenant_id` on their rows — isolation
/// *is* the connection they are handed — so this is the check that makes
/// it hold, whether tenants sit in their own databases or share the main
/// one. A host-context token (no tenant) may not act as a tenant, and a
/// tenant's token may not act on the host.
fn ensure_token_matches_tenant(parts: &Parts, claims: &Claims) -> Result<(), Response> {
    let resolved = parts.extensions.get::<TenantRef>().map(|t| t.id);
    if resolved == claims.tenant_id {
        return Ok(());
    }
    tracing::warn!(
        user = %claims.sub,
        token_tenant = ?claims.tenant_id,
        request_tenant = ?resolved,
        "rejected a token presented against a tenant it was not issued for"
    );
    Err(Error::Forbidden.into_response())
}
