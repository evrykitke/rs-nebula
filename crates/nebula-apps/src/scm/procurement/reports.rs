//! Procurement reports: GRNI and supplier balances.
//!
//! GRNI (goods received, not invoiced) needs no extra tables — it is
//! `(received_qty − billed_qty) × effective price × exchange rate` over
//! open order lines, grouped by supplier. Once perpetual GL posting
//! exists, the GRNI *account* should equal this report; reconciling the
//! two is a health check. Supplier balances list posted invoices per
//! supplier (payments arrive with accounting's payment phase; until then
//! the balance is simply what has been billed).
//!
//! Both render through the framework engine (PDF/Excel/table) and are
//! also served as JSON under `/procurement/reports/*` for the client.

use crate::scm::inventory::item::item;
use crate::scm::inventory::stock;
use crate::scm::procurement::invoice::{invoice, invoice_line};
use crate::scm::procurement::order::{effective_price, order, order_line};
use crate::scm::procurement::permissions::names;
use crate::scm::procurement::supplier::supplier;
use axum::routing::get;
use axum::{Json, Router};
use nebula::auth::Authz;
use nebula::error::Error;
use nebula::{
    Column as ReportColumn, DataCx, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportFormat, ReportOutput, Result, Table, TenantDb, sea_orm,
};
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

const GRNI_KEY: &str = "scm_grni";
const SUPPLIER_BALANCES_KEY: &str = "scm_supplier_balances";

// ---------------------------------------------------------------------------
// Queries (shared by the JSON endpoints and the report engine)
// ---------------------------------------------------------------------------

/// One open order line on the GRNI position.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct GrniRow {
    pub supplier_id: Uuid,
    pub supplier_name: String,
    pub order_id: Uuid,
    pub order_number: Option<String>,
    pub item_id: Uuid,
    pub sku: String,
    pub item_name: String,
    /// Received and not yet billed, order-line UoM.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    /// Effective PO price × exchange rate — base currency.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub unit_value: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub value: Decimal,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct GrniView {
    pub rows: Vec<GrniRow>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total: Decimal,
}

/// One supplier's billed exposure.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SupplierBalanceRow {
    pub supplier_id: Uuid,
    pub code: String,
    pub name: String,
    pub currency: String,
    pub invoices: i64,
    /// Posted invoice totals in the supplier currency.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub balance: Decimal,
    /// The same, converted at each invoice's exchange rate.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub base_balance: Decimal,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SupplierBalancesView {
    pub rows: Vec<SupplierBalanceRow>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total_base: Decimal,
}

/// Read-side queries over the procurement tables.
pub struct ProcurementQueries {
    db: DatabaseConnection,
}

