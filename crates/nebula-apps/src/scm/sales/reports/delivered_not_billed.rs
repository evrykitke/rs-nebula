//! Delivered-Not-Billed: order lines shipped but not yet invoiced, valued at
//! the effective price. The outbound twin of GRNI.

use super::{money, qty};
use super::queries::{DnbView, SalesQueries};
use crate::scm::sales::permissions::names;
use nebula::error::Error;
use nebula::{
    Column as ReportColumn, DataCx, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportOutput, Result, Table,
};
use std::sync::Arc;

const DNB_KEY: &str = "scm_delivered_not_billed";

pub struct DnbDataSource;
#[async_trait::async_trait]
impl ReportDataSource for DnbDataSource {
    fn key(&self) -> &'static str {
        DNB_KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = SalesQueries::new(db.clone()).delivered_not_billed().await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct DeliveredNotBilledReport;
impl ReportDefinition for DeliveredNotBilledReport {
    fn name(&self) -> &'static str {
        "delivered-not-billed"
    }
    fn title(&self) -> &'static str {
        "Delivered Not Billed"
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
        vec![Arc::new(DnbDataSource)]
    }
    fn build(&self, data: &ReportData) -> Result<Report> {
        let view: DnbView = data.get(DNB_KEY)?;
        let mut table = Table::new(vec![
            ReportColumn::new("Customer"),
            ReportColumn::new("Order"),
            ReportColumn::new("SKU"),
            ReportColumn::new("Item"),
            ReportColumn::number("Qty"),
            ReportColumn::number("Unit price"),
            ReportColumn::number("Value"),
        ]);
        for r in &view.rows {
            table = table.row([
                r.customer_name.clone(),
                r.order_number.clone().unwrap_or_default(),
                r.sku.clone(),
                r.item_name.clone(),
                qty(r.qty),
                money(r.unit_price),
                money(r.value),
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
        Ok(Report::new("Delivered Not Billed")
            .subtitle("Order lines shipped but not yet invoiced")
            .with(table.into_widget()))
    }
}
