//! Supplier scorecards: delivery performance, graded from the paper trail.
//!
//! On-time percentage (receipt date against the order line's expected date),
//! rejection rate (`rejected_qty` against what was delivered), return rate
//! (posted returns against receipts), order→receipt lead time, invoice price
//! variance against the PO, and how much a supplier's PO pricing drifts per
//! item across receipts. All of it from what was already posted — nobody has to
//! keep a second set of records for a supplier to be measured.

use super::queries::{ProcurementQueries, SupplierScorecardView};
use super::{money, opt_num, opt_pct};
use crate::scm::procurement::permissions::names;
use nebula::error::Error;
use nebula::{
    Column as ReportColumn, DataCx, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportOutput, Result, Table,
};
use std::sync::Arc;

const SUPPLIER_SCORECARD_KEY: &str = "scm_supplier_scorecards";

pub struct SupplierScorecardDataSource;

#[async_trait::async_trait]
impl ReportDataSource for SupplierScorecardDataSource {
    fn key(&self) -> &'static str {
        SUPPLIER_SCORECARD_KEY
    }

    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = ProcurementQueries::new(db.clone())
            .supplier_scorecards(None, None)
            .await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct SupplierScorecardReport;

impl ReportDefinition for SupplierScorecardReport {
    fn name(&self) -> &'static str {
        "supplier-scorecards"
    }

    fn title(&self) -> &'static str {
        "Supplier Scorecards"
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
        vec![Arc::new(SupplierScorecardDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let view: SupplierScorecardView = data.get(SUPPLIER_SCORECARD_KEY)?;

        let mut table = Table::new(vec![
            ReportColumn::new("Code"),
            ReportColumn::new("Supplier"),
            ReportColumn::number("Orders"),
            ReportColumn::number("Receipts"),
            ReportColumn::number("Invoices"),
            ReportColumn::number("On-time %"),
            ReportColumn::number("Reject %"),
            ReportColumn::number("Return %"),
            ReportColumn::number("Lead days"),
            ReportColumn::number("Price var %"),
            ReportColumn::number("Price drift %"),
            ReportColumn::number("Received value"),
        ]);

        for row in &view.rows {
            table = table.row([
                row.code.clone(),
                row.name.clone(),
                row.orders.to_string(),
                row.receipts.to_string(),
                row.invoices.to_string(),
                opt_pct(row.on_time_pct),
                opt_pct(row.rejection_pct),
                opt_pct(row.return_pct),
                opt_num(row.avg_lead_days),
                opt_pct(row.price_variance_pct),
                opt_pct(row.price_range_pct),
                money(row.received_value_base),
            ]);
        }

        Ok(Report::new("Supplier Scorecards")
            .subtitle(
                "Delivery, quality and pricing performance from posted receipts, invoices and returns",
            )
            .with(table.into_widget()))
    }
}
