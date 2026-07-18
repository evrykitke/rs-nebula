//! POS Item Sales: what actually sold at the tills, best sellers first.

use super::queries::{ItemSalesView, PosQueries};
use super::{money, qty, window};
use crate::scm::pos::permissions::names;
use nebula::error::Error;
use nebula::{
    Column as ReportColumn, DataCx, Orientation, Report, ReportData, ReportDataSource,
    ReportDefinition, ReportOutput, Result, Table,
};
use std::sync::Arc;

const KEY: &str = "pos_item_sales";

pub struct ItemSalesDataSource;
#[async_trait::async_trait]
impl ReportDataSource for ItemSalesDataSource {
    fn key(&self) -> &'static str {
        KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = PosQueries::new(db.clone())
            .item_sales(cx.params.date("from")?, cx.params.date("to")?)
            .await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct ItemSalesReport;
impl ReportDefinition for ItemSalesReport {
    fn name(&self) -> &'static str {
        "pos-item-sales"
    }
    fn title(&self) -> &'static str {
        "POS Item Sales"
    }
    fn group(&self) -> &'static str {
        "Point of Sale"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf, ReportOutput::Excel, ReportOutput::Table]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(ItemSalesDataSource)]
    }
    fn build(&self, data: &ReportData) -> Result<Report> {
        let view: ItemSalesView = data.get(KEY)?;
        let mut table = Table::new(vec![
            ReportColumn::new("SKU"),
            ReportColumn::wide("Item"),
            ReportColumn::number("Sold"),
            ReportColumn::number("Refunded"),
            ReportColumn::number("Gross"),
            ReportColumn::number("VAT"),
            ReportColumn::number("Ex VAT"),
        ]);
        for r in &view.rows {
            table = table.row([
                r.sku.clone(),
                r.name.clone(),
                qty(r.qty_sold),
                qty(r.qty_refunded),
                money(r.gross),
                money(r.tax),
                money(r.net),
            ]);
        }
        table = table.totals([
            String::new(),
            "Total".to_string(),
            String::new(),
            String::new(),
            money(view.gross),
            money(view.tax),
            money(view.gross - view.tax),
        ]);
        Ok(Report::new("POS Item Sales")
            .subtitle(format!(
                "Till sales by item, net of refunds{}",
                window(view.from, view.to)
            ))
            .orientation(Orientation::Landscape)
            .with(table.into_widget()))
    }
}
