//! Trial Balance: every account's ending balance in its natural column.
//!
//! The oldest check in bookkeeping — if the two columns do not foot to the same
//! total, something is wrong upstream of every other report here.

use super::{money, title_case, tone_of};
use crate::accounting::ledger::{LedgerQueries, TrialBalance};
use crate::accounting::permissions::names;
use nebula::{
    Column, DataCx, Report, ReportData, ReportDataSource, ReportDefinition, ReportOutput, Result,
    Row, Table,
};
use std::sync::Arc;

const KEY: &str = "accounting_trial_balance";

/// Fetches the whole-ledger trial balance from the request's tenant database.
pub struct TrialBalanceDataSource;

#[async_trait::async_trait]
impl ReportDataSource for TrialBalanceDataSource {
    fn key(&self) -> &'static str {
        KEY
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
        let tb: TrialBalance = data.get(KEY)?;

        let mut table = Table::new(vec![
            Column::new("Code"),
            Column::new("Account"),
            Column::center("Type"),
            Column::number("Debit"),
            Column::number("Credit"),
        ]);

        // Sorted by code, so the types interleave — the colour is what tells
        // an asset from a liability without reading the middle of every row.
        for row in &tb.rows {
            table = table.add(
                Row::new([
                    row.code.clone(),
                    row.name.clone(),
                    title_case(row.account_type.as_str()),
                    money(row.debit),
                    money(row.credit),
                ])
                .tone(tone_of(row.account_type)),
            );
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
