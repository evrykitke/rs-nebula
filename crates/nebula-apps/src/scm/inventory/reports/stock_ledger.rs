//! Stock Ledger: the most recent movements across every item and warehouse.

use super::{money, qty};
use crate::scm::inventory::levels::{LedgerFilter, LedgerRowView, StockQueries};
use crate::scm::inventory::permissions::names;
use nebula::{
    Column, DataCx, Report, ReportData, ReportDataSource, ReportDefinition, ReportOutput, Result,
    Table,
};
use std::sync::Arc;

const KEY: &str = "scm_stock_ledger";

/// The most recent ledger rows the whole-history report shows.
const LEDGER_REPORT_ROWS: u64 = 500;

/// The most recent ledger rows across every item and warehouse.
pub struct StockLedgerDataSource;

#[async_trait::async_trait]
impl ReportDataSource for StockLedgerDataSource {
    fn key(&self) -> &'static str {
        KEY
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
        let rows: Vec<LedgerRowView> = data.get(KEY)?;

        let mut table = Table::new(vec![
            Column::new("Date"),
            Column::new("Document"),
            Column::new("SKU"),
            Column::new("Warehouse"),
            Column::number("Qty"),
            Column::number("Balance"),
            Column::number("Unit cost"),
            Column::number("Value"),
        ]);

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
            .subtitle(format!(
                "Oldest {LEDGER_REPORT_ROWS} entries at most, in posting order"
            ))
            .with(table.into_widget()))
    }
}
