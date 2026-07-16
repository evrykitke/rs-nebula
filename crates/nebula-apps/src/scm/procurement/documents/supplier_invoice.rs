//! The supplier's invoice as booked — the company's own record of a bill it
//! received, not one it issues.

use super::status_line;
use crate::scm::document::{Document, amount, date, quantity, total_line};
use crate::scm::procurement::invoice::{InvoiceService, InvoiceView};
use crate::scm::procurement::permissions::names;
use nebula::{
    Column, DataCx, Error, KeyValue, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportOutput, Result,
};
use std::sync::Arc;

const KEY: &str = "scm_supplier_invoice_doc";

pub struct SupplierInvoiceDataSource;

#[async_trait::async_trait]
impl ReportDataSource for SupplierInvoiceDataSource {
    fn key(&self) -> &'static str {
        KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = InvoiceService::new(db.clone())
            .view(cx.params.id()?)
            .await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct SupplierInvoiceDocument;

impl ReportDefinition for SupplierInvoiceDocument {
    fn name(&self) -> &'static str {
        "supplier-invoice"
    }
    fn title(&self) -> &'static str {
        "Supplier Invoice"
    }
    fn group(&self) -> &'static str {
        "Procurement"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::INVOICES_VIEW)
    }
    /// Drawn for one record: without `?id=` there is nothing to draw.
    fn requires_record(&self) -> bool {
        true
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(SupplierInvoiceDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let i: InvoiceView = data.get(KEY)?;

        let mut meta = vec![
            KeyValue::new("Invoice date", date(i.invoice_date)),
            KeyValue::new("Currency", i.currency.clone()),
        ];
        if !i.supplier_invoice_no.trim().is_empty() {
            // Their number, not ours: it is what the supplier quotes when
            // they chase payment.
            meta.push(KeyValue::new(
                "Their invoice no.",
                i.supplier_invoice_no.clone(),
            ));
        }
        if let Some(d) = i.due_date {
            meta.push(KeyValue::new("Due date", date(d)));
        }
        if let Some(t) = i.payment_terms_days.filter(|d| *d > 0) {
            meta.push(KeyValue::new("Payment terms", format!("{t} days")));
        }
        if let Some(o) = i.order_number.as_deref().filter(|s| !s.trim().is_empty()) {
            meta.push(KeyValue::new("Against order", o));
        }

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
        totals.push(total_line(&format!("Total ({})", i.currency), i.total));

        Ok(Document {
            title: "Supplier Invoice".to_string(),
            number: i.number.clone().into(),
            status: status_line(i.status.as_str(), i.cancel_reason.as_deref()),
            party_label: "Supplier",
            party: vec![i.supplier_name.clone()],
            second_label: None,
            second: Vec::new(),
            meta,
            columns,
            rows,
            totals,
            terms: None,
            memo: i.memo.clone(),
            signatures: Vec::new(),
            footer_notes: Vec::new(),
        }
        .into_report())
    }
}
