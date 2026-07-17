//! Income Statement: revenue against expenses over a window.

use super::{money, section_rows};
use crate::accounting::account::AccountType;
use crate::accounting::ledger::{IncomeStatement, LedgerQueries};
use crate::accounting::permissions::names;
use nebula::{
    Column, DataCx, Report, ReportData, ReportDataSource, ReportDefinition, ReportOutput, Result,
    Table,
};
use std::sync::Arc;

const KEY: &str = "accounting_income_statement";

/// Fetches the all-time income statement from the request's tenant database.
pub struct IncomeStatementDataSource;

#[async_trait::async_trait]
impl ReportDataSource for IncomeStatementDataSource {
    fn key(&self) -> &'static str {
        KEY
    }

    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let is = LedgerQueries::new(db.clone())
            .income_statement(None, None)
            .await?;
        serde_json::to_value(is).map_err(|e| nebula::Error::internal(e.to_string()))
    }
}

pub struct IncomeStatementReport;

impl ReportDefinition for IncomeStatementReport {
    fn name(&self) -> &'static str {
        "income-statement"
    }

    fn title(&self) -> &'static str {
        "Income Statement"
    }

    fn group(&self) -> &'static str {
        "Accounting"
    }

    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf, ReportOutput::Excel, ReportOutput::Table]
    }

    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }

    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(IncomeStatementDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let is: IncomeStatement = data.get(KEY)?;

        let mut table = Table::new(vec![Column::new("Account"), Column::number("Amount")]);

        table = section_rows(table, &is.revenue, AccountType::Revenue);
        table = section_rows(table, &is.expenses, AccountType::Expense);
        table = table.totals(["Net income".to_string(), money(is.net_income)]);

        let subtitle = match (is.from, is.to) {
            (Some(from), Some(to)) => format!("{from} to {to}"),
            (None, Some(to)) => format!("Through {to}"),
            (Some(from), None) => format!("From {from}"),
            (None, None) => "All posted entries".to_string(),
        };
        Ok(Report::new("Income Statement")
            .subtitle(subtitle)
            .with(table.into_widget()))
    }
}
