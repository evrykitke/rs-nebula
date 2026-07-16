//! Sales documents: the pages a customer receives.
//!
//! The outbound twins of [`crate::scm::procurement::documents`]. Each is
//! parameterised by `?id=`, reads the same view its detail screen reads, and
//! lays out through [`crate::scm::document`].
//!
//! One document per file, as on the procurement side. What they share — the
//! customer's own address — lives here: unlike a purchase order, a document a
//! customer receives has to say who it is addressed to, so each datasource
//! loads the customer alongside the record and hands both to the layout as
//! one payload.

pub mod credit_note;
pub mod delivery_note;
pub mod quotation;
pub mod sales_invoice;
pub mod sales_order;

pub use credit_note::CreditNoteDocument;
pub use delivery_note::DeliveryNoteDocument;
pub use quotation::QuotationDocument;
pub use sales_invoice::SalesInvoiceDocument;
pub use sales_order::SalesOrderDocument;

use crate::scm::sales::customer::customer;
use nebula::sea_orm::EntityTrait;
use nebula::{DataCx, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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

/// A record plus the customer it is addressed to.
#[derive(Debug, Serialize, Deserialize)]
pub struct Addressed<T> {
    pub record: T,
    pub party: Party,
}

/// Load a customer's printable details. A missing customer is not fatal: the
/// document still names them from the record it was built for, and printing a
/// page without an address beats refusing to print at all.
pub async fn party_of(cx: &DataCx<'_>, customer_id: Uuid, fallback_name: &str) -> Result<Party> {
    let db = cx.require_db()?;
    let Some(c) = customer::Entity::find_by_id(customer_id).one(db).await? else {
        return Ok(Party { name: fallback_name.to_string(), ..Default::default() });
    };

    Ok(Party {
        // The legal name is what a contract and an invoice need; the trading
        // name is for the screen.
        name: c.legal_name.clone().unwrap_or_else(|| c.name.clone()),
        contact: c.contact_name.clone(),
        tax_number: c.tax_number.clone(),
        billing: address_lines(
            &c.billing_address_line1,
            &c.billing_address_line2,
            &c.billing_city,
            &c.billing_region,
            &c.billing_postal_code,
            &c.billing_country,
        ),
        shipping: address_lines(
            &c.shipping_address_line1,
            &c.shipping_address_line2,
            &c.shipping_city,
            &c.shipping_region,
            &c.shipping_postal_code,
            &c.shipping_country,
        ),
    })
}

/// An address as it is written on an envelope: street lines, then the
/// locality on one line, then the country. Not one field per line — that is
/// the schema's shape, not an address's.
fn address_lines(
    line1: &Option<String>,
    line2: &Option<String>,
    city: &Option<String>,
    region: &Option<String>,
    postal: &Option<String>,
    country: &Option<String>,
) -> Vec<String> {
    fn text(v: &Option<String>) -> Option<&str> {
        v.as_deref().map(str::trim).filter(|v| !v.is_empty())
    }
    let mut out: Vec<String> = Vec::new();
    for part in [line1, line2] {
        if let Some(v) = text(part) {
            out.push(v.to_string());
        }
    }
    let locality: Vec<String> = [city, region, postal]
        .into_iter()
        .filter_map(text)
        .map(str::to_string)
        .collect();
    if !locality.is_empty() {
        out.push(locality.join(", "));
    }
    if let Some(v) = text(country) {
        out.push(v.to_string());
    }
    out
}

/// The party block: who they are, then how to reach them.
pub fn party_block(p: &Party, address: &[String]) -> Vec<String> {
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

/// The status line under a document's number. A draft or cancelled document
/// has to say so on the page: a printed draft that reads as issued is how a
/// customer ends up paying against something that does not exist.
pub fn status_line(status: &str, cancel_reason: Option<&str>) -> String {
    let label = status.replace('_', " ");
    match cancel_reason.filter(|s| !s.trim().is_empty()) {
        Some(why) => format!("{label} — {why}"),
        None => label,
    }
}
