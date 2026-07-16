//! Sales documents: the pages a customer receives.
//!
//! The outbound twins of [`crate::scm::procurement::documents`]. Each is
//! parameterised by `?id=`, reads the same view its detail screen reads, and
//! lays out through [`crate::scm::document`].
//!
//! Unlike the procurement side, these carry the customer's own address: a
//! document a customer receives has to say who it is addressed to, so each
//! datasource loads the customer alongside the record and hands both to the
//! layout as one payload.

use crate::scm::document::{Document, amount, date, quantity, total_line};
use crate::scm::sales::credit_note::{CreditNoteService, CreditNoteView};
use crate::scm::sales::customer::customer;
use crate::scm::sales::delivery::{DeliveryService, DeliveryView};
use crate::scm::sales::invoice::{InvoiceService, InvoiceView};
use crate::scm::sales::order::{OrderService, OrderView};
use crate::scm::sales::permissions::names;
use crate::scm::sales::quotation::{QuotationService, QuotationView};
use nebula::sea_orm::EntityTrait;
use nebula::{
    Column, DataCx, Error, KeyValue, Report, ReportData, ReportDataSource, ReportDefinition,
    ReportOutput, Result, Signature,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

const INVOICE_KEY: &str = "scm_sales_invoice_doc";
const ORDER_KEY: &str = "scm_sales_order_doc";
const DELIVERY_KEY: &str = "scm_delivery_note_doc";
const QUOTATION_KEY: &str = "scm_quotation_doc";
const CREDIT_NOTE_KEY: &str = "scm_credit_note_doc";

// ---------------------------------------------------------------------------
// The party a sales document is addressed to
// ---------------------------------------------------------------------------

/// The customer as a document prints them. Flattened out of the customer row
/// so the layout never reaches into the entity.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Party {
    pub name: String,
    pub contact: Option<String>,
    pub tax_number: Option<String>,
    pub billing: Vec<String>,
    pub shipping: Vec<String>,
}

/// Load a customer's printable details. A missing customer is not fatal: the
/// document still names them from the record it was built for, and printing
/// a page without an address beats refusing to print at all.
async fn party_of(cx: &DataCx<'_>, customer_id: Uuid, fallback_name: &str) -> Result<Party> {
    let db = cx.require_db()?;
    let Some(c) = customer::Entity::find_by_id(customer_id).one(db).await? else {
        return Ok(Party { name: fallback_name.to_string(), ..Default::default() });
    };
    let lines = |l1: &Option<String>, l2: &Option<String>, city: &Option<String>,
                 region: &Option<String>, post: &Option<String>, country: &Option<String>| {
        // City, region and postcode belong on one line, the way an envelope
        // is written — not stacked one per field.
        let mut out: Vec<String> = Vec::new();
        for part in [l1, l2] {
            if let Some(v) = part.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                out.push(v.to_string());
            }
        }
        let locality: Vec<String> = [city, region, post]
            .into_iter()
            .filter_map(|v| v.as_deref().map(str::trim).filter(|v| !v.is_empty()))
            .map(str::to_string)
            .collect();
        if !locality.is_empty() {
            out.push(locality.join(", "));
        }
        if let Some(v) = country.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            out.push(v.to_string());
        }
        out
    };

    Ok(Party {
        name: c.legal_name.clone().unwrap_or_else(|| c.name.clone()),
        contact: c.contact_name.clone(),
        tax_number: c.tax_number.clone(),
        billing: lines(
            &c.billing_address_line1,
            &c.billing_address_line2,
            &c.billing_city,
            &c.billing_region,
            &c.billing_postal_code,
            &c.billing_country,
        ),
        shipping: lines(
            &c.shipping_address_line1,
            &c.shipping_address_line2,
            &c.shipping_city,
            &c.shipping_region,
            &c.shipping_postal_code,
            &c.shipping_country,
        ),
    })
}

/// The party block: who they are, then how to reach them.
fn party_block(p: &Party, address: &[String]) -> Vec<String> {
    let mut out = vec![p.name.clone()];
    if let Some(c) = p.contact.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push(format!("Attn: {c}"));
    }
    out.extend(address.iter().cloned());
    if let Some(t) = p.tax_number.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push(format!("VAT/Tax: {t}"));
    }
    out
}

/// A record plus the customer it is addressed to.
#[derive(Debug, Serialize, Deserialize)]
pub struct Addressed<T> {
    pub record: T,
    pub party: Party,
}

// ---------------------------------------------------------------------------
// Sales invoice
// ---------------------------------------------------------------------------

pub struct InvoiceDataSource;

#[async_trait::async_trait]
impl ReportDataSource for InvoiceDataSource {
    fn key(&self) -> &'static str {
        INVOICE_KEY
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
        let Addressed { record: i, party } = data.get::<Addressed<InvoiceView>>(INVOICE_KEY)?;

