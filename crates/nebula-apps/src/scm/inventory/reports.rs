//! Inventory reports rendered through the framework reporting engine.
//!
//! All four read what the stock engine maintains, via the same
//! [`StockQueries`] that serve the JSON endpoints — so PDF, Excel and the
//! on-screen table always agree with the API. The engine's data sources
//! carry no parameters, so these are whole-position reports; filtered
//! views (one item's ledger, one warehouse) live on the interactive
//! `/inventory/stock/*` endpoints.

use crate::scm::inventory::levels::{
    LedgerFilter, LedgerRowView, LevelView, LevelsFilter, StockQueries, ValuationSummary,
};
use crate::scm::inventory::permissions::names;
use nebula::{
    Column, DataCx, Report, ReportData, ReportDataSource, ReportDefinition, ReportFormat,
    ReportOutput, Result, Table,
};
use rust_decimal::Decimal;
use std::sync::Arc;

const STOCK_BALANCE_KEY: &str = "scm_stock_balance";
const STOCK_LEDGER_KEY: &str = "scm_stock_ledger";
const VALUATION_KEY: &str = "scm_valuation_summary";
const REORDER_KEY: &str = "scm_reorder";

/// The most recent ledger rows the whole-history report shows.
const LEDGER_REPORT_ROWS: u64 = 500;

// ---------------------------------------------------------------------------
// Stock balance
// ---------------------------------------------------------------------------

/// Every item × warehouse position with stock or value.
pub struct StockBalanceDataSource;

#[async_trait::async_trait]
impl ReportDataSource for StockBalanceDataSource {
    fn key(&self) -> &'static str {
        STOCK_BALANCE_KEY
    }

    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let levels = StockQueries::new(db.clone())
            .levels(LevelsFilter {
                warehouse_id: None,
                item_id: None,
                below_reorder: false,
            })
            .await?;
        serde_json::to_value(levels).map_err(|e| nebula::Error::internal(e.to_string()))
    }
}

pub struct StockBalanceReport;

impl ReportDefinition for StockBalanceReport {
    fn name(&self) -> &'static str {
        "stock-balance"
    }

    fn title(&self) -> &'static str {
        "Stock Balance"
    }

    fn group(&self) -> &'static str {
        "Inventory"
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
        vec![Arc::new(StockBalanceDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let levels: Vec<LevelView> = data.get(STOCK_BALANCE_KEY)?;

        let mut table = Table::new(vec![
            Column::new("SKU"),
            Column::new("Item"),
            Column::new("Warehouse"),
            Column::number("On hand"),
            Column::number("On order"),
            Column::number("Avg cost"),
            Column::number("Value"),
        ])
        .title("Stock Balance");

        let mut total_value = Decimal::ZERO;
        for row in &levels {
            total_value += row.value;
            table = table.row([
                row.sku.clone(),
                row.item_name.clone(),
                row.warehouse_code.clone(),
                qty(row.on_hand),
                qty(row.on_order),
                money(row.avg_cost),
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
            money(total_value),
        ]);

        Ok(Report::new("Stock Balance")
            .subtitle("Current position, all warehouses")
            .with(table.into_widget()))
    }
}

// ---------------------------------------------------------------------------
// Stock ledger
// ---------------------------------------------------------------------------

/// The most recent ledger rows across every item and warehouse.
pub struct StockLedgerDataSource;

#[async_trait::async_trait]
impl ReportDataSource for StockLedgerDataSource {
    fn key(&self) -> &'static str {
        STOCK_LEDGER_KEY
    }

    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let rows = StockQueries::new(db.clone())
            .ledger(LedgerFilter {
                item_id: None,
                warehouse_id: None,
                from: None,
                to: None,
                after_seq: None,
                limit: Some(LEDGER_REPORT_ROWS),
            })
            .await?;
        serde_json::to_value(rows).map_err(|e| nebula::Error::internal(e.to_string()))
    }
}

pub struct StockLedgerReport;

