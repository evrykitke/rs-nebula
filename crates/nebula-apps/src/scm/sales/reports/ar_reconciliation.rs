//! AR Reconciliation: the AR control account against Σ open invoice balances.
//!
//! The receivable sibling of the stock/GRNI health check — if these two differ,
//! the ledger and the subledger have drifted and every AR figure is suspect.

use super::queries::{ArReconciliationView, SalesQueries};
use rust_decimal::Decimal;
use crate::scm::sales::permissions::names;
use nebula::error::Error;
use nebula::{
    Column as ReportColumn, DataCx, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportOutput, Result, Table,
};
use std::sync::Arc;

const AR_RECON_KEY: &str = "scm_ar_reconciliation";

pub struct ArReconDataSource;
#[async_trait::async_trait]
impl ReportDataSource for ArReconDataSource {
    fn key(&self) -> &'static str {
        AR_RECON_KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = SalesQueries::new(db.clone()).ar_reconciliation().await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct ArReconciliationReport;
impl ReportDefinition for ArReconciliationReport {
    fn name(&self) -> &'static str {
        "ar-reconciliation"
    }
    fn title(&self) -> &'static str {
        "AR Reconciliation"
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
        vec![Arc::new(ArReconDataSource)]
    }
    fn build(&self, data: &ReportData) -> Result<Report> {
        let view: ArReconciliationView = data.get(AR_RECON_KEY)?;
        let opt = |v: Option<Decimal>| v.map(|v| format!("{:.2}", v)).unwrap_or_else(|| "—".into());
        let table = Table::new(vec![
            ReportColumn::new("Measure"),
            ReportColumn::number("Operational"),
            ReportColumn::number("GL balance"),
            ReportColumn::number("Gap"),
        ])
        .row([
            "Accounts receivable".to_string(),
            format!("{:.2}", view.ar_open),
            opt(view.ar_account_balance),
            opt(view.ar_gap),
        ])
        .row([
            "Pending GL requests".to_string(),
            view.pending_outbox.to_string(),
            String::new(),
            String::new(),
        ]);
        Ok(Report::new("AR Reconciliation")
            .subtitle("Open invoice balances against the AR control account, base currency")
            .with(table.into_widget()))
    }
}
