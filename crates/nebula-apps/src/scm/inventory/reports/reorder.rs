//! Reorder Advice: positions at or below their effective reorder level.
//!
//! What the auto-reorder worker acts on, in a form a buyer can read first.

use super::qty;
use crate::scm::inventory::levels::{LevelView, LevelsFilter, StockQueries};
use crate::scm::inventory::permissions::names;
use nebula::{
    Column, DataCx, Report, ReportData, ReportDataSource, ReportDefinition, ReportOutput, Result,
    Table,
};
use std::sync::Arc;

const KEY: &str = "scm_reorder";

/// Positions at or below their effective reorder level.
pub struct ReorderDataSource;

#[async_trait::async_trait]
impl ReportDataSource for ReorderDataSource {
    fn key(&self) -> &'static str {
        KEY
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
        let levels: Vec<LevelView> = data.get(KEY)?;

        let mut table = Table::new(vec![
            Column::new("SKU"),
            Column::new("Item"),
            Column::new("Warehouse"),
            Column::number("On hand"),
            Column::number("On order"),
            Column::number("Reorder level"),
            Column::number("Reorder qty"),
        ]);

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
