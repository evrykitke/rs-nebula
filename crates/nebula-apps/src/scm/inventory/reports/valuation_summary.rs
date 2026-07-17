//! Stock Valuation: moving-average value by warehouse.

use super::money;
use crate::scm::inventory::levels::{StockQueries, ValuationSummary};
use crate::scm::inventory::permissions::names;
use nebula::{
    Column, DataCx, Report, ReportData, ReportDataSource, ReportDefinition, ReportOutput, Result,
    Table,
};
use std::sync::Arc;

const KEY: &str = "scm_valuation_summary";

pub struct ValuationSummaryDataSource;

#[async_trait::async_trait]
impl ReportDataSource for ValuationSummaryDataSource {
    fn key(&self) -> &'static str {
        KEY
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
        let summary: ValuationSummary = data.get(KEY)?;

        let mut table = Table::new(vec![
            Column::new("Warehouse"),
            Column::new("Name"),
            Column::number("Items"),
            Column::number("Value"),
        ]);

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
