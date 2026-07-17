//! Sales reports: the accounts-receivable lens on the order-to-cash trail.
//!
//! - **AR Aging** — posted invoices with an open balance, bucketed by age.
//! - **Delivered-Not-Billed** — shipped but not yet invoiced (GRNI's twin).
//! - **Sales Register** — posted invoices in a window: net / tax / gross.
//! - **Sales Margins** — revenue against the COGS the deliveries booked.
//! - **AR Reconciliation** — the AR control account against the subledger.
//!
//! One report per file, as with the SCM documents; each states its own columns
//! and wording and nothing else. The arithmetic they share is in [`queries`],
//! which also backs the JSON endpoints below.
//!
//! The customer statement is not here: it is a document a customer receives,
//! not a report about the business, so it lives in
//! [`crate::scm::sales::documents::customer_statement`]. Its data still comes
//! from [`queries`], and its JSON endpoint is below with the rest.

pub mod ar_aging;
pub mod ar_reconciliation;
pub mod delivered_not_billed;
pub mod queries;
pub mod sales_margins;
pub mod sales_register;

pub use ar_aging::ArAgingReport;
pub use ar_reconciliation::ArReconciliationReport;
pub use delivered_not_billed::DeliveredNotBilledReport;
pub use queries::{
    ArAgingRow, ArAgingView, ArReconciliationView, DnbRow, DnbView, MarginRow, MarginsView,
    RegisterRow, RegisterView, SalesQueries, StatementLine, StatementView,
};
pub use sales_margins::SalesMarginsReport;
pub use sales_register::SalesRegisterReport;

use crate::scm::sales::permissions::names;
use axum::extract::{Path, Query};
use axum::routing::get;
use axum::{Json, Router};
use nebula::auth::Authz;
use nebula::{Result, TenantDb};
use rust_decimal::Decimal;
use serde::Deserialize;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Shared formatting
// ---------------------------------------------------------------------------

/// Blank for zero, otherwise two decimals — the accounting convention.
pub(crate) fn money(amount: Decimal) -> String {
    if amount.is_zero() {
        String::new()
    } else {
        format!("{:.2}", amount)
    }
}

/// The window a report covers, as a subtitle clause — empty when unbounded,
/// so an unfiltered report says nothing rather than "from ∞".
pub(crate) fn window(from: Option<chrono::NaiveDate>, to: Option<chrono::NaiveDate>) -> String {
    match (from, to) {
        (Some(f), Some(t)) => format!(", {f} to {t}"),
        (Some(f), None) => format!(", from {f}"),
        (None, Some(t)) => format!(", to {t}"),
        (None, None) => String::new(),
    }
}

/// Quantities print trimmed; zero prints blank.
pub(crate) fn qty(v: Decimal) -> String {
    if v.is_zero() {
        String::new()
    } else {
        v.normalize().to_string()
    }
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AsOfQuery {
    /// The aging cut-off; defaults to today.
    pub as_of: Option<chrono::NaiveDate>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RegisterQuery {
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
    pub customer_id: Option<Uuid>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct WindowQuery {
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StatementQuery {
    pub from: chrono::NaiveDate,
    pub to: chrono::NaiveDate,
}

pub(crate) fn routes() -> Router {
    Router::new()
        .route("/sales/reports/ar-aging", get(ar_aging_json))
        .route("/sales/reports/delivered-not-billed", get(dnb_json))
        .route("/sales/reports/register", get(register_json))
        .route("/sales/reports/margins", get(margins_json))
        .route("/sales/reports/ar-reconciliation", get(ar_recon_json))
        .route("/sales/customers/{id}/statement", get(statement_json))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    ar_aging_json,
    dnb_json,
    register_json,
    margins_json,
    ar_recon_json,
    statement_json
))]
struct ApiDoc;

#[utoipa::path(get, path = "/sales/reports/ar-aging", tag = "sales",
    params(AsOfQuery), responses((status = 200, body = ArAgingView)))]
async fn ar_aging_json(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<AsOfQuery>,
) -> Result<Json<ArAgingView>> {
    authz.require(names::REPORTS_VIEW).await?;
    let as_of = q.as_of.unwrap_or_else(|| chrono::Utc::now().date_naive());
    SalesQueries::new(db).ar_aging(as_of).await.map(Json)
}

#[utoipa::path(get, path = "/sales/reports/delivered-not-billed", tag = "sales",
    responses((status = 200, body = DnbView)))]
async fn dnb_json(authz: Authz, TenantDb(db): TenantDb) -> Result<Json<DnbView>> {
    authz.require(names::REPORTS_VIEW).await?;
    SalesQueries::new(db).delivered_not_billed().await.map(Json)
}

#[utoipa::path(get, path = "/sales/reports/register", tag = "sales",
    params(RegisterQuery), responses((status = 200, body = RegisterView)))]
async fn register_json(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<RegisterQuery>,
) -> Result<Json<RegisterView>> {
    authz.require(names::REPORTS_VIEW).await?;
    SalesQueries::new(db)
        .register(q.from, q.to, q.customer_id)
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/sales/reports/margins", tag = "sales",
    params(WindowQuery), responses((status = 200, body = MarginsView)))]
async fn margins_json(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<WindowQuery>,
) -> Result<Json<MarginsView>> {
    authz.require(names::REPORTS_VIEW).await?;
    SalesQueries::new(db).margins(q.from, q.to).await.map(Json)
}

#[utoipa::path(get, path = "/sales/reports/ar-reconciliation", tag = "sales",
    responses((status = 200, body = ArReconciliationView)))]
async fn ar_recon_json(authz: Authz, TenantDb(db): TenantDb) -> Result<Json<ArReconciliationView>> {
    authz.require(names::REPORTS_VIEW).await?;
    SalesQueries::new(db).ar_reconciliation().await.map(Json)
}

#[utoipa::path(get, path = "/sales/customers/{id}/statement", tag = "sales",
    params(("id" = Uuid, Path, description = "Customer id"), StatementQuery),
    responses((status = 200, body = StatementView)))]
async fn statement_json(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Query(q): Query<StatementQuery>,
) -> Result<Json<StatementView>> {
    authz.require(names::REPORTS_VIEW).await?;
    SalesQueries::new(db)
        .customer_statement(id, q.from, q.to)
        .await
        .map(Json)
}
