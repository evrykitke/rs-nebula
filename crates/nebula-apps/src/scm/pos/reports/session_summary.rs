//! POS Sessions: every session in a window — takings, variance, tempo.

use super::queries::{PosQueries, SessionSummaryView};
use super::{money, window};
use crate::scm::pos::permissions::names;
use nebula::error::Error;
use nebula::{
    Column as ReportColumn, DataCx, Orientation, Report, ReportData, ReportDataSource,
    ReportDefinition, ReportOutput, Result, Table,
};
use std::sync::Arc;

const KEY: &str = "pos_sessions";

pub struct SessionSummaryDataSource;
#[async_trait::async_trait]
impl ReportDataSource for SessionSummaryDataSource {
    fn key(&self) -> &'static str {
        KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = PosQueries::new(db.clone())
            .sessions(
                cx.params.date("from")?,
                cx.params.date("to")?,
                cx.params.parse("register_id")?,
            )
            .await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct SessionSummaryReport;
impl ReportDefinition for SessionSummaryReport {
    fn name(&self) -> &'static str {
        "pos-sessions"
    }
    fn title(&self) -> &'static str {
        "POS Sessions"
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
        vec![Arc::new(SessionSummaryDataSource)]
    }
    fn build(&self, data: &ReportData) -> Result<Report> {
        let view: SessionSummaryView = data.get(KEY)?;
        let mut table = Table::new(vec![
            ReportColumn::new("Session"),
            ReportColumn::new("Register"),
            ReportColumn::new("Opened"),
            ReportColumn::new("Status"),
            ReportColumn::number("Sales"),
            ReportColumn::number("Refunds"),
            ReportColumn::number("Voids"),
            ReportColumn::number("Gross"),
            ReportColumn::number("Refunded"),
            ReportColumn::number("Net"),
            ReportColumn::number("VAT"),
            ReportColumn::number("Cash +/-"),
            ReportColumn::number("Sec/Sale"),
        ]);
        for r in &view.rows {
            table = table.row([
                r.number.clone().unwrap_or_default(),
                r.register_code.clone(),
                r.opened_at.format("%Y-%m-%d %H:%M").to_string(),
                r.status.as_str().to_string(),
                r.orders.to_string(),
                if r.refunds == 0 {
                    String::new()
                } else {
                    r.refunds.to_string()
                },
                if r.voids == 0 {
                    String::new()
                } else {
                    r.voids.to_string()
                },
                money(r.gross_sales),
                money(r.refund_total),
                money(r.net_total),
                money(r.tax_total),
                r.cash_variance.map(money).unwrap_or_default(),
                r.avg_sale_seconds
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
            ]);
        }
        table = table.totals([
            String::new(),
            String::new(),
            String::new(),
            "Total".to_string(),
            String::new(),
            String::new(),
            String::new(),
            money(view.gross_sales),
            money(view.refund_total),
            money(view.net_total),
            money(view.tax_total),
            money(view.cash_variance),
            String::new(),
        ]);
        Ok(Report::new("POS Sessions")
            .subtitle(format!("Till sessions{}", window(view.from, view.to)))
            .orientation(Orientation::Landscape)
            .with(table.into_widget()))
    }
}
