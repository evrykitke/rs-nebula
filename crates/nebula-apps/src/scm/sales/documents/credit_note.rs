//! The credit note: what is being taken back off an invoice, and why.

use super::{Addressed, party_block, party_of, status_line};
use crate::scm::document::{Document, amount, date, quantity, total_line};
use crate::scm::sales::credit_note::{CreditNoteService, CreditNoteView};
use crate::scm::sales::permissions::names;
use nebula::{
    Column, DataCx, Error, KeyValue, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportOutput, Result,
};
use std::sync::Arc;

const KEY: &str = "scm_credit_note_doc";

pub struct CreditNoteDataSource;

#[async_trait::async_trait]
impl ReportDataSource for CreditNoteDataSource {
    fn key(&self) -> &'static str {
        KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let record = CreditNoteService::new(db.clone())
            .view(cx.params.id()?)
            .await?;
        let party = party_of(cx, record.customer_id, &record.customer_name).await?;
        serde_json::to_value(Addressed { record, party })
            .map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct CreditNoteDocument;

impl ReportDefinition for CreditNoteDocument {
    fn name(&self) -> &'static str {
        "credit-note"
    }
    fn title(&self) -> &'static str {
        "Credit Note"
    }
    fn group(&self) -> &'static str {
        "Sales"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::CREDIT_NOTES_VIEW)
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(CreditNoteDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let Addressed { record: c, party } = data.get::<Addressed<CreditNoteView>>(KEY)?;

        let mut meta = vec![
            KeyValue::new("Date", date(c.credit_date)),
            KeyValue::new("Currency", c.currency.clone()),
        ];
        if let Some(inv) = c.invoice_number.as_deref().filter(|s| !s.trim().is_empty()) {
            // A credit note without its invoice is unbookable by the
            // customer: it is the whole point of the document.
            meta.push(KeyValue::new("Against invoice", inv));
        }
        if !c.reason.trim().is_empty() {
            meta.push(KeyValue::new("Reason", c.reason.clone()));
        }

        // Credit note lines credit invoice lines, so they carry a description
        // rather than a SKU of their own.
        let taxed = c.lines.iter().any(|l| !l.tax.is_zero());
        let mut columns = vec![
            Column::new("#"),
            Column::wide("Description"),
            Column::number("Qty"),
            Column::number("Unit price"),
            Column::number("Net"),
        ];
        if taxed {
            columns.push(Column::number("Tax"));
        }

        let rows = c
            .lines
            .iter()
            .map(|l| {
                let mut cells = vec![
                    l.line_no.to_string(),
                    l.description.clone(),
                    quantity(l.qty),
                    amount(l.unit_price),
                    amount(l.net),
                ];
                if taxed {
                    cells.push(amount(l.tax));
                }
                cells
            })
            .collect();

        let mut totals = vec![total_line("Subtotal", c.subtotal)];
        if !c.tax.is_zero() {
            totals.push(total_line("Tax", c.tax));
        }
        totals.push(total_line(&format!("Credited ({})", c.currency), c.total));

        Ok(Document {
            title: "Credit Note".to_string(),
            number: c.number.clone().into(),
            status: status_line(c.status.as_str(), c.cancel_reason.as_deref()),
            party_label: "Credit to",
            party: party_block(&party, &party.billing),
            second_label: None,
            second: Vec::new(),
            meta,
            columns,
            rows,
            totals,
            terms: None,
            memo: c.memo.clone(),
            signatures: Vec::new(),
        }
        .into_report())
    }
}
