//! Balance Sheet: what the business owns against what it owes, as of a date.
//!
//! Current earnings sit below equity rather than inside it: they are the income
//! statement's net income, not a posted balance, and the sheet only foots once
//! they are counted.

use super::{money, section_rows, tone_of};
use crate::accounting::account::AccountType;
use crate::accounting::ledger::{BalanceSheet, LedgerQueries};
use crate::accounting::permissions::names;
use nebula::{
    Column, DataCx, Report, ReportData, ReportDataSource, ReportDefinition, ReportOutput, Result,
    Row, Table,
};
use std::sync::Arc;

const KEY: &str = "accounting_balance_sheet";

/// Fetches the balance sheet (as of today) from the request's tenant database.
pub struct BalanceSheetDataSource;

#[async_trait::async_trait]
impl ReportDataSource for BalanceSheetDataSource {
    fn key(&self) -> &'static str {
        KEY
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
        let bs: BalanceSheet = data.get(KEY)?;

        let mut table = Table::new(vec![Column::new("Account"), Column::number("Amount")]);

        table = section_rows(table, &bs.assets, AccountType::Asset);
        table = section_rows(table, &bs.liabilities, AccountType::Liability);
        table = section_rows(table, &bs.equity, AccountType::Equity);
        // Earnings are equity that has not been posted there yet, so they wear
        // equity's colour: the sheet only foots once they are counted, and a
        // reader tracing that has to see them as part of the same block.
        let equity = tone_of(AccountType::Equity);
        if !bs.prior_earnings.is_zero() {
            table = table.add(
                Row::new([
                    "Retained earnings (prior years)".to_string(),
                    money(bs.prior_earnings),
                ])
                .tone(equity),
            );
        }
        table = table.add(
            Row::new(["Current earnings".to_string(), money(bs.current_earnings)]).tone(equity),
        );
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
