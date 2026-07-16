//! Procurement documents: the pages you send to a supplier or file away.
//!
//! These differ from the reports in [`super::reports`] in what they are for.
//! A report summarises many records for someone inside the company; a
//! document *is* one record, addressed outward — a purchase order commits the
//! company to a supplier. So each is parameterised by `?id=`, reads the same
//! view the detail screen reads, and lays out through
//! [`crate::scm::document`] so every SCM document is a sibling of the rest.
//!
//! One document per file. What varies between them is the words and the
//! columns, which is exactly what a reader comes here to check — so a file
//! holds one document's decisions and nothing else. What they share lives
//! below.

pub mod goods_receipt;
pub mod purchase_order;
pub mod purchase_return;
pub mod requisition;
pub mod rfq;
pub mod supplier_invoice;
pub mod supplier_payment;

pub use goods_receipt::GoodsReceiptDocument;
pub use purchase_order::PurchaseOrderDocument;
pub use purchase_return::PurchaseReturnDocument;
pub use requisition::RequisitionDocument;
pub use rfq::RfqDocument;
pub use supplier_invoice::SupplierInvoiceDocument;
pub use supplier_payment::SupplierPaymentDocument;

use crate::scm::procurement::order::OrderService;
use nebula::{DataCx, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A record plus the supplier it concerns.
///
/// Receipts and returns are keyed to an order, not a supplier, so the
/// supplier's name is not on the record — but a goods-received note with no
/// supplier on it is unfileable. The datasource resolves it through the order
/// and hands both to the layout as one payload.
#[derive(Debug, Serialize, Deserialize)]
pub struct WithSupplier<T> {
    pub record: T,
    pub supplier_name: String,
}

/// The supplier on an order, or an empty name when the order has gone. A
/// missing name must not stop the page printing: the rest of it is still the
/// evidence of what arrived.
pub async fn supplier_of(cx: &DataCx<'_>, order_id: Uuid) -> Result<String> {
    let db = cx.require_db()?;
    Ok(OrderService::new(db.clone())
        .view(order_id)
        .await
        .map(|o| o.supplier_name)
        .unwrap_or_default())
}

/// A line's traceability in one cell: the lot, then any serials on it.
pub fn trace_of(batch: Option<&str>, serials: &[String]) -> String {
    let mut trace = batch.unwrap_or_default().to_string();
    if !serials.is_empty() {
        if !trace.is_empty() {
            trace.push(' ');
        }
        trace.push_str(&serials.join(", "));
    }
    trace
}

/// The status line under a document's number. A draft or cancelled document
/// has to say so on the page: a printed draft that reads as issued is how a
/// supplier ends up shipping against nothing.
pub fn status_line(status: &str, reason: Option<&str>) -> String {
    let label = status.replace('_', " ");
    match reason.filter(|s| !s.trim().is_empty()) {
        Some(why) => format!("{label} — {why}"),
        None => label,
    }
}
