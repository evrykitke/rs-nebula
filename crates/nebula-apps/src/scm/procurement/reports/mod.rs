//! Procurement reports: the buying trail, read back.
//!
//! - **GRNI** — goods received, not invoiced, by supplier.
//! - **Supplier balances** — what has been billed, per supplier.
//! - **Supplier scorecards** — delivery performance, graded from the paper
//!   trail alone.
//!
//! One report per file, as with the SCM documents; each states its own columns
//! and wording and nothing else. The arithmetic they share is in [`queries`],
//! which also backs the JSON endpoints below — so `/procurement/reports/*` and
//! the PDF can never disagree about a number.

pub mod grni;
pub mod queries;
pub mod supplier_balances;
pub mod supplier_scorecard;

pub use grni::GrniReport;
pub use queries::{
    GrniRow, GrniView, ProcurementQueries, SupplierBalanceRow, SupplierBalancesView,
    SupplierScorecardRow, SupplierScorecardView,
};
pub use supplier_balances::SupplierBalancesReport;
pub use supplier_scorecard::SupplierScorecardReport;

use crate::scm::procurement::permissions::names;
use axum::extract::Query;
use axum::routing::get;
use axum::{Json, Router};
use nebula::auth::Authz;
use nebula::{Result, TenantDb};
use rust_decimal::Decimal;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Shared formatting
// ---------------------------------------------------------------------------

/// A percentage that may be uncomputable — blank when `None`.
pub(crate) fn opt_pct(v: Option<Decimal>) -> String {
    v.map(|v| format!("{:.2}", v)).unwrap_or_default()
}

/// A number that may be uncomputable — blank when `None`.
pub(crate) fn opt_num(v: Option<Decimal>) -> String {
    v.map(|v| v.normalize().to_string()).unwrap_or_default()
}

/// Quantities print trimmed; zero prints blank.
pub(crate) fn qty(v: Decimal) -> String {
    if v.is_zero() {
        String::new()
    } else {
        v.normalize().to_string()
    }
}

/// Blank for zero, otherwise two decimals — the accounting convention.
pub(crate) fn money(amount: Decimal) -> String {
    if amount.is_zero() {
        String::new()
    } else {
        format!("{:.2}", amount)
    }
}

// ---------------------------------------------------------------------------
// HTTP surface (JSON views of the same queries)
// ---------------------------------------------------------------------------

pub(crate) fn routes() -> Router {
    Router::new()
        .route("/procurement/reports/grni", get(grni_json))
        .route(
            "/procurement/reports/supplier-balances",
            get(supplier_balances_json),
        )
        .route(
            "/procurement/reports/supplier-scorecards",
            get(supplier_scorecards_json),
        )
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(grni_json, supplier_balances_json, supplier_scorecards_json))]
struct ApiDoc;

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ScorecardQuery {
    /// Document date window, inclusive; open-ended when omitted.
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

#[utoipa::path(get, path = "/procurement/reports/grni", tag = "procurement",
    responses((status = 200, body = GrniView)))]
async fn grni_json(authz: Authz, TenantDb(db): TenantDb) -> Result<Json<GrniView>> {
    authz.require(names::REPORTS_VIEW).await?;
    ProcurementQueries::new(db).grni().await.map(Json)
}

#[utoipa::path(get, path = "/procurement/reports/supplier-balances", tag = "procurement",
    responses((status = 200, body = SupplierBalancesView)))]
async fn supplier_balances_json(
    authz: Authz,
    TenantDb(db): TenantDb,
) -> Result<Json<SupplierBalancesView>> {
    authz.require(names::REPORTS_VIEW).await?;
    ProcurementQueries::new(db)
        .supplier_balances()
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/procurement/reports/supplier-scorecards", tag = "procurement",
    params(ScorecardQuery),
    responses((status = 200, body = SupplierScorecardView)))]
async fn supplier_scorecards_json(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<ScorecardQuery>,
) -> Result<Json<SupplierScorecardView>> {
    authz.require(names::REPORTS_VIEW).await?;
    ProcurementQueries::new(db)
        .supplier_scorecards(q.from, q.to)
        .await
        .map(Json)
}
