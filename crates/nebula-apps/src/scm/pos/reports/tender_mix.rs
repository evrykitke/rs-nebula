//! Tender Mix: how the window's money arrived — cash, M-Pesa, card.

use super::queries::{PosQueries, TenderMixView};
use super::{money, window};
use crate::scm::pos::permissions::names;
use nebula::error::Error;
use nebula::{
    Column as ReportColumn, DataCx, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportOutput, Result, Table,
};
use std::sync::Arc;

const KEY: &str = "pos_tender_mix";

pub struct TenderMixDataSource;
#[async_trait::async_trait]
impl ReportDataSource for TenderMixDataSource {
    fn key(&self) -> &'static str {
        KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = PosQueries::new(db.clone())
            .tender_mix(cx.params.date("from")?, cx.params.date("to")?)
            .await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct TenderMixReport;
impl ReportDefinition for TenderMixReport {
    fn name(&self) -> &'static str {
        "pos-tender-mix"
    }
    fn title(&self) -> &'static str {
        "POS Tender Mix"
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
        vec![Arc::new(TenderMixDataSource)]
    }
    fn build(&self, data: &ReportData) -> Result<Report> {
        let view: TenderMixView = data.get(KEY)?;
        let mut table = Table::new(vec![
            ReportColumn::new("Tender"),
            ReportColumn::number("Payments"),
            ReportColumn::number("Sales"),
            ReportColumn::number("Refunds"),
            ReportColumn::number("Net"),
            ReportColumn::number("Share %"),
        ]);
        for r in &view.rows {
            table = table.row([
                r.tender.clone(),
                r.payments.to_string(),
                money(r.sales),
                money(r.refunds),
                money(r.net),
                r.share_pct.map(|p| p.to_string()).unwrap_or_default(),
            ]);
        }
        table = table.totals([
            "Total".to_string(),
            String::new(),
            String::new(),
            String::new(),
            money(view.net_total),
            String::new(),
        ]);
        Ok(Report::new("POS Tender Mix")
            .subtitle(format!("Takings by tender{}", window(view.from, view.to)))
            .with(table.into_widget()))
    }
}
