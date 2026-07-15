//! Procurement reports: GRNI, supplier balances, supplier scorecards.
//!
//! GRNI (goods received, not invoiced) needs no extra tables — it is
//! `(received_qty − billed_qty) × effective price × exchange rate` over
//! open order lines, grouped by supplier. Once perpetual GL posting
//! exists, the GRNI *account* should equal this report; reconciling the
//! two is a health check. Supplier balances list posted invoices per
//! supplier (payments arrive with accounting's payment phase; until then
//! the balance is simply what has been billed).
//!
//! The supplier scorecard grades delivery performance from the posted
//! paper trail alone: on-time percentage (receipt date against the order
//! line's expected date), rejection rate (`rejected_qty` against what was
//! delivered), return rate (posted returns against receipts), order→
//! receipt lead time, invoice price variance against the PO, and how much
//! a supplier's PO pricing drifts per item across receipts.
//!
//! All render through the framework engine (PDF/Excel/table) and are
//! also served as JSON under `/procurement/reports/*` for the client.

use crate::scm::inventory::item::item;
use crate::scm::inventory::stock;
use crate::scm::procurement::invoice::{invoice, invoice_line};
use crate::scm::procurement::order::{effective_price, order, order_line};
use crate::scm::procurement::permissions::names;
use crate::scm::procurement::receipt::{receipt, receipt_line};
use crate::scm::procurement::returns::{preturn, return_line};
use crate::scm::procurement::supplier::supplier;
use axum::extract::Query;
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
const SUPPLIER_SCORECARD_KEY: &str = "scm_supplier_scorecards";

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

