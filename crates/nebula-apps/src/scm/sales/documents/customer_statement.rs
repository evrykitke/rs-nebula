//! The statement of account: everything that moved a customer's balance over a
//! period, and what they owe at the end of it.
//!
//! The one SCM document that is not an instrument. An invoice is issued once
//! and stands; a statement is a view of an account between two dates, so it
//! carries no number, and the period it covers is the whole of its identity.

use super::{Addressed, Party, party_block, party_of};
use crate::scm::document::{Document, DocumentNumber, amount, date, total_line};
use crate::scm::sales::permissions::names;
use crate::scm::sales::reports::{SalesQueries, StatementView};
use nebula::{
    Column, DataCx, KeyValue, Report, ReportData, ReportDataSource, ReportDefinition, ReportOutput,
    Result,
};
use std::sync::Arc;

const KEY: &str = "scm_customer_statement_doc";

/// The period a statement covers when the caller does not say: the quarter up
/// to today. A statement without dates is not an error worth refusing — but it
/// must never silently mean "all time", which no one asks for.
const DEFAULT_DAYS: i64 = 90;

/// Loads the statement and the customer it is addressed to.
pub struct CustomerStatementDataSource;

#[async_trait::async_trait]
impl ReportDataSource for CustomerStatementDataSource {
    fn key(&self) -> &'static str {
        KEY
    }

    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let id = cx.params.id()?;
        let to = cx
            .params
            .date("to")?
            .unwrap_or_else(|| chrono::Utc::now().date_naive());
        let from = cx
            .params
            .date("from")?
            .unwrap_or_else(|| to - chrono::Duration::days(DEFAULT_DAYS));
        let record = SalesQueries::new(db.clone())
            .customer_statement(id, from, to)
            .await?;
        let party = party_of(cx, record.customer_id, &record.customer_name).await?;
        serde_json::to_value(Addressed { record, party })
            .map_err(|e| nebula::Error::internal(e.to_string()))
    }
}

pub struct CustomerStatementDocument;

impl ReportDefinition for CustomerStatementDocument {
    fn name(&self) -> &'static str {
        "customer-statement"
    }
    fn title(&self) -> &'static str {
        "Statement of Account"
    }
    fn group(&self) -> &'static str {
        "Sales"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(CustomerStatementDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let Addressed { record: s, party }: Addressed<StatementView> = data.get(KEY)?;
        let party: Party = party;

        // A row each, not "{from} to {to}" on one: the meta block's value
        // column is narrow, and one long period wraps mid-date into `31-` and
        // `Dec-2026`.
        let meta = vec![
            KeyValue::new("From", date(s.from)),
            KeyValue::new("To", date(s.to)),
            KeyValue::new("Currency", s.currency.clone()),
            KeyValue::new("Opening balance", amount(s.opening_balance)),
        ];

        let columns = vec![
            Column::new("Date"),
            Column::new("Type"),
            Column::wide("Reference"),
            Column::number("Amount"),
            Column::number("Balance"),
        ];

        // Opening balance as the first line, so the running balance column
        // starts from something rather than appearing to begin mid-air.
        let mut rows = vec![vec![
            date(s.from),
            String::new(),
            "Balance brought forward".to_string(),
            String::new(),
            amount(s.opening_balance),
        ]];
        rows.extend(s.lines.iter().map(|l| {
            vec![
                date(l.date),
                l.kind.replace('_', " "),
                l.reference.clone().unwrap_or_default(),
                amount(l.amount),
                amount(l.balance),
            ]
        }));

        Ok(Document {
            title: "Statement of Account".to_string(),
            number: DocumentNumber::Unnumbered,
            // The line a reader acts on, so it goes where the status goes —
            // beside the title, not buried under the last row.
            status: format!("Balance due {} {}", s.currency, amount(s.closing_balance)),
            party_label: "Account",
            party: party_block(&party, &party.billing),
            second_label: None,
            second: Vec::new(),
            meta,
            columns,
            rows,
            totals: vec![total_line(
                &format!("Closing balance ({})", s.currency),
                s.closing_balance,
            )],
            terms: None,
            memo: None,
            // Nobody signs a statement: it is a report of what happened, not an
            // undertaking.
            signatures: Vec::new(),
        }
        .into_report())
    }
}
