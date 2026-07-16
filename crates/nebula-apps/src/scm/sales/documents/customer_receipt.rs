//! The receipt: proof that money arrived, and what it was put against.

use super::{Addressed, Party, party_block, party_of, status_line};
use crate::scm::document::{Document, amount, date, total_line};
use crate::scm::sales::payment::{PaymentService, PaymentView};
use crate::scm::sales::permissions::names;
use nebula::{
    Column, DataCx, KeyValue, Report, ReportData, ReportDataSource, ReportDefinition, ReportOutput,
    Result, Signature,
};
use rust_decimal::Decimal;
use std::sync::Arc;

const KEY: &str = "scm_customer_receipt_doc";

/// Loads the payment and the customer it came from.
pub struct CustomerReceiptDataSource;

#[async_trait::async_trait]
impl ReportDataSource for CustomerReceiptDataSource {
    fn key(&self) -> &'static str {
        KEY
    }

    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let record = PaymentService::new(db.clone())
            .view(cx.params.id()?)
            .await?;
        let party = party_of(cx, record.customer_id, &record.customer_name).await?;
        serde_json::to_value(Addressed { record, party })
            .map_err(|e| nebula::Error::internal(e.to_string()))
    }
}

pub struct CustomerReceiptDocument;

impl ReportDefinition for CustomerReceiptDocument {
    fn name(&self) -> &'static str {
        "customer-receipt"
    }
    fn title(&self) -> &'static str {
        "Receipt"
    }
    fn group(&self) -> &'static str {
        "Sales"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::PAYMENTS_VIEW)
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(CustomerReceiptDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let Addressed { record: p, party }: Addressed<PaymentView> = data.get(KEY)?;
        let party: Party = party;

        let mut meta = vec![
            KeyValue::new("Received", date(p.payment_date)),
            KeyValue::new("Method", p.method.replace('_', " ")),
            KeyValue::new("Currency", p.currency.clone()),
        ];
        if let Some(r) = p.reference.as_deref().filter(|s| !s.trim().is_empty()) {
            meta.push(KeyValue::new("Reference", r));
        }
        if p.exchange_rate != Decimal::ONE {
            meta.push(KeyValue::new("Exchange rate", amount(p.exchange_rate)));
        }

        let columns = vec![
            Column::wide("Invoice"),
            Column::number("Invoice total"),
            Column::number("Applied"),
        ];

        let rows = p
            .allocations
            .iter()
            .map(|a| {
                vec![
                    // An allocation with no invoice is money held on account —
                    // it must read as that, not as a blank line.
                    a.invoice_number
                        .clone()
                        .unwrap_or_else(|| "On account".to_string()),
                    amount(a.invoice_total),
                    amount(a.amount),
                ]
            })
            .collect();

        // Standing credit is the customer's money: say so on the page they
        // keep, or the next statement looks wrong to them.
        let mut totals = Vec::new();
        if !p.unallocated.is_zero() {
            totals.push(total_line("Applied to invoices", p.amount - p.unallocated));
            totals.push(total_line("Held on account", p.unallocated));
        }
        totals.push(total_line(
            &format!("Total received ({})", p.currency),
            p.amount,
        ));

        Ok(Document {
            title: "Receipt".to_string(),
            number: p.number.clone().into(),
            status: status_line(p.status.as_str(), p.reverse_reason.as_deref()),
            party_label: "Received from",
            party: party_block(&party, &party.billing),
            second_label: None,
            second: Vec::new(),
            meta,
            columns,
            rows,
            totals,
            terms: None,
            memo: p.memo.clone(),
            signatures: vec![Signature::new("Received by").dated()],
        }
        .into_report())
    }
}