        let mut meta = vec![
            KeyValue::new("Invoice date", date(i.invoice_date)),
            KeyValue::new("Currency", i.currency.clone()),
        ];
        if let Some(due) = i.due_date {
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

// ---------------------------------------------------------------------------
// Sales order confirmation
// ---------------------------------------------------------------------------

pub struct OrderDataSource;

#[async_trait::async_trait]
impl ReportDataSource for OrderDataSource {
    fn key(&self) -> &'static str {
        ORDER_KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let record = OrderService::new(db.clone()).view(cx.params.id()?).await?;
        let party = party_of(cx, record.customer_id, &record.customer_name).await?;
        serde_json::to_value(Addressed { record, party })
            .map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct SalesOrderDocument;

impl ReportDefinition for SalesOrderDocument {
    fn name(&self) -> &'static str {
        "sales-order"
    }
    fn title(&self) -> &'static str {
        "Sales Order"
    }
    fn group(&self) -> &'static str {
        "Sales"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::ORDERS_VIEW)
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(OrderDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let Addressed { record: o, party } = data.get::<Addressed<OrderView>>(ORDER_KEY)?;

        let mut meta = vec![
            KeyValue::new("Order date", date(o.order_date)),
            KeyValue::new("Currency", o.currency.clone()),
        ];
        if let Some(e) = o.expected_date {
            meta.push(KeyValue::new("Expected", date(e)));
        }
        if o.payment_terms_days > 0 {
            meta.push(KeyValue::new("Payment terms", format!("{} days", o.payment_terms_days)));
        }
        if let Some(po) = o.customer_po_no.as_deref().filter(|s| !s.trim().is_empty()) {
            meta.push(KeyValue::new("Your PO", po));
        }
        if let Some(i) = o.incoterms.as_deref().filter(|s| !s.trim().is_empty()) {
            meta.push(KeyValue::new("Incoterms", i));
        }

        // The order's own shipping address wins over the customer's default:
        // this order may be going somewhere else.
        let ship_to = match o.shipping_address.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(a) => a.lines().map(|l| l.trim().to_string()).collect(),
            None if !party.shipping.is_empty() => party.shipping.clone(),
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

        Ok(Document {
            title: "Sales Order".to_string(),
            number: o.number.clone(),
            status: status_line(o.status.as_str(), o.cancel_reason.as_deref()),
            party_label: "Customer",
            party: party_block(&party, &party.billing),
            second_label: Some("Ship to"),
            second: ship_to,
            meta,
            columns,
            rows,
            totals,
            terms: o.terms_and_conditions.clone(),
            memo: o.memo.clone(),
            signatures: Vec::new(),
        }
        .into_report())
    }
}

// ---------------------------------------------------------------------------
// Delivery note
// ---------------------------------------------------------------------------

pub struct DeliveryDataSource;

#[async_trait::async_trait]
impl ReportDataSource for DeliveryDataSource {
    fn key(&self) -> &'static str {
        DELIVERY_KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let record = DeliveryService::new(db.clone()).view(cx.params.id()?).await?;
        let party = party_of(cx, record.customer_id, &record.customer_name).await?;
        serde_json::to_value(Addressed { record, party })
            .map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct DeliveryNoteDocument;

impl ReportDefinition for DeliveryNoteDocument {
    fn name(&self) -> &'static str {
        "delivery-note"
    }
    fn title(&self) -> &'static str {
        "Delivery Note"
    }
    fn group(&self) -> &'static str {
        "Sales"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::DELIVERIES_VIEW)
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(DeliveryDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let Addressed { record: d, party } = data.get::<Addressed<DeliveryView>>(DELIVERY_KEY)?;

        let mut meta = vec![KeyValue::new("Delivery date", date(d.delivery_date))];
        if let Some(o) = d.order_number.as_deref().filter(|s| !s.trim().is_empty()) {
            meta.push(KeyValue::new("Order", o));
        }
        for (label, value) in [
            ("Carrier", d.carrier.as_deref()),
            ("Tracking", d.tracking_no.as_deref()),
            ("Vehicle", d.vehicle_reg.as_deref()),
            ("Driver", d.driver_name.as_deref()),
        ] {
            if let Some(v) = value.filter(|s| !s.trim().is_empty()) {
                meta.push(KeyValue::new(label, v));
            }
        }

        let ship_to = match d.shipping_address.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(a) => a.lines().map(|l| l.trim().to_string()).collect(),
            None => party.shipping.clone(),
        };

        // A delivery note carries no prices — it proves what arrived, and the
        // person signing for it has no business seeing the margin.
        let batched = d.lines.iter().any(|l| l.batch_no.is_some() || !l.serial_nos.is_empty());
        let mut columns = vec![
            Column::new("#"),
            Column::new("SKU"),
            Column::wide("Description"),
            Column::number("Qty"),
        ];
        if batched {
            columns.push(Column::new("Batch / serials"));
        }

        let rows = d
            .lines
            .iter()
            .map(|l| {
                let mut cells = vec![
                    l.line_no.to_string(),
                    l.sku.clone(),
                    l.item_name.clone(),
                    quantity(l.qty),
                ];
                if batched {
                    let mut trace = l.batch_no.clone().unwrap_or_default();
                    if !l.serial_nos.is_empty() {
                        if !trace.is_empty() {
                            trace.push(' ');
                        }
                        trace.push_str(&l.serial_nos.join(", "));
                    }
                    cells.push(trace);
                }
                cells
            })
            .collect();

        Ok(Document {
            title: "Delivery Note".to_string(),
            number: d.number.clone(),
            status: status_line(d.status.as_str(), None),
            party_label: "Customer",
            party: party_block(&party, &party.billing),
            second_label: Some("Deliver to"),
            second: ship_to,
            meta,
            columns,
            rows,
            totals: Vec::new(),
            terms: None,
            memo: d.memo.clone(),
            signatures: vec![
                Signature::new("Delivered by").dated(),
                match d.received_by_name.as_deref().filter(|s| !s.trim().is_empty()) {
                    Some(name) => Signature::new("Received by").name(name).dated(),
                    None => Signature::new("Received by").dated(),
                },
            ],
        }
        .into_report())
    }
}