impl ProcurementQueries {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// The operational GRNI: every order line with more received than
    /// billed, valued at PO price × rate.
    pub async fn grni(&self) -> Result<GrniView> {
        let lines = order_line::Entity::find()
            .filter(
                Expr::col(order_line::Column::ReceivedQty)
                    .gt(Expr::col(order_line::Column::BilledQty)),
            )
            .all(&self.db)
            .await?;
        let order_ids: Vec<Uuid> = lines.iter().map(|l| l.order_id).collect();
        let orders: HashMap<Uuid, order::Model> = order::Entity::find()
            .filter(order::Column::Id.is_in(order_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|o| (o.id, o))
            .collect();
        let supplier_ids: Vec<Uuid> = orders.values().map(|o| o.supplier_id).collect();
        let suppliers: HashMap<Uuid, supplier::Model> = supplier::Entity::find()
            .filter(supplier::Column::Id.is_in(supplier_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|s| (s.id, s))
            .collect();
        let item_ids: Vec<Uuid> = lines.iter().map(|l| l.item_id).collect();
        let items: HashMap<Uuid, item::Model> = item::Entity::find()
            .filter(item::Column::Id.is_in(item_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|i| (i.id, i))
            .collect();

        let mut rows: Vec<GrniRow> = Vec::new();
        let mut total = Decimal::ZERO;
        for l in &lines {
            let Some(order_row) = orders.get(&l.order_id) else {
                continue;
            };
            let qty = l.received_qty - l.billed_qty;
            let unit_value = stock::round_cost(
                effective_price(l.unit_price, l.discount_pct) * order_row.exchange_rate,
            );
            let value = stock::round_money(qty * unit_value);
            total += value;
            let item = items.get(&l.item_id);
            let supplier_row = suppliers.get(&order_row.supplier_id);
            rows.push(GrniRow {
                supplier_id: order_row.supplier_id,
                supplier_name: supplier_row.map(|s| s.name.clone()).unwrap_or_default(),
                order_id: order_row.id,
                order_number: order_row.number.clone(),
                item_id: l.item_id,
                sku: item.map(|i| i.sku.clone()).unwrap_or_default(),
                item_name: item.map(|i| i.name.clone()).unwrap_or_default(),
                qty,
                unit_value,
                value,
            });
        }
        rows.sort_by(|a, b| {
            (a.supplier_name.as_str(), a.order_number.as_deref())
                .cmp(&(b.supplier_name.as_str(), b.order_number.as_deref()))
        });
        Ok(GrniView { rows, total })
    }

    /// Posted invoices summed per supplier, in supplier currency and in
    /// base at each invoice's rate.
    pub async fn supplier_balances(&self) -> Result<SupplierBalancesView> {
        let invoices = invoice::Entity::find()
            .filter(invoice::Column::Status.eq("posted"))
            .all(&self.db)
            .await?;
        let invoice_ids: Vec<Uuid> = invoices.iter().map(|i| i.id).collect();
        let mut lines_by_invoice: HashMap<Uuid, Vec<invoice_line::Model>> = HashMap::new();
        for l in invoice_line::Entity::find()
            .filter(invoice_line::Column::InvoiceId.is_in(invoice_ids))
            .all(&self.db)
            .await?
        {
            lines_by_invoice.entry(l.invoice_id).or_default().push(l);
        }
        let supplier_ids: Vec<Uuid> = invoices.iter().map(|i| i.supplier_id).collect();
        let suppliers: HashMap<Uuid, supplier::Model> = supplier::Entity::find()
            .filter(supplier::Column::Id.is_in(supplier_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|s| (s.id, s))
            .collect();

        let mut per_supplier: HashMap<Uuid, (i64, Decimal, Decimal)> = HashMap::new();
        for inv in &invoices {
            let mut subtotal = Decimal::ZERO;
            for l in lines_by_invoice.get(&inv.id).map(|v| v.as_slice()).unwrap_or(&[]) {
                subtotal += stock::round_money(l.qty * effective_price(l.unit_price, l.discount_pct));
            }
            let mut total = subtotal;
            if let Some(pct) = inv.discount_pct {
                total -= stock::round_money(subtotal * pct / Decimal::ONE_HUNDRED);
            }
            if let Some(amount) = inv.discount_amount {
                total -= amount;
            }
            if let Some(charges) = inv.other_charges {
                total += charges;
            }
            let total = stock::round_money(total);
            let entry = per_supplier.entry(inv.supplier_id).or_default();
            entry.0 += 1;
            entry.1 += total;
            entry.2 += stock::round_money(total * inv.exchange_rate);
        }

        let mut rows: Vec<SupplierBalanceRow> = per_supplier
            .into_iter()
            .map(|(supplier_id, (count, balance, base_balance))| {
                let s = suppliers.get(&supplier_id);
                SupplierBalanceRow {
                    supplier_id,
                    code: s.map(|s| s.code.clone()).unwrap_or_default(),
                    name: s.map(|s| s.name.clone()).unwrap_or_default(),
                    currency: s.map(|s| s.currency.clone()).unwrap_or_default(),
                    invoices: count,
                    balance,
                    base_balance,
                }
            })
            .collect();
        rows.sort_by(|a, b| a.code.cmp(&b.code));
        let total_base = rows.iter().map(|r| r.base_balance).sum();
        Ok(SupplierBalancesView { rows, total_base })
    }
}

// ---------------------------------------------------------------------------
// Framework reports
// ---------------------------------------------------------------------------

pub struct GrniDataSource;

#[async_trait::async_trait]
impl ReportDataSource for GrniDataSource {
    fn key(&self) -> &'static str {
        GRNI_KEY
    }

    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = ProcurementQueries::new(db.clone()).grni().await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct GrniReport;

impl ReportDefinition for GrniReport {
    fn name(&self) -> &'static str {
        "grni"
    }

    fn title(&self) -> &'static str {
        "Goods Received Not Invoiced"
    }

    fn group(&self) -> &'static str {
        "Procurement"
    }

    fn default_format(&self) -> ReportFormat {
        ReportFormat::Compact
    }

    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf, ReportOutput::Excel, ReportOutput::Table]
    }

    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }

    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(GrniDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let view: GrniView = data.get(GRNI_KEY)?;

        let mut table = Table::new(vec![
            ReportColumn::new("Supplier"),
            ReportColumn::new("Order"),
            ReportColumn::new("SKU"),
            ReportColumn::new("Item"),
            ReportColumn::number("Qty"),
            ReportColumn::number("Unit value"),
            ReportColumn::number("Value"),
        ])
        .title("Goods Received Not Invoiced");

        for row in &view.rows {
            table = table.row([
                row.supplier_name.clone(),
                row.order_number.clone().unwrap_or_default(),
                row.sku.clone(),
                row.item_name.clone(),
                qty(row.qty),
                money(row.unit_value),
                money(row.value),
            ]);
        }
        table = table.totals([
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            "Total".to_string(),
            money(view.total),
        ]);

        Ok(Report::new("Goods Received Not Invoiced")
            .subtitle("Open order lines received but not yet billed, base currency")
            .with(table.into_widget()))
    }
}

