//! AR Aging: every posted invoice with an open balance, bucketed by how far
//! past its due date it is (current / 1–30 / 31–60 / 61–90 / 90+).

use super::money;
use super::queries::{ArAgingView, SalesQueries};
use crate::scm::sales::permissions::names;
use nebula::error::Error;
use nebula::{
    Column as ReportColumn, DataCx, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportOutput, Result, Table,
};
use std::sync::Arc;

const AR_AGING_KEY: &str = "scm_ar_aging";

pub struct ArAgingDataSource;
#[async_trait::async_trait]
impl ReportDataSource for ArAgingDataSource {
    fn key(&self) -> &'static str {
        AR_AGING_KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        // `?as_of=` ages the book at a past date — what the balances looked
        // like at a month end, not only today.
        let as_of = cx
            .params
            .date("as_of")?
            .unwrap_or_else(|| chrono::Utc::now().date_naive());
        let view = SalesQueries::new(db.clone()).ar_aging(as_of).await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct ArAgingReport;
impl ReportDefinition for ArAgingReport {
    fn name(&self) -> &'static str {
        "ar-aging"
    }
    fn title(&self) -> &'static str {
        "AR Aging"
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
        vec![Arc::new(ArAgingDataSource)]
    }
    fn build(&self, data: &ReportData) -> Result<Report> {
        let view: ArAgingView = data.get(AR_AGING_KEY)?;
        let mut table = Table::new(vec![
            ReportColumn::new("Code"),
            ReportColumn::new("Customer"),
            ReportColumn::number("Current"),
            ReportColumn::number("1–30"),
            ReportColumn::number("31–60"),
            ReportColumn::number("61–90"),
            ReportColumn::number("90+"),
            ReportColumn::number("Total"),
        ]);
        for r in &view.rows {
            table = table.row([
                r.code.clone(),
                r.name.clone(),
                money(r.current),
                money(r.d1_30),
                money(r.d31_60),
                money(r.d61_90),
                money(r.d90_plus),
                money(r.total),
            ]);
        }
        table = table.totals([
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            "Total".to_string(),
            money(view.total),
        ]);
        Ok(Report::new("AR Aging")
            .subtitle(format!(
                "Open customer balances by age of the due date, as of {}",
                view.as_of
            ))
            .with(table.into_widget()))
    }
}
