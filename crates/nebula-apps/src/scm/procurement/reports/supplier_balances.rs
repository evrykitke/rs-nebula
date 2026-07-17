//! Supplier balances: what has been billed, per supplier.
//!
//! Posted invoices only. Payments arrive with accounting's payment phase; until
//! then the balance is simply what has been billed.

use super::money;
use super::queries::{ProcurementQueries, SupplierBalancesView};
use crate::scm::procurement::permissions::names;
use nebula::error::Error;
use nebula::{
    Column as ReportColumn, DataCx, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportOutput, Result, Table,
};
use std::sync::Arc;

const SUPPLIER_BALANCES_KEY: &str = "scm_supplier_balances";

pub struct SupplierBalancesDataSource;

#[async_trait::async_trait]
impl ReportDataSource for SupplierBalancesDataSource {
    fn key(&self) -> &'static str {
        SUPPLIER_BALANCES_KEY
    }

    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = ProcurementQueries::new(db.clone())
            .supplier_balances()
            .await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct SupplierBalancesReport;

impl ReportDefinition for SupplierBalancesReport {
    fn name(&self) -> &'static str {
        "supplier-balances"
    }

    fn title(&self) -> &'static str {
        "Supplier Balances"
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
        vec![Arc::new(SupplierBalancesDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let view: SupplierBalancesView = data.get(SUPPLIER_BALANCES_KEY)?;

        let mut table = Table::new(vec![
            ReportColumn::new("Code"),
            ReportColumn::new("Supplier"),
            ReportColumn::new("Currency"),
            ReportColumn::number("Invoices"),
            ReportColumn::number("Balance"),
            ReportColumn::number("Base balance"),
        ]);

        for row in &view.rows {
            table = table.row([
                row.code.clone(),
                row.name.clone(),
                row.currency.clone(),
                row.invoices.to_string(),
                money(row.balance),
                money(row.base_balance),
            ]);
        }
        table = table.totals([
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            "Total".to_string(),
            money(view.total_base),
        ]);

        Ok(Report::new("Supplier Balances")
            .subtitle("Posted purchase invoices per supplier (payments not yet in scope)")
            .with(table.into_widget()))
    }
}