// ---------------------------------------------------------------------------
// Quotation
// ---------------------------------------------------------------------------

pub struct QuotationDataSource;

#[async_trait::async_trait]
impl ReportDataSource for QuotationDataSource {
    fn key(&self) -> &'static str {
        QUOTATION_KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let record = QuotationService::new(db.clone()).view(cx.params.id()?).await?;
        let party = party_of(cx, record.customer_id, &record.customer_name).await?;
        serde_json::to_value(Addressed { record, party })
            .map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct QuotationDocument;

impl ReportDefinition for QuotationDocument {
    fn name(&self) -> &'static str {
        "quotation"
    }
    fn title(&self) -> &'static str {
        "Quotation"
    }
    fn group(&self) -> &'static str {
        "Sales"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::QUOTATIONS_VIEW)
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(QuotationDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let Addressed { record: q, party } = data.get::<Addressed<QuotationView>>(QUOTATION_KEY)?;

        let mut meta = vec![
            KeyValue::new("Date", date(q.quote_date)),
            KeyValue::new("Currency", q.currency.clone()),
        ];
        if let Some(v) = q.valid_until {
            // The one date that decides whether this page still means
            // anything.
            meta.push(KeyValue::new("Valid until", date(v)));
        }

        let discounted = q.lines.iter().any(|l| l.discount_pct.is_some());
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

        let rows = q
            .lines
            .iter()
            .map(|l| {
                let mut cells = vec![
                    l.line_no.to_string(),
                    l.sku.clone(),
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

        let mut totals = vec![total_line("Subtotal", q.subtotal)];
        if let Some(pct) = q.discount_pct.filter(|d| !d.is_zero()) {
            totals.push(KeyValue::new("Discount", format!("{}%", amount(pct))));
        }
        if let Some(a) = q.discount_amount.filter(|d| !d.is_zero()) {
            totals.push(total_line("Discount", a));
        }
        if let Some(c) = q.other_charges.filter(|d| !d.is_zero()) {
            totals.push(total_line("Other charges", c));
        }
        totals.push(total_line(&format!("Total ({})", q.currency), q.total));

        Ok(Document {
            title: "Quotation".to_string(),
            number: q.number.clone(),
            status: status_line(q.status.as_str(), None),
            party_label: "Quoted to",
            party: party_block(&party, &party.billing),
            second_label: None,
            second: Vec::new(),
            meta,
            columns,
            rows,
            totals,
            terms: q.terms_and_conditions.clone(),
            memo: q.memo.clone(),
            signatures: Vec::new(),
        }
        .into_report())
    }
}

// ---------------------------------------------------------------------------
// Credit note
// ---------------------------------------------------------------------------

pub struct CreditNoteDataSource;

#[async_trait::async_trait]
impl ReportDataSource for CreditNoteDataSource {
    fn key(&self) -> &'static str {
        CREDIT_NOTE_KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let record = CreditNoteService::new(db.clone()).view(cx.params.id()?).await?;
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
        let Addressed { record: c, party } =
            data.get::<Addressed<CreditNoteView>>(CREDIT_NOTE_KEY)?;

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
            number: c.number.clone(),
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

/// The status line under a document's number. A draft or cancelled document
/// has to say so on the page: a printed draft that reads as issued is how a
/// customer ends up paying against something that does not exist.
fn status_line(status: &str, cancel_reason: Option<&str>) -> String {
    let label = status.replace('_', " ");
    match cancel_reason.filter(|s| !s.trim().is_empty()) {
        Some(why) => format!("{label} — {why}"),
        None => label,
    }
}
