//! The invoice the customer is asked to pay.

use super::{Addressed, party_block, party_of, status_line};
use crate::scm::document::{Document, amount, date, quantity, total_line};
use crate::scm::sales::invoice::{InvoiceService, InvoiceView};
use crate::scm::sales::permissions::names;
use nebula::{
    Column, DataCx, Error, KeyValue, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportOutput, Result,
};
use std::sync::Arc;

const KEY: &str = "scm_sales_invoice_doc";

pub struct InvoiceDataSource;

#[async_trait::async_trait]
impl ReportDataSource for InvoiceDataSource {
    fn key(&self) -> &'static str {
        KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let record = InvoiceService::new(db.clone()).view(cx.params.id()?).await?;
        let party = party_of(cx, record.customer_id, &record.customer_name).await?;
        serde_json::to_value(Addressed { record, party })
            .map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct SalesInvoiceDocument;

impl ReportDefinition for SalesInvoiceDocument {
    fn name(&self) -> &'static str {
        "sales-invoice"
    }
    fn title(&self) -> &'static str {
        "Sales Invoice"
    }
    fn group(&self) -> &'static str {
        "Sales"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::INVOICES_VIEW)
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(InvoiceDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let Addressed { record: i, party } = data.get::<Addressed<InvoiceView>>(KEY)?;

        let mut meta = vec![
            KeyValue::new("Invoice date", date(i.invoice_date)),
            KeyValue::new("Currency", i.currency.clone()),
        ];
        if let Some(due) = i.due_date {
            // The date the money is expected. On an invoice it is second in
            // importance only to the total.
            meta.push(KeyValue::new("Due date", date(due)));
        }
        if let Some(terms) = i.payment_terms_days.filter(|d| *d > 0) {
            meta.push(KeyValue::new("Payment terms", format!("{terms} days")));
        }
        if let Some(o) = i.order_number.as_deref().filter(|s| !s.trim().is_empty()) {
            meta.push(KeyValue::new("Our order", o));
        }
        if let Some(po) = i.customer_po_no.as_deref().filter(|s| !s.trim().is_empty()) {
            // The customer's own reference: often the only number their
            // accounts payable will match against.
            meta.push(KeyValue::new("Your PO", po));
        }

        let taxed = i.lines.iter().any(|l| !l.tax.is_zero());
        let discounted = i.lines.iter().any(|l| l.discount_pct.is_some());
        let mut columns = vec![
            Column::new("#"),
            Column::new("SKU"),
            Column::wide("Description"),
            Column::number("Qty"),
            Column::number("Unit price"),
        ];
        if discounted {
            columns.push(Column::number("Disc %"));
        }
        columns.push(Column::number("Net"));
        if taxed {
            columns.push(Column::number("Tax"));
        }

        let rows = i
            .lines
            .iter()
            .map(|l| {
                let mut cells = vec![
                    l.line_no.to_string(),
                    l.sku.clone(),
                    l.description.clone(),
                    quantity(l.qty),
                    amount(l.unit_price),
                ];
                if discounted {
                    cells.push(l.discount_pct.map(amount).unwrap_or_default());
                }
                cells.push(amount(l.net));
                if taxed {
                    cells.push(amount(l.tax));
                }
                cells
            })
            .collect();

        let mut totals = vec![total_line("Subtotal", i.subtotal)];
        if let Some(pct) = i.discount_pct.filter(|d| !d.is_zero()) {
            totals.push(KeyValue::new("Discount", format!("{}%", amount(pct))));
        }
        if let Some(a) = i.discount_amount.filter(|d| !d.is_zero()) {
            totals.push(total_line("Discount", a));
        }
        if let Some(c) = i.other_charges.filter(|d| !d.is_zero()) {
            totals.push(total_line("Other charges", c));
        }
        if !i.tax.is_zero() {
            totals.push(total_line("Tax", i.tax));
        }
        totals.push(total_line(&format!("Total ({})", i.currency), i.total));
        // What is still owed is the number the reader is looking for; it
        // differs from the total the moment anything is paid.
        if i.outstanding != i.total {
            totals.push(total_line("Outstanding", i.outstanding));
        }

        Ok(Document {
            title: "Invoice".to_string(),
            number: i.number.clone(),
            status: status_line(i.status.as_str(), i.cancel_reason.as_deref()),
            party_label: "Bill to",
            party: party_block(&party, &party.billing),
            second_label: None,
            second: Vec::new(),
            meta,
            columns,
            rows,
            totals,
            terms: None,
            memo: i.memo.clone(),
            signatures: Vec::new(),
        }
        .into_report())
    }
}
