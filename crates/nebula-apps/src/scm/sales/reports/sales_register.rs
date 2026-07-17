//! Sales Register: posted invoices in a window — net, tax and gross.

use super::queries::{RegisterView, SalesQueries};
use super::{money, window};
use crate::scm::sales::permissions::names;
use nebula::error::Error;
use nebula::{
    Column as ReportColumn, DataCx, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportOutput, Result, Table,
};
use std::sync::Arc;

const REGISTER_KEY: &str = "scm_sales_register";

pub struct RegisterDataSource;
#[async_trait::async_trait]
impl ReportDataSource for RegisterDataSource {
    fn key(&self) -> &'static str {
        REGISTER_KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = SalesQueries::new(db.clone())
            .register(
                cx.params.date("from")?,
                cx.params.date("to")?,
                cx.params.parse("customer_id")?,
            )
            .await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct SalesRegisterReport;
impl ReportDefinition for SalesRegisterReport {
    fn name(&self) -> &'static str {
        "sales-register"
    }
    fn title(&self) -> &'static str {
        "Sales Register"
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
        vec![Arc::new(RegisterDataSource)]
    }
    fn build(&self, data: &ReportData) -> Result<Report> {
        let view: RegisterView = data.get(REGISTER_KEY)?;
        let mut table = Table::new(vec![
            ReportColumn::new("Number"),
            ReportColumn::new("Date"),
            ReportColumn::new("Customer"),
            ReportColumn::number("Net"),
            ReportColumn::number("Tax"),
            ReportColumn::number("Total"),
        ]);
        for r in &view.rows {
            table = table.row([
                r.number.clone().unwrap_or_default(),
                r.invoice_date.to_string(),
                r.customer_name.clone(),
                money(r.net),
                money(r.tax),
                money(r.total),
            ]);
        }
        table = table.totals([
            String::new(),
            String::new(),
            "Total".to_string(),
            money(view.net),
            money(view.tax),
            money(view.total),
        ]);
        Ok(Report::new("Sales Register")
            .subtitle(format!(
                "Posted sales invoices{}",
                window(view.from, view.to)
            ))
            .with(table.into_widget()))
    }
}