impl ReportDefinition for StockLedgerReport {
    fn name(&self) -> &'static str {
        "stock-ledger"
    }

    fn title(&self) -> &'static str {
        "Stock Ledger"
    }

    fn group(&self) -> &'static str {
        "Inventory"
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
        vec![Arc::new(StockLedgerDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let rows: Vec<LedgerRowView> = data.get(STOCK_LEDGER_KEY)?;

        let mut table = Table::new(vec![
            Column::new("Date"),
            Column::new("Document"),
            Column::new("SKU"),
            Column::new("Warehouse"),
            Column::number("Qty"),
            Column::number("Balance"),
            Column::number("Unit cost"),
            Column::number("Value"),
        ])
        .title("Stock Ledger");

        for row in &rows {
            table = table.row([
                row.entry_date.to_string(),
                row.number.clone().unwrap_or_default(),
                row.sku.clone(),
                row.warehouse_code.clone(),
                qty(row.qty_delta),
                qty(row.qty_after),
                money(row.unit_cost),
                money(row.value_delta),
            ]);
        }

        Ok(Report::new("Stock Ledger")
            .subtitle(format!("Oldest {LEDGER_REPORT_ROWS} entries at most, in posting order"))
            .with(table.into_widget()))
    }
}

// ---------------------------------------------------------------------------
// Valuation summary
// ---------------------------------------------------------------------------

pub struct ValuationSummaryDataSource;

#[async_trait::async_trait]
impl ReportDataSource for ValuationSummaryDataSource {
    fn key(&self) -> &'static str {
        VALUATION_KEY
    }

    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let summary = StockQueries::new(db.clone()).valuation(None).await?;
        serde_json::to_value(summary).map_err(|e| nebula::Error::internal(e.to_string()))
    }
}

pub struct ValuationSummaryReport;

impl ReportDefinition for ValuationSummaryReport {
    fn name(&self) -> &'static str {
        "valuation-summary"
    }

    fn title(&self) -> &'static str {
        "Stock Valuation"
    }

    fn group(&self) -> &'static str {
        "Inventory"
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
        vec![Arc::new(ValuationSummaryDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let summary: ValuationSummary = data.get(VALUATION_KEY)?;

        let mut table = Table::new(vec![
            Column::new("Warehouse"),
            Column::new("Name"),
            Column::number("Items"),
            Column::number("Value"),
        ])
        .title("Stock Valuation");

        for row in &summary.warehouses {
            table = table.row([
                row.warehouse_code.clone(),
                row.warehouse_name.clone(),
                row.items.to_string(),
                money(row.total_value),
            ]);
        }
        table = table.totals([
            String::new(),
            String::new(),
            "Total".to_string(),
            money(summary.total_value),
        ]);

        Ok(Report::new("Stock Valuation")
            .subtitle("Moving-average value by warehouse")
            .with(table.into_widget()))
    }
}

// ---------------------------------------------------------------------------
// Reorder
// ---------------------------------------------------------------------------

/// Positions at or below their effective reorder level.
pub struct ReorderDataSource;

#[async_trait::async_trait]
impl ReportDataSource for ReorderDataSource {
    fn key(&self) -> &'static str {
        REORDER_KEY
    }

    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let levels = StockQueries::new(db.clone())
            .levels(LevelsFilter {
                warehouse_id: None,
                item_id: None,
                below_reorder: true,
            })
            .await?;
        serde_json::to_value(levels).map_err(|e| nebula::Error::internal(e.to_string()))
    }
}

pub struct ReorderReport;

impl ReportDefinition for ReorderReport {
    fn name(&self) -> &'static str {
        "reorder"
    }

    fn title(&self) -> &'static str {
        "Reorder Advice"
    }

    fn group(&self) -> &'static str {
        "Inventory"
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
        vec![Arc::new(ReorderDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let levels: Vec<LevelView> = data.get(REORDER_KEY)?;

        let mut table = Table::new(vec![
            Column::new("SKU"),
            Column::new("Item"),
            Column::new("Warehouse"),
            Column::number("On hand"),
            Column::number("On order"),
            Column::number("Reorder level"),
            Column::number("Reorder qty"),
        ])
        .title("Reorder Advice");

        for row in &levels {
            table = table.row([
                row.sku.clone(),
                row.item_name.clone(),
                row.warehouse_code.clone(),
                qty(row.on_hand),
                qty(row.on_order),
                row.reorder_level.map(qty).unwrap_or_default(),
                row.reorder_qty.map(qty).unwrap_or_default(),
            ]);
        }

        Ok(Report::new("Reorder Advice")
            .subtitle("Positions at or below their reorder level")
            .with(table.into_widget()))
    }
}

/// Quantities print trimmed (10, not 10.000000); zero prints blank.
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
