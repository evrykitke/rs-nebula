//! Sales Margins: invoice revenue against the true moving-average COGS the
//! deliveries booked — the stock ledger's own `value_delta`, per item, not a
//! standard cost anybody had to maintain.

use super::queries::{MarginsView, SalesQueries};
use super::{money, window};
use crate::scm::sales::permissions::names;
use nebula::error::Error;
use nebula::{
    Column as ReportColumn, DataCx, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportOutput, Result, Table,
};
use std::sync::Arc;

const MARGINS_KEY: &str = "scm_sales_margins";

pub struct MarginsDataSource;
#[async_trait::async_trait]
impl ReportDataSource for MarginsDataSource {
    fn key(&self) -> &'static str {
        MARGINS_KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = SalesQueries::new(db.clone())
            .margins(cx.params.date("from")?, cx.params.date("to")?)
            .await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct SalesMarginsReport;
impl ReportDefinition for SalesMarginsReport {
    fn name(&self) -> &'static str {
        "sales-margins"
    }
    fn title(&self) -> &'static str {
        "Sales Margins"
    }
    fn group(&self) -> &'static str {
        "Sales"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf, ReportOutput::Excel, ReportOutput::Table]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(MarginsDataSource)]
    }
    fn build(&self, data: &ReportData) -> Result<Report> {
        let view: MarginsView = data.get(MARGINS_KEY)?;
        let mut table = Table::new(vec![
            ReportColumn::new("SKU"),
            ReportColumn::new("Item"),
            ReportColumn::number("Revenue"),
            ReportColumn::number("COGS"),
            ReportColumn::number("Margin"),
            ReportColumn::number("Margin %"),
        ]);
        for r in &view.rows {
            table = table.row([
                r.sku.clone(),
                r.item_name.clone(),
                money(r.revenue),
                money(r.cogs),
                money(r.margin),
                r.margin_pct
                    .map(|v| format!("{:.2}", v))
                    .unwrap_or_default(),
            ]);
        }
        table = table.totals([
            String::new(),
            "Total".to_string(),
            money(view.revenue),
            money(view.cogs),
            money(view.margin),
            String::new(),
        ]);
        Ok(Report::new("Sales Margins")
            .subtitle(format!(
                "Invoiced revenue against the moving-average COGS deliveries booked{}",
                window(view.from, view.to)
            ))
            .with(table.into_widget()))
    }
}