/// One supplier's performance grades. Percentages and averages are
/// `None` when the window holds no data to compute them from.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SupplierScorecardRow {
    pub supplier_id: Uuid,
    pub code: String,
    pub name: String,
    /// Orders placed (submitted or beyond) in the window.
    pub orders: i64,
    /// Goods receipts posted in the window.
    pub receipts: i64,
    /// Purchase invoices posted in the window.
    pub invoices: i64,
    /// Received quantity × PO price × rate — base currency.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub received_value_base: Decimal,
    /// Posted invoice line value at each invoice's rate — base currency.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub billed_value_base: Decimal,
    /// Receipt lines arriving on or before their expected date, as a
    /// percentage of lines that had one.
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub on_time_pct: Option<Decimal>,
    /// Rejected quantity over delivered quantity (accepted + rejected).
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub rejection_pct: Option<Decimal>,
    /// Returned quantity over received quantity.
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub return_pct: Option<Decimal>,
    /// Average days from order date to goods receipt.
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub avg_lead_days: Option<Decimal>,
    /// Average days from order date to invoice date.
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub avg_bill_days: Option<Decimal>,
    /// Value-weighted percentage the supplier billed above (+) or below
    /// (−) the PO price.
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub price_variance_pct: Option<Decimal>,
    /// Average per-item spread of PO prices across receipts — how much
    /// this supplier's pricing drifts order to order.
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub price_range_pct: Option<Decimal>,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SupplierScorecardView {
    #[schema(value_type = Option<String>, format = Date)]
    pub from: Option<chrono::NaiveDate>,
    #[schema(value_type = Option<String>, format = Date)]
    pub to: Option<chrono::NaiveDate>,
    pub rows: Vec<SupplierScorecardRow>,
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

    /// Grade every supplier with activity in the window from the posted
    /// paper trail: orders by order date, receipts / invoices / returns by
    /// their document dates. Reversed documents and their mirrors are
    /// excluded so a receipt taken back stops counting.
    pub async fn supplier_scorecards(
        &self,
        from: Option<chrono::NaiveDate>,
        to: Option<chrono::NaiveDate>,
    ) -> Result<SupplierScorecardView> {
        let in_window = |d: chrono::NaiveDate| {
            from.is_none_or(|f| d >= f) && to.is_none_or(|t| d <= t)
        };

        // The full order map: receipts and invoices in the window may
        // reference orders placed before it.
        let orders: HashMap<Uuid, order::Model> = order::Entity::find()
            .all(&self.db)
            .await?
            .into_iter()
            .map(|o| (o.id, o))
            .collect();

        let mut receipts = Vec::new();
        for r in receipt::Entity::find()
            .filter(receipt::Column::Status.eq("posted"))
            .filter(receipt::Column::ReversesId.is_null())
            .all(&self.db)
            .await?
        {
            if in_window(r.receipt_date) {
                receipts.push(r);
            }
        }
        let receipt_ids: Vec<Uuid> = receipts.iter().map(|r| r.id).collect();
        let receipt_lines = receipt_line::Entity::find()
            .filter(receipt_line::Column::ReceiptId.is_in(receipt_ids))
            .all(&self.db)
            .await?;

        let mut invoices = Vec::new();
        for i in invoice::Entity::find()
            .filter(invoice::Column::Status.eq("posted"))
            .all(&self.db)
            .await?
        {
            if in_window(i.invoice_date) {
                invoices.push(i);
            }
        }
        let invoice_ids: Vec<Uuid> = invoices.iter().map(|i| i.id).collect();
        let invoice_lines = invoice_line::Entity::find()
            .filter(invoice_line::Column::InvoiceId.is_in(invoice_ids))
            .all(&self.db)
            .await?;

        let mut returns = Vec::new();
        for r in preturn::Entity::find()
            .filter(preturn::Column::Status.eq("posted"))
            .filter(preturn::Column::ReversesId.is_null())
            .all(&self.db)
            .await?
        {
            if in_window(r.return_date) {
                returns.push(r);
            }
        }
        let return_ids: Vec<Uuid> = returns.iter().map(|r| r.id).collect();
        let return_lines = return_line::Entity::find()
            .filter(return_line::Column::ReturnId.is_in(return_ids))
            .all(&self.db)
            .await?;

        let mut order_line_ids: Vec<Uuid> = receipt_lines.iter().map(|l| l.order_line_id).collect();
        order_line_ids.extend(invoice_lines.iter().filter_map(|l| l.order_line_id));
        order_line_ids.extend(return_lines.iter().map(|l| l.order_line_id));
        let order_lines: HashMap<Uuid, order_line::Model> = order_line::Entity::find()
            .filter(order_line::Column::Id.is_in(order_line_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|l| (l.id, l))
            .collect();

        #[derive(Default)]
        struct Acc {
            orders: i64,
            receipts: i64,
            invoices: i64,
            received_qty: Decimal,
            received_value: Decimal,
            delivered_qty: Decimal,
            rejected_qty: Decimal,
            returned_qty: Decimal,
            on_time: i64,
            due: i64,
            lead_days: i64,
            lead_count: i64,
            bill_days: i64,
            bill_count: i64,
            billed_value: Decimal,
            variance_num: Decimal,
            variance_den: Decimal,
            // item -> the base-currency PO prices its receipts arrived at
            prices: HashMap<Uuid, Vec<Decimal>>,
        }
        let mut per_supplier: HashMap<Uuid, Acc> = HashMap::new();

        for o in orders.values() {
            if !matches!(o.status.as_str(), "draft" | "cancelled") && in_window(o.order_date) {
                per_supplier.entry(o.supplier_id).or_default().orders += 1;
            }
        }

        let receipts_by_id: HashMap<Uuid, &receipt::Model> =
            receipts.iter().map(|r| (r.id, r)).collect();
        for r in &receipts {
            let Some(order_row) = orders.get(&r.order_id) else {
                continue;
            };
            let acc = per_supplier.entry(order_row.supplier_id).or_default();
            acc.receipts += 1;
            acc.lead_days += (r.receipt_date - order_row.order_date).num_days();
            acc.lead_count += 1;
        }
        for l in &receipt_lines {
            let Some(r) = receipts_by_id.get(&l.receipt_id) else {
                continue;
            };
            let Some(order_row) = orders.get(&r.order_id) else {
                continue;
            };
            let acc = per_supplier.entry(order_row.supplier_id).or_default();
            acc.received_qty += l.qty;
            acc.delivered_qty += l.qty + l.rejected_qty;
            acc.rejected_qty += l.rejected_qty;
            if let Some(ol) = order_lines.get(&l.order_line_id) {
                let unit = stock::round_cost(
                    effective_price(ol.unit_price, ol.discount_pct) * order_row.exchange_rate,
                );
                acc.received_value += stock::round_money(l.qty * unit);
                acc.prices.entry(ol.item_id).or_default().push(unit);
                if let Some(expected) = ol.expected_date.or(order_row.expected_date) {
                    acc.due += 1;
                    if r.receipt_date <= expected {
                        acc.on_time += 1;
                    }
                }
            }
        }

        let invoices_by_id: HashMap<Uuid, &invoice::Model> =
            invoices.iter().map(|i| (i.id, i)).collect();
        for inv in &invoices {
            let acc = per_supplier.entry(inv.supplier_id).or_default();
            acc.invoices += 1;
            if let Some(order_row) = inv.order_id.and_then(|id| orders.get(&id)) {
                acc.bill_days += (inv.invoice_date - order_row.order_date).num_days();
                acc.bill_count += 1;
            }
        }
        for l in &invoice_lines {
            let Some(inv) = invoices_by_id.get(&l.invoice_id) else {
                continue;
            };
            let acc = per_supplier.entry(inv.supplier_id).or_default();
            let inv_unit = effective_price(l.unit_price, l.discount_pct);
            acc.billed_value +=
                stock::round_money(l.qty * stock::round_cost(inv_unit * inv.exchange_rate));
            if let Some(ol) = l.order_line_id.and_then(|id| order_lines.get(&id)) {
                let po_unit = effective_price(ol.unit_price, ol.discount_pct);
                acc.variance_num +=
                    stock::round_money(l.qty * (inv_unit - po_unit) * inv.exchange_rate);
                acc.variance_den += stock::round_money(l.qty * po_unit * inv.exchange_rate);
            }
        }

        let returns_by_id: HashMap<Uuid, &preturn::Model> =
            returns.iter().map(|r| (r.id, r)).collect();
        for l in &return_lines {
            let Some(r) = returns_by_id.get(&l.return_id) else {
                continue;
            };
            let Some(order_row) = orders.get(&r.order_id) else {
                continue;
            };
            per_supplier
                .entry(order_row.supplier_id)
                .or_default()
                .returned_qty += l.qty;
        }

        let supplier_ids: Vec<Uuid> = per_supplier.keys().copied().collect();
        let suppliers: HashMap<Uuid, supplier::Model> = supplier::Entity::find()
            .filter(supplier::Column::Id.is_in(supplier_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|s| (s.id, s))
            .collect();

        let hundred = Decimal::ONE_HUNDRED;
        let pct = |num: Decimal, den: Decimal| {
            (!den.is_zero()).then(|| (num / den * hundred).round_dp(2))
        };
        let avg_days = |sum: i64, count: i64| {
            (count > 0).then(|| (Decimal::from(sum) / Decimal::from(count)).round_dp(1))
        };

        let mut rows: Vec<SupplierScorecardRow> = per_supplier
            .into_iter()
            .map(|(supplier_id, acc)| {
                // Per item with two or more observed prices: spread over
                // average, averaged across items.
                let mut spreads = Vec::new();
                for prices in acc.prices.values().filter(|p| p.len() >= 2) {
                    let min = prices.iter().copied().min().unwrap_or_default();
                    let max = prices.iter().copied().max().unwrap_or_default();
                    let avg: Decimal =
                        prices.iter().copied().sum::<Decimal>() / Decimal::from(prices.len());
                    if !avg.is_zero() {
                        spreads.push((max - min) / avg * hundred);
                    }
                }
                let price_range_pct = (!spreads.is_empty()).then(|| {
                    (spreads.iter().copied().sum::<Decimal>() / Decimal::from(spreads.len()))
                        .round_dp(2)
                });
                let s = suppliers.get(&supplier_id);
                SupplierScorecardRow {
                    supplier_id,
                    code: s.map(|s| s.code.clone()).unwrap_or_default(),
                    name: s.map(|s| s.name.clone()).unwrap_or_default(),
                    orders: acc.orders,
                    receipts: acc.receipts,
                    invoices: acc.invoices,
                    received_value_base: acc.received_value,
                    billed_value_base: acc.billed_value,
                    on_time_pct: pct(Decimal::from(acc.on_time), Decimal::from(acc.due)),
                    rejection_pct: pct(acc.rejected_qty, acc.delivered_qty),
                    return_pct: pct(acc.returned_qty, acc.received_qty),
                    avg_lead_days: avg_days(acc.lead_days, acc.lead_count),
                    avg_bill_days: avg_days(acc.bill_days, acc.bill_count),
                    price_variance_pct: pct(acc.variance_num, acc.variance_den),
                    price_range_pct,
                }
            })
            .collect();
        rows.sort_by(|a, b| a.code.cmp(&b.code));
        Ok(SupplierScorecardView { from, to, rows })
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

pub struct SupplierScorecardDataSource;

#[async_trait::async_trait]
impl ReportDataSource for SupplierScorecardDataSource {
    fn key(&self) -> &'static str {
        SUPPLIER_SCORECARD_KEY
    }

    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = ProcurementQueries::new(db.clone())
            .supplier_scorecards(None, None)
            .await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct SupplierScorecardReport;

impl ReportDefinition for SupplierScorecardReport {
    fn name(&self) -> &'static str {
        "supplier-scorecards"
    }

    fn title(&self) -> &'static str {
        "Supplier Scorecards"
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
        vec![Arc::new(SupplierScorecardDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let view: SupplierScorecardView = data.get(SUPPLIER_SCORECARD_KEY)?;

        let mut table = Table::new(vec![
            ReportColumn::new("Code"),
            ReportColumn::new("Supplier"),
            ReportColumn::number("Orders"),
            ReportColumn::number("Receipts"),
            ReportColumn::number("Invoices"),
            ReportColumn::number("On-time %"),
            ReportColumn::number("Reject %"),
            ReportColumn::number("Return %"),
            ReportColumn::number("Lead days"),
            ReportColumn::number("Price var %"),
            ReportColumn::number("Price drift %"),
            ReportColumn::number("Received value"),
        ])
        .title("Supplier Scorecards");

        for row in &view.rows {
            table = table.row([
                row.code.clone(),
                row.name.clone(),
                row.orders.to_string(),
                row.receipts.to_string(),
                row.invoices.to_string(),
                opt_pct(row.on_time_pct),
                opt_pct(row.rejection_pct),
                opt_pct(row.return_pct),
                opt_num(row.avg_lead_days),
                opt_pct(row.price_variance_pct),
                opt_pct(row.price_range_pct),
                money(row.received_value_base),
            ]);
        }

        Ok(Report::new("Supplier Scorecards")
            .subtitle(
                "Delivery, quality and pricing performance from posted receipts, invoices and returns",
            )
            .with(table.into_widget()))
    }
}

/// A percentage that may be uncomputable — blank when `None`.
fn opt_pct(v: Option<Decimal>) -> String {
    v.map(|v| format!("{:.2}", v)).unwrap_or_default()
}

/// A number that may be uncomputable — blank when `None`.
fn opt_num(v: Option<Decimal>) -> String {
    v.map(|v| v.normalize().to_string()).unwrap_or_default()
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