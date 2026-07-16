//! The remittance advice: what was paid, and which invoices it settles.
//!
//! The supplier's side of a payment. Money arriving in a bank account says
//! nothing about what it is for — this is the page that tells them, so they can
//! close the right invoices instead of guessing.

use super::status_line;
use crate::scm::document::{Document, amount, date, total_line};
use crate::scm::procurement::payment::{PaymentService, PaymentView};
use crate::scm::procurement::permissions::names;
use nebula::{
    Column, DataCx, KeyValue, Report, ReportData, ReportDataSource, ReportDefinition, ReportOutput,
    Result, Signature,
};
use rust_decimal::Decimal;
use std::sync::Arc;

const KEY: &str = "scm_supplier_payment_doc";

/// Loads the one payment the caller asked for.
pub struct SupplierPaymentDataSource;

#[async_trait::async_trait]
impl ReportDataSource for SupplierPaymentDataSource {
    fn key(&self) -> &'static str {
        KEY
    }

    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = PaymentService::new(db.clone())
            .view(cx.params.id()?)
            .await?;
        serde_json::to_value(view).map_err(|e| nebula::Error::internal(e.to_string()))
    }
}

pub struct SupplierPaymentDocument;

impl ReportDefinition for SupplierPaymentDocument {
    fn name(&self) -> &'static str {
        "supplier-payment"
    }
    fn title(&self) -> &'static str {
        "Remittance Advice"
    }
    fn group(&self) -> &'static str {
        "Procurement"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::PAYMENTS_VIEW)
    }
    /// Drawn for one record: without `?id=` there is nothing to draw.
    fn requires_record(&self) -> bool {
        true
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(SupplierPaymentDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let p: PaymentView = data.get(KEY)?;

        let mut meta = vec![
            KeyValue::new("Payment date", date(p.payment_date)),
            KeyValue::new("Method", p.method.replace('_', " ")),
            KeyValue::new("Currency", p.currency.clone()),
        ];
        if let Some(r) = p.reference.as_deref().filter(|s| !s.trim().is_empty()) {
            // The bank reference is how they match it to their statement.
            meta.push(KeyValue::new("Reference", r));
        }
        if p.exchange_rate != Decimal::ONE {
            meta.push(KeyValue::new("Exchange rate", amount(p.exchange_rate)));
        }

        let columns = vec![
            Column::new("Invoice"),
            Column::wide("Their reference"),
            Column::number("Invoice total"),
            Column::number("Paid"),
        ];

        let rows = p
            .allocations
            .iter()
            .map(|a| {
                vec![
                    a.invoice_number.clone().unwrap_or_default(),
                    a.supplier_invoice_no.clone(),
                    amount(a.invoice_total),
                    amount(a.amount),
                ]
            })
            .collect();

        // What the payment covers against what left the bank. They differ when
        // money is paid on account, and a supplier who cannot see that will
        // chase the difference.
        let applied: Decimal = p.allocations.iter().map(|a| a.amount).sum();
        let mut totals = Vec::new();
        if applied != p.amount {
            totals.push(total_line("Applied to invoices", applied));
            totals.push(total_line("On account", p.amount - applied));
        }
        totals.push(total_line(
            &format!("Total paid ({})", p.currency),
            p.amount,
        ));

        Ok(Document {
            title: "Remittance Advice".to_string(),
            number: p.number.clone().into(),
            status: status_line(p.status.as_str(), p.reverse_reason.as_deref()),
            party_label: "Paid to",
            party: vec![p.supplier_name.clone()],
            second_label: None,
            second: Vec::new(),
            meta,
            columns,
            rows,
            totals,
            terms: None,
            memo: p.memo.clone(),
            signatures: vec![Signature::new("Authorised by").dated()],
            footer_notes: Vec::new(),
        }
        .into_report())
    }
}
