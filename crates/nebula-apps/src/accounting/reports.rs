//! Accounting reports rendered through the framework reporting engine.
//!
//! `TrialBalanceReport` lists every account's ending balance in its
//! natural debit or credit column; the two columns foot to the same
//! total. Its data comes from [`TrialBalanceDataSource`], which reads the
//! request's tenant ledger — so the same report serves PDF, Excel and the
//! interactive on-screen table.

use crate::accounting::ledger::{
    BalanceSheet, IncomeStatement, LedgerQueries, StatementSection, TrialBalance,
};
use crate::accounting::permissions::names;
use nebula::{
    Column, DataCx, Report, ReportData, ReportDataSource, ReportDefinition, ReportOutput, Result,
    Table,
};
use rust_decimal::Decimal;
use std::sync::Arc;

const TRIAL_BALANCE_KEY: &str = "accounting_trial_balance";
const BALANCE_SHEET_KEY: &str = "accounting_balance_sheet";
const INCOME_STATEMENT_KEY: &str = "accounting_income_statement";

/// Fetches the whole-ledger trial balance from the request's tenant
/// database.
pub struct TrialBalanceDataSource;

#[async_trait::async_trait]
impl ReportDataSource for TrialBalanceDataSource {
    fn key(&self) -> &'static str {
        TRIAL_BALANCE_KEY
    }

    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let tb = LedgerQueries::new(db.clone()).trial_balance(None).await?;
        serde_json::to_value(tb).map_err(|e| nebula::Error::internal(e.to_string()))
    }
}

pub struct TrialBalanceReport;

impl ReportDefinition for TrialBalanceReport {
    fn name(&self) -> &'static str {
        "trial-balance"
    }

    fn title(&self) -> &'static str {
        "Trial Balance"
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
        vec![Arc::new(TrialBalanceDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let tb: TrialBalance = data.get(TRIAL_BALANCE_KEY)?;

        let mut table = Table::new(vec![
            Column::new("Code"),
            Column::new("Account"),
            Column::center("Type"),
            Column::number("Debit"),
            Column::number("Credit"),
        ]);

        for row in &tb.rows {
            table = table.row([
                row.code.clone(),
                row.name.clone(),
                title_case(row.account_type.as_str()),
                money(row.debit),
                money(row.credit),
            ]);
        }
        table = table.totals([
            String::new(),
            String::new(),
            "Total".to_string(),
            money(tb.total_debit),
            money(tb.total_credit),
        ]);

        let subtitle = match tb.as_of {
            Some(date) => format!("As of {date}"),
            None => "All posted entries".to_string(),
        };
        Ok(Report::new("Trial Balance")
            .subtitle(subtitle)
            .with(table.into_widget()))
    }
}

// ---------------------------------------------------------------------------
// Balance sheet
// ---------------------------------------------------------------------------

/// Fetches the balance sheet (as of today) from the request's tenant database.
pub struct BalanceSheetDataSource;

#[async_trait::async_trait]
impl ReportDataSource for BalanceSheetDataSource {
    fn key(&self) -> &'static str {
        BALANCE_SHEET_KEY
    }

    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let bs = LedgerQueries::new(db.clone()).balance_sheet(None).await?;
        serde_json::to_value(bs).map_err(|e| nebula::Error::internal(e.to_string()))
    }
}

pub struct BalanceSheetReport;

impl ReportDefinition for BalanceSheetReport {
    fn name(&self) -> &'static str {
        "balance-sheet"
    }

    fn title(&self) -> &'static str {
        "Balance Sheet"
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
        vec![Arc::new(BalanceSheetDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let bs: BalanceSheet = data.get(BALANCE_SHEET_KEY)?;

        let mut table = Table::new(vec![Column::new("Account"), Column::number("Amount")]);

        table = section_rows(table, &bs.assets);
        table = section_rows(table, &bs.liabilities);
        table = section_rows(table, &bs.equity);
        if !bs.prior_earnings.is_zero() {
            table = table.row([
                "Retained earnings (prior years)".to_string(),
                money(bs.prior_earnings),
            ]);
        }
        table = table.row(["Current earnings".to_string(), money(bs.current_earnings)]);
        table = table.totals([
            "Total liabilities & equity".to_string(),
            money(bs.total_liabilities_and_equity),
        ]);

        let subtitle = match bs.as_of {
            Some(date) => format!("As of {date}"),
            None => "As of today".to_string(),
        };
        Ok(Report::new("Balance Sheet")
            .subtitle(subtitle)
            .with(table.into_widget()))
    }
}

// ---------------------------------------------------------------------------
// Income statement
// ---------------------------------------------------------------------------

/// Fetches the all-time income statement from the request's tenant database.
pub struct IncomeStatementDataSource;

#[async_trait::async_trait]
impl ReportDataSource for IncomeStatementDataSource {
    fn key(&self) -> &'static str {
        INCOME_STATEMENT_KEY
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
        let is: IncomeStatement = data.get(INCOME_STATEMENT_KEY)?;

        let mut table = Table::new(vec![Column::new("Account"), Column::number("Amount")]);

        table = section_rows(table, &is.revenue);
        table = section_rows(table, &is.expenses);
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

/// Render a statement section as a header row, its lines, and a subtotal row.
fn section_rows(mut table: Table, section: &StatementSection) -> Table {
    table = table.row([section.title.clone(), String::new()]);
    for line in &section.lines {
        table = table.row([format!("  {} {}", line.code, line.name), money(line.amount)]);
    }
    table.row([
        format!("Total {}", section.title.to_lowercase()),
        money(section.total),
    ])
}

/// Blank for zero, otherwise the amount to two decimals — the accounting
/// convention that keeps the columns scannable.
fn money(amount: Decimal) -> String {
    if amount.is_zero() {
        String::new()
    } else {
        format!("{:.2}", amount)
    }
}

fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
