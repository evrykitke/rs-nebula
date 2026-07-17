//! Stock Balance: every item × warehouse position with stock or value.

use super::{money, qty};
use crate::scm::inventory::levels::{LevelView, LevelsFilter, StockQueries};
use crate::scm::inventory::permissions::names;
use nebula::{
    Column, DataCx, Report, ReportData, ReportDataSource, ReportDefinition, ReportOutput, Result,
    Table,
};
use rust_decimal::Decimal;
use std::sync::Arc;

const KEY: &str = "scm_stock_balance";

/// Every item × warehouse position with stock or value.
pub struct StockBalanceDataSource;

#[async_trait::async_trait]
impl ReportDataSource for StockBalanceDataSource {
    fn key(&self) -> &'static str {
        KEY
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
        let levels: Vec<LevelView> = data.get(KEY)?;

        let mut table = Table::new(vec![
            Column::new("SKU"),
            Column::new("Item"),
            Column::new("Warehouse"),
            Column::number("On hand"),
            Column::number("On order"),
            Column::number("Avg cost"),
            Column::number("Value"),
        ]);

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
