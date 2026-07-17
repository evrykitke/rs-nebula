//! GRNI: goods received, not invoiced.
//!
//! `(received_qty − billed_qty) × effective price × exchange rate` over open
//! order lines, grouped by supplier — no extra tables needed. Once perpetual GL
//! posting exists, the GRNI *account* should equal this report; reconciling the
//! two is a health check.

use super::queries::{GrniView, ProcurementQueries};
use super::{money, qty};
use crate::scm::procurement::permissions::names;
use nebula::error::Error;
use nebula::{
    Column as ReportColumn, DataCx, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportOutput, Result, Table,
};
use std::sync::Arc;

const GRNI_KEY: &str = "scm_grni";

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
        ]);

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