pub struct SupplierBalancesDataSource;

#[async_trait::async_trait]
impl ReportDataSource for SupplierBalancesDataSource {
    fn key(&self) -> &'static str {
        SUPPLIER_BALANCES_KEY
    }

    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = ProcurementQueries::new(db.clone()).supplier_balances().await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct SupplierBalancesReport;

impl ReportDefinition for SupplierBalancesReport {
    fn name(&self) -> &'static str {
        "supplier-balances"
    }

    fn title(&self) -> &'static str {
        "Supplier Balances"
    }

    fn group(&self) -> &'static str {
        "Procurement"
    }

    fn default_format(&self) -> ReportFormat {
        ReportFormat::Compact
    }

    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf, ReportOutput::Excel, ReportOutput::Table]
    }

    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }

    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(SupplierBalancesDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let view: SupplierBalancesView = data.get(SUPPLIER_BALANCES_KEY)?;

        let mut table = Table::new(vec![
            ReportColumn::new("Code"),
            ReportColumn::new("Supplier"),
            ReportColumn::new("Currency"),
            ReportColumn::number("Invoices"),
            ReportColumn::number("Balance"),
            ReportColumn::number("Base balance"),
        ])
        .title("Supplier Balances");

        for row in &view.rows {
            table = table.row([
                row.code.clone(),
                row.name.clone(),
                row.currency.clone(),
                row.invoices.to_string(),
                money(row.balance),
                money(row.base_balance),
            ]);
        }
        table = table.totals([
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            "Total".to_string(),
            money(view.total_base),
        ]);

        Ok(Report::new("Supplier Balances")
            .subtitle("Posted purchase invoices per supplier (payments not yet in scope)")
            .with(table.into_widget()))
    }
}

/// Quantities print trimmed; zero prints blank.
fn qty(v: Decimal) -> String {
    if v.is_zero() {
        String::new()
    } else {
        v.normalize().to_string()
    }
}

/// Blank for zero, otherwise two decimals — the accounting convention.
fn money(amount: Decimal) -> String {
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
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(grni_json, supplier_balances_json))]
struct ApiDoc;

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
