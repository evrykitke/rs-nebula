//! Accounting reports rendered through the framework reporting engine.
//!
//! `TrialBalanceReport` lists every account's ending balance in its
//! natural debit or credit column; the two columns foot to the same
//! total. Its data comes from [`TrialBalanceDataSource`], which reads the
//! request's tenant ledger — so the same report serves PDF, Excel and the
//! interactive on-screen table.

use crate::accounting::ledger::{LedgerQueries, TrialBalance};
use crate::accounting::permissions::names;
use nebula::{
    Column, DataCx, Report, ReportData, ReportDataSource, ReportDefinition, ReportFormat,
    ReportOutput, Result, Table,
};
use rust_decimal::Decimal;
use std::sync::Arc;

const TRIAL_BALANCE_KEY: &str = "accounting_trial_balance";

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

    fn default_format(&self) -> ReportFormat {
        ReportFormat::Compact
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
        ])
        .title("Trial Balance");

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
