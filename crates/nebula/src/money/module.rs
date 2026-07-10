//! Currency endpoints:
//!
//! - `GET /currencies` — the full list, anonymous: onboarding shows a
//!   currency picker before any account exists, and codes are not secrets
//! - `POST /currencies` — add a deployment-specific currency
//! - `DELETE /currencies/{id}` — remove one; system currencies are
//!   seeded reference data and refuse deletion
//!
//! Rows live in the main database — the currency list is shared by the
//! whole deployment, tenants only choose their default from it. New
//! currencies join the in-memory [`super::CurrencyRegistry`] on the next
//! restart; the list endpoints always read the live table.

use super::currency::{self, NewCurrency, Store};
use crate::audit::Audit;
use crate::auth::Authz;
use crate::auth::permission;
use crate::error::Result;
use crate::module::{Module, ModuleContext};
use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use sea_orm::DatabaseConnection;
use serde::Deserialize;
use uuid::Uuid;

pub struct CurrencyModule;

#[derive(Clone)]
struct CurrencyState {
    main_db: DatabaseConnection,
}

impl Module for CurrencyModule {
    fn name(&self) -> &'static str {
        "currencies"
    }

    fn configure(&self, ctx: &mut ModuleContext) {
        let state = CurrencyState {
            main_db: ctx.require_db(),
        };
        ctx.add_api(crate::module::build_openapi(|| {
            <ApiDoc as utoipa::OpenApi>::openapi()
        }));
        ctx.add_routes(
            Router::new()
                .route("/currencies", get(list_currencies).post(create_currency))
                .route("/currencies/{id}", axum::routing::delete(delete_currency))
                .with_state(state),
        );
    }
}

/// The currency module's OpenAPI contribution — the source client
/// generators (NSwag) build the `currency` service proxy from.
#[derive(utoipa::OpenApi)]
#[openapi(paths(list_currencies, create_currency, delete_currency))]
struct ApiDoc;

/// Anonymous on purpose: the onboarding form needs the list before a
/// tenant or user exists.
#[utoipa::path(get, path = "/currencies", tag = "currency",
    responses((status = 200, body = Vec<currency::Model>)))]
async fn list_currencies(
    State(state): State<CurrencyState>,
) -> Result<Json<Vec<currency::Model>>> {
    Store::new(state.main_db.clone()).find_all().await.map(Json)
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateCurrencyRequest {
    /// Three uppercase ASCII letters (ISO 4217 or an app-defined unit).
    pub code: String,
    pub name: String,
    /// Decimal places of the minor unit, 0-6.
    pub minor_units: i16,
}

/// Deployment-wide: the currency joins every tenant's picker.
#[utoipa::path(post, path = "/currencies", tag = "currency",
    request_body = CreateCurrencyRequest,
    responses((status = 200, body = currency::Model)))]
async fn create_currency(
    State(state): State<CurrencyState>,
    authz: Authz,
    audit: Audit,
    Json(req): Json<CreateCurrencyRequest>,
) -> Result<Json<currency::Model>> {
    authz.require(permission::names::TENANT_SETTINGS).await?;
    let row = Store::new(state.main_db.clone())
        .create(NewCurrency {
            code: req.code,
            name: req.name,
            minor_units: req.minor_units,
        })
        .await?;
    audit.0.created("currency", row.id, &row).await;
    Ok(Json(row))
}

#[utoipa::path(delete, path = "/currencies/{id}", tag = "currency",
    params(("id" = Uuid, Path, description = "Currency id")),
    responses((status = 200, body = currency::Model)))]
async fn delete_currency(
    State(state): State<CurrencyState>,
    authz: Authz,
    audit: Audit,
    Path(id): Path<Uuid>,
) -> Result<Json<currency::Model>> {
    authz.require(permission::names::TENANT_SETTINGS).await?;
    let row = Store::new(state.main_db.clone()).delete(id).await?;
    audit.0.deleted("currency", row.id, &row).await;
    Ok(Json(row))
}
