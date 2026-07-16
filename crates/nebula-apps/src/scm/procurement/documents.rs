//! Procurement documents: the pages you send to a supplier or file away.
//!
//! These differ from the reports in [`super::reports`] in what they are for.
//! A report summarises many records for someone inside the company; a
//! document *is* one record, addressed outward — a purchase order commits
//! the company to a supplier. So each is parameterised by `?id=`, reads the
//! same view the detail screen reads, and lays out through
//! [`crate::scm::document`] so every SCM document is a sibling of the rest.

use crate::scm::document::{Document, amount, date, quantity, total_line};
use crate::scm::procurement::order::{OrderService, OrderView};
use crate::scm::procurement::permissions::names;
use nebula::{
    Column, DataCx, KeyValue, Report, ReportData, ReportDataSource, ReportDefinition, ReportOutput,
    Result, Signature,
};
use std::sync::Arc;

const PURCHASE_ORDER_KEY: &str = "scm_purchase_order_doc";

/// Loads the one order the caller asked for.
pub struct PurchaseOrderDataSource;

#[async_trait::async_trait]
impl ReportDataSource for PurchaseOrderDataSource {
    fn key(&self) -> &'static str {
        PURCHASE_ORDER_KEY
    }

    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = OrderService::new(db.clone()).view(cx.params.id()?).await?;
        serde_json::to_value(view).map_err(|e| nebula::Error::internal(e.to_string()))
    }
}

/// The purchase order as sent to a supplier.
pub struct PurchaseOrderDocument;

impl ReportDefinition for PurchaseOrderDocument {
    fn name(&self) -> &'static str {
        "purchase-order"
    }
    fn title(&self) -> &'static str {
        "Purchase Order"
    }
    fn group(&self) -> &'static str {
        "Procurement"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        // A document is a page, not a dataset: there is nothing to hand to a
        // spreadsheet, and nothing to sort on screen.
        &[ReportOutput::Pdf]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::ORDERS_VIEW)
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(PurchaseOrderDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let o: OrderView = data.get(PURCHASE_ORDER_KEY)?;

        let mut meta = vec![
            KeyValue::new("Order date", date(o.order_date)),
            KeyValue::new("Currency", o.currency.clone()),
        ];
        if let Some(expected) = o.expected_date {
            meta.push(KeyValue::new("Expected", date(expected)));
        }
        if o.payment_terms_days > 0 {
            meta.push(KeyValue::new(
                "Payment terms",
                format!("{} days", o.payment_terms_days),
            ));
        }
        if let Some(r) = o.reference.as_deref().filter(|s| !s.trim().is_empty()) {
            meta.push(KeyValue::new("Reference", r));
        }
        if let Some(i) = o.incoterms.as_deref().filter(|s| !s.trim().is_empty()) {
            meta.push(KeyValue::new("Incoterms", i));
        }
        if let Some(s) = o.shipping_method.as_deref().filter(|s| !s.trim().is_empty()) {
            meta.push(KeyValue::new("Ship via", s));
        }

        let mut party = vec![o.supplier_name.clone()];
        if let Some(c) = o.supplier_contact.as_deref().filter(|s| !s.trim().is_empty()) {
            party.push(format!("Attn: {c}"));
        }

        // Where the goods go: the explicit delivery address, else the
        // receiving warehouse.
        let deliver_to = match o.delivery_address.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(addr) => addr.lines().map(|l| l.trim().to_string()).collect(),
            None => vec![format!("Warehouse {}", o.warehouse_code)],
        };

        let discounted = o.lines.iter().any(|l| l.discount_pct.is_some());
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

        let rows = o
            .lines
            .iter()
            .map(|l| {
                let mut cells = vec![
                    l.line_no.to_string(),
                    l.sku.clone(),
                    // The line's own description overrides the item's name:
                    // it is what was actually agreed with the supplier.
                    l.description.clone().unwrap_or_else(|| l.item_name.clone()),
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

        // Only show the adjustments that exist: a document listing a zero
        // discount invites the question of why it is there.
        let mut totals = vec![total_line("Subtotal", o.subtotal)];
        if let Some(pct) = o.discount_pct.filter(|d| !d.is_zero()) {
            totals.push(KeyValue::new("Discount", format!("{}%", amount(pct))));
        }
        if let Some(a) = o.discount_amount.filter(|d| !d.is_zero()) {
            totals.push(total_line("Discount", a));
        }
        if let Some(c) = o.other_charges.filter(|d| !d.is_zero()) {
            totals.push(total_line("Other charges", c));
        }
        totals.push(total_line(&format!("Total ({})", o.currency), o.total));

        let status = match o.status.as_str() {
            "cancelled" => match o.cancel_reason.as_deref().filter(|s| !s.trim().is_empty()) {
                Some(why) => format!("Cancelled — {why}"),
                None => "Cancelled".to_string(),
            },
            other => other.replace('_', " "),
        };

        Ok(Document {
            title: "Purchase Order".to_string(),
            number: o.number.clone(),
            status,
            party_label: "Supplier",
            party,
            second_label: Some("Deliver to"),
            second: deliver_to,
            meta,
            columns,
            rows,
            totals,
            terms: o.terms_and_conditions.clone(),
            memo: o.memo.clone(),
            signatures: vec![
                Signature::new("Prepared by").dated(),
                Signature::new("Approved by").dated(),
            ],
        }
        .into_report())
    }
}
