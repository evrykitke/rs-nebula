//! POS Hourly Sales: the shape of the day — when the shop actually takes
//! its money. Hours are till-local when the caller passes `tz_offset`
//! (minutes east of UTC, e.g. 180 for Nairobi); UTC otherwise.

use super::queries::{HourlyView, PosQueries};
use super::{money, window};
use crate::scm::pos::permissions::names;
use nebula::error::Error;
use nebula::{
    Column as ReportColumn, DataCx, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportOutput, Result, Table,
};
use std::sync::Arc;

const KEY: &str = "pos_hourly_sales";

pub struct HourlySalesDataSource;
#[async_trait::async_trait]
impl ReportDataSource for HourlySalesDataSource {
    fn key(&self) -> &'static str {
        KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = PosQueries::new(db.clone())
            .hourly(
                cx.params.date("from")?,
                cx.params.date("to")?,
                cx.params.parse("tz_offset")?.unwrap_or(0),
            )
            .await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct HourlySalesReport;
impl ReportDefinition for HourlySalesReport {
    fn name(&self) -> &'static str {
        "pos-hourly-sales"
    }
    fn title(&self) -> &'static str {
        "POS Hourly Sales"
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
        vec![Arc::new(HourlySalesDataSource)]
    }
    fn build(&self, data: &ReportData) -> Result<Report> {
        let view: HourlyView = data.get(KEY)?;
        let mut table = Table::new(vec![
            ReportColumn::new("Hour"),
            ReportColumn::number("Sales"),
            ReportColumn::number("Refunds"),
            ReportColumn::number("Gross"),
            ReportColumn::number("Refunded"),
            ReportColumn::number("Net"),
        ]);
        let mut sales = 0i64;
        let mut net = rust_decimal::Decimal::ZERO;
        for r in &view.rows {
            sales += r.sales;
            net += r.net_total;
            table = table.row([
                format!("{:02}:00–{:02}:59", r.hour, r.hour),
                r.sales.to_string(),
                if r.refunds == 0 {
                    String::new()
                } else {
                    r.refunds.to_string()
                },
                money(r.gross_sales),
                money(r.refund_total),
                money(r.net_total),
            ]);
        }
        table = table.totals([
            "Total".to_string(),
            sales.to_string(),
            String::new(),
            String::new(),
            String::new(),
            money(net),
        ]);
        let tz = if view.tz_offset_minutes == 0 {
            " (UTC hours)".to_string()
        } else {
            format!(" (UTC{:+} min)", view.tz_offset_minutes)
        };
        Ok(Report::new("POS Hourly Sales")
            .subtitle(format!(
                "Takings by hour of day{}{}",
                window(view.from, view.to),
                tz
            ))
            .with(table.into_widget()))
    }
}
