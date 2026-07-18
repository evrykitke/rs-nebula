//! POS reports: the back-office lens on the tills.
//!
//! - **POS Sessions** — every session in a window: takings, variance, tempo.
//! - **Tender Mix** — how the money arrived: cash, M-Pesa, card.
//! - **Item Sales** — what sold, net of refunds, best sellers first.
//! - **Hourly Sales** — the shape of the day, by hour.
//! - **Z Report** — one closed session, printable on the letterhead.
//!
//! One report per file, as everywhere in scm; the arithmetic they share is
//! in [`queries`], which also backs the JSON endpoints below — so the API
//! and the PDF can never disagree.

pub mod hourly_sales;
pub mod item_sales;
pub mod queries;
pub mod session_summary;
pub mod tender_mix;
pub mod z_report;

pub use hourly_sales::HourlySalesReport;
pub use item_sales::ItemSalesReport;
pub use queries::{
    HourlyRow, HourlyView, ItemSalesRow, ItemSalesView, PosQueries, SessionSummaryRow,
    SessionSummaryView, TenderMixRow, TenderMixView, TenderSheet, ZItemRow, ZView,
};
pub use session_summary::SessionSummaryReport;
pub use tender_mix::TenderMixReport;
pub use z_report::ZReportDocument;

use crate::scm::pos::permissions::names;
use axum::extract::Query;
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

/// Quantities print trimmed; zero prints blank.
pub(crate) fn qty(v: Decimal) -> String {
    if v.is_zero() {
        String::new()
    } else {
        v.normalize().to_string()
    }
}

/// The window a report covers, as a subtitle clause — empty when unbounded.
pub(crate) fn window(from: Option<chrono::NaiveDate>, to: Option<chrono::NaiveDate>) -> String {
    match (from, to) {
        (Some(f), Some(t)) => format!(", {f} to {t}"),
        (Some(f), None) => format!(", from {f}"),
        (None, Some(t)) => format!(", to {t}"),
        (None, None) => String::new(),
    }
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SessionsQuery {
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
    pub register_id: Option<Uuid>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct WindowQuery {
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct HourlyQuery {
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
    /// Minutes east of UTC to bucket hours in (e.g. 180 for Nairobi).
    pub tz_offset: Option<i32>,
}

pub(crate) fn routes() -> Router {
    Router::new()
        .route("/pos/reports/sessions", get(sessions_json))
        .route("/pos/reports/tender-mix", get(tender_mix_json))
        .route("/pos/reports/item-sales", get(item_sales_json))
        .route("/pos/reports/hourly", get(hourly_json))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(sessions_json, tender_mix_json, item_sales_json, hourly_json))]
struct ApiDoc;

#[utoipa::path(get, path = "/pos/reports/sessions", tag = "pos",
    params(SessionsQuery), responses((status = 200, body = SessionSummaryView)))]
async fn sessions_json(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<SessionsQuery>,
) -> Result<Json<SessionSummaryView>> {
    authz.require(names::REPORTS_VIEW).await?;
    PosQueries::new(db)
        .sessions(q.from, q.to, q.register_id)
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/pos/reports/tender-mix", tag = "pos",
    params(WindowQuery), responses((status = 200, body = TenderMixView)))]
async fn tender_mix_json(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<WindowQuery>,
) -> Result<Json<TenderMixView>> {
    authz.require(names::REPORTS_VIEW).await?;
    PosQueries::new(db).tender_mix(q.from, q.to).await.map(Json)
}

#[utoipa::path(get, path = "/pos/reports/item-sales", tag = "pos",
    params(WindowQuery), responses((status = 200, body = ItemSalesView)))]
async fn item_sales_json(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<WindowQuery>,
) -> Result<Json<ItemSalesView>> {
    authz.require(names::REPORTS_VIEW).await?;
    PosQueries::new(db).item_sales(q.from, q.to).await.map(Json)
}

#[utoipa::path(get, path = "/pos/reports/hourly", tag = "pos",
    params(HourlyQuery), responses((status = 200, body = HourlyView)))]
async fn hourly_json(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<HourlyQuery>,
) -> Result<Json<HourlyView>> {
    authz.require(names::REPORTS_VIEW).await?;
    PosQueries::new(db)
        .hourly(q.from, q.to, q.tz_offset.unwrap_or(0))
        .await
        .map(Json)
}
