//! Sales invoices: the accounts-receivable spine.
//!
//! Posting validates every line against the sales order under the order
//! row lock — the billing consistency check, the outbound mirror of
//! procurement's three-way match:
//!
//! 1. **Delivered** — `billed_qty + qty ≤ delivered_qty`: only what has
//!    actually shipped can be billed (strict-delivered keeps the
//!    delivered-not-billed report meaningful; a bill-up-to-ordered policy
//!    is a later tenant setting).
//! 2. **Priced** — the line's effective price equals the order line's
//!    exactly (a tolerance percentage becomes a tenant setting later).
//!
//! Totals resolve net/tax/gross from the accounting tax codes (read over
//! the same contained SQL seam the GL reconciliation uses), honouring the
//! header `tax_inclusive` flag and the customer's tax exemption. Posting
//! books **Dr AR / Cr Sales / Cr VAT output** (rounding residue to the
//! rounding role) through the GL port in the posting transaction, and
//! bumps `billed_qty`. Payment state is derived from posted allocations,
//! never stored. Cancelling a posted invoice (nothing allocated against
//! it) restores `billed_qty` and books the mirror entry.

use crate::scm::gl;
use crate::scm::inventory::item::item;
use crate::scm::inventory::stock::round_money;
use crate::scm::sales::customer::customer;
use crate::scm::sales::order::{
    self, OrderStatus, effective_price, load_lines as load_order_lines, load_order,
    load_order_locked, order_line,
};
use crate::scm::sales::permissions::names;
use axum::extract::{Path, Query};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use nebula::audit::Audit;
use nebula::auth::Authz;
use nebula::error::{Error, Result};
use nebula::{CurrentTenant, Events, Numbering, TenantDb, sea_orm};
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, QueryOrder, QuerySelect,
    Set, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Where a sales invoice is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = SalesInvoiceStatus)]
pub enum InvoiceStatus {
    Draft,
    Posted,
    Cancelled,
}

impl InvoiceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            InvoiceStatus::Draft => "draft",
            InvoiceStatus::Posted => "posted",
            InvoiceStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "draft" => Ok(InvoiceStatus::Draft),
            "posted" => Ok(InvoiceStatus::Posted),
            "cancelled" => Ok(InvoiceStatus::Cancelled),
            other => Err(Error::internal(format!("unknown invoice status {other:?}"))),
        }
    }
}

/// How much of a posted invoice has been settled: `unpaid` (nothing
/// settled or not posted), `partially_paid`, or `paid` in full.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = SalesSettlementStatus)]
pub enum SettlementStatus {
    Unpaid,
    PartiallyPaid,
    Paid,
}

/// The sales invoice header.
pub mod invoice {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "sales_invoices")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub number: Option<String>,
        pub customer_id: Uuid,
        pub order_id: Option<Uuid>,
        pub invoice_date: Date,
        pub due_date: Option<Date>,
        pub payment_terms_days: Option<i32>,
        pub currency: String,
        #[sea_orm(column_type = "Decimal(Some((20, 8)))")]
        pub exchange_rate: Decimal,
        pub tax_inclusive: bool,
        #[sea_orm(column_type = "Decimal(Some((7, 4)))", nullable)]
        pub discount_pct: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))", nullable)]
        pub discount_amount: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))", nullable)]
        pub other_charges: Option<Decimal>,
        pub customer_po_no: Option<String>,
        pub salesperson_id: Option<Uuid>,
        pub attachment_file_id: Option<Uuid>,
        pub memo: Option<String>,
        pub status: String,
        pub posted_at: Option<DateTimeUtc>,
        pub posted_by: Option<Uuid>,
        pub cancelled_at: Option<DateTimeUtc>,
        pub cancelled_by: Option<Uuid>,
        pub cancel_reason: Option<String>,
        pub created_at: DateTimeUtc,
        pub created_by: Option<Uuid>,
        pub updated_at: DateTimeUtc,
        pub updated_by: Option<Uuid>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// One sales invoice line, usually against an order line.
pub mod invoice_line {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "sales_invoice_lines")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub invoice_id: Uuid,
        pub order_line_id: Option<Uuid>,
        pub line_no: i32,
        pub description: String,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub qty: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub unit_price: Decimal,
        #[sea_orm(column_type = "Decimal(Some((7, 4)))", nullable)]
        pub discount_pct: Option<Decimal>,
        pub tax_code_id: Option<Uuid>,
        pub memo: Option<String>,
        pub created_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

// ---------------------------------------------------------------------------
// Tax: the contained accounting seam
// ---------------------------------------------------------------------------

/// The percentage rates of a set of accounting tax codes, read straight
/// from the accounting schema — the same deliberate, contained SQL seam
/// the GL reconciliation uses (reconciling or taxing across two bounded
/// contexts is inherently a cross-context read; going through SQL keeps
/// the apps unlinked). Codes absent here — or a database with no
/// accounting schema at all — resolve to a zero rate, so an untaxed
/// tenant simply sees no VAT.
pub(crate) async fn tax_rates<C: ConnectionTrait>(
    conn: &C,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Decimal>> {
    let mut ids: Vec<Uuid> = ids.to_vec();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let present = conn
        .query_one(Statement::from_string(
            DbBackend::Postgres,
            "SELECT to_regclass('accounting_tax_codes') IS NOT NULL AS present",
        ))
        .await?
        .map(|r| r.try_get::<bool>("", "present").unwrap_or(false))
        .unwrap_or(false);
    if !present {
        return Ok(HashMap::new());
    }
    // UUIDs are injection-safe; an IN list sidesteps array-binding quirks.
    let list = ids
        .iter()
        .map(|id| format!("'{id}'"))
        .collect::<Vec<_>>()
        .join(",");
    let rows = conn
        .query_all(Statement::from_string(
            DbBackend::Postgres,
            format!("SELECT id, rate FROM accounting_tax_codes WHERE id IN ({list})"),
        ))
        .await?;
    let mut map = HashMap::new();
    for r in rows {
        let id: Uuid = r
            .try_get("", "id")
            .map_err(|e| Error::internal(format!("tax code id: {e}")))?;
        let rate: Decimal = r.try_get("", "rate").unwrap_or(Decimal::ZERO);
        map.insert(id, rate);
    }
    Ok(map)
}

/// One document's money in its own currency: the line-net subtotal (before
/// header effects), the tax, and the gross total after header discounts,
/// other charges and the tax.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Totals {
    pub tax: Decimal,
    pub total: Decimal,
}

/// A priced, tax-coded line for the totals math.
pub(crate) struct TaxLine {
    pub qty: Decimal,
    pub unit_price: Decimal,
    pub discount_pct: Option<Decimal>,
    pub tax_code_id: Option<Uuid>,
}

/// Compute net/tax/gross honouring `tax_inclusive`, the header discounts
/// and charges, and the customer's tax exemption. Tax is per line off the
/// line net; the header discount reduces net without re-taxing (a rare
/// case not worth the complication in this cut).
pub(crate) fn compute_totals(
    lines: &[TaxLine],
    rates: &HashMap<Uuid, Decimal>,
    tax_inclusive: bool,
    tax_exempt: bool,
    discount_pct: Option<Decimal>,
    discount_amount: Option<Decimal>,
    other_charges: Option<Decimal>,
) -> Totals {
    let mut subtotal = Decimal::ZERO;
    let mut tax = Decimal::ZERO;
    for l in lines {
        let rate = if tax_exempt {
            Decimal::ZERO
        } else {
            l.tax_code_id
                .and_then(|id| rates.get(&id).copied())
                .unwrap_or(Decimal::ZERO)
        };
        let line_amt = round_money(l.qty * effective_price(l.unit_price, l.discount_pct));
        let (line_net, line_tax) = if tax_inclusive {
            let net = round_money(line_amt / (Decimal::ONE + rate / Decimal::ONE_HUNDRED));
            (net, line_amt - net)
        } else {
            (
                line_amt,
                round_money(line_amt * rate / Decimal::ONE_HUNDRED),
            )
        };
        subtotal += line_net;
        tax += line_tax;
    }
    let mut net_after_header = subtotal;
    if let Some(pct) = discount_pct {
        net_after_header -= round_money(subtotal * pct / Decimal::ONE_HUNDRED);
    }
    if let Some(a) = discount_amount {
        net_after_header -= a;
    }
    if let Some(c) = other_charges {
        net_after_header += c;
    }
    Totals {
        tax,
        total: round_money(net_after_header + tax),
    }
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// An invoice line as supplied by a caller.
pub struct InvoiceLineInput {
    pub order_line_id: Option<Uuid>,
    pub description: Option<String>,
    pub qty: Decimal,
    pub unit_price: Decimal,
    pub discount_pct: Option<Decimal>,
    pub tax_code_id: Option<Uuid>,
    pub memo: Option<String>,
}

/// A new draft sales invoice. Direct invoices (no order) are a later
/// phase — `order_id` is required here.
pub struct NewInvoice {
    pub order_id: Uuid,
    pub invoice_date: chrono::NaiveDate,
    pub due_date: Option<chrono::NaiveDate>,
    pub payment_terms_days: Option<i32>,
    pub exchange_rate: Option<Decimal>,
    pub tax_inclusive: bool,
    pub discount_pct: Option<Decimal>,
    pub discount_amount: Option<Decimal>,
    pub other_charges: Option<Decimal>,
    pub customer_po_no: Option<String>,
    pub attachment_file_id: Option<Uuid>,
    pub memo: Option<String>,
    pub lines: Vec<InvoiceLineInput>,
    pub created_by: Option<Uuid>,
}

/// The sales invoice service over one (tenant) connection.
pub struct InvoiceService {
    db: DatabaseConnection,
}

impl InvoiceService {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn create_draft(&self, new: NewInvoice) -> Result<InvoiceView> {
        let order_row = validate_invoice(&self.db, &new).await?;
        let txn = self.db.begin().await?;
        let invoice_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        invoice::ActiveModel {
            id: Set(invoice_id),
            number: Set(None),
            customer_id: Set(order_row.customer_id),
            order_id: Set(Some(new.order_id)),
            invoice_date: Set(new.invoice_date),
            due_date: Set(new.due_date),
            payment_terms_days: Set(new
                .payment_terms_days
                .or(Some(order_row.payment_terms_days))),
            currency: Set(order_row.currency.clone()),
            exchange_rate: Set(new.exchange_rate.unwrap_or(order_row.exchange_rate)),
            tax_inclusive: Set(new.tax_inclusive),
            discount_pct: Set(new.discount_pct),
            discount_amount: Set(new.discount_amount),
            other_charges: Set(new.other_charges),
            customer_po_no: Set(clean(new.customer_po_no).or(order_row.customer_po_no.clone())),
            salesperson_id: Set(order_row.salesperson_id),
            attachment_file_id: Set(new.attachment_file_id),
            memo: Set(clean(new.memo)),
            status: Set(InvoiceStatus::Draft.as_str().to_string()),
            posted_at: Set(None),
            posted_by: Set(None),
            cancelled_at: Set(None),
            cancelled_by: Set(None),
            cancel_reason: Set(None),
            created_at: Set(now),
            created_by: Set(new.created_by),
            updated_at: Set(now),
            updated_by: Set(None),
        }
        .insert(&txn)
        .await?;
        insert_lines(&txn, invoice_id, &new.lines, &order_row).await?;
        txn.commit().await?;
        self.view(invoice_id).await
    }

    /// Replace a draft's header and lines wholesale. The order is fixed.
    pub async fn update_draft(&self, id: Uuid, new: NewInvoice) -> Result<InvoiceView> {
        let txn = self.db.begin().await?;
        let existing = load_invoice_locked(&txn, id).await?;
        if InvoiceStatus::parse(&existing.status)? != InvoiceStatus::Draft {
            return Err(Error::Validation(
                "only a draft invoice can be edited".into(),
            ));
        }
        if existing.order_id != Some(new.order_id) {
            return Err(Error::Validation(
                "an invoice's order cannot change; delete the draft and create a new one".into(),
            ));
        }
        let order_row = validate_invoice(&txn, &new).await?;
        invoice_line::Entity::delete_many()
            .filter(invoice_line::Column::InvoiceId.eq(id))
            .exec(&txn)
            .await?;
        insert_lines(&txn, id, &new.lines, &order_row).await?;
        let mut active: invoice::ActiveModel = existing.into();
        active.invoice_date = Set(new.invoice_date);
        active.due_date = Set(new.due_date);
        active.payment_terms_days = Set(new
            .payment_terms_days
            .or(Some(order_row.payment_terms_days)));
        active.exchange_rate = Set(new.exchange_rate.unwrap_or(order_row.exchange_rate));
        active.tax_inclusive = Set(new.tax_inclusive);
        active.discount_pct = Set(new.discount_pct);
        active.discount_amount = Set(new.discount_amount);
        active.other_charges = Set(new.other_charges);
        active.customer_po_no = Set(clean(new.customer_po_no).or(order_row.customer_po_no.clone()));
        active.attachment_file_id = Set(new.attachment_file_id);
        active.memo = Set(clean(new.memo));
        active.updated_at = Set(chrono::Utc::now());
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    pub async fn delete_draft(&self, id: Uuid) -> Result<InvoiceView> {
        let view = self.view(id).await?;
        let txn = self.db.begin().await?;
        let existing = load_invoice_locked(&txn, id).await?;
        if InvoiceStatus::parse(&existing.status)? != InvoiceStatus::Draft {
            return Err(Error::Validation(
                "only a draft invoice can be deleted".into(),
            ));
        }
        invoice::Entity::delete_by_id(id).exec(&txn).await?;
        txn.commit().await?;
        Ok(view)
    }

    /// Post a draft invoice: the billing consistency check runs under the
    /// order row lock, `billed_qty` bumps, the SINV number lands and the
    /// due date resolves — one transaction. The GL request (Dr AR / Cr
    /// Sales / Cr VAT output) is staged with it and published after commit.
    pub async fn post(
        &self,
        id: Uuid,
        numbering: &Numbering,
        by: Option<Uuid>,
        gl: &gl::Gl,
    ) -> Result<InvoiceView> {
        let txn = self.db.begin().await?;
        let invoice_row = load_invoice_locked(&txn, id).await?;
        if InvoiceStatus::parse(&invoice_row.status)? != InvoiceStatus::Draft {
            return Err(Error::Validation(
                "only a draft invoice can be posted".into(),
            ));
        }
        let order_id = invoice_row
            .order_id
            .ok_or_else(|| Error::internal("invoice without an order"))?;
        let order_row = load_order_locked(&txn, order_id).await?;

        let lines = load_invoice_lines(&txn, id).await?;
        if lines.is_empty() {
            return Err(Error::Validation(
                "an invoice needs at least one line".into(),
            ));
        }
        let order_lines: HashMap<Uuid, order_line::Model> = load_order_lines(&txn, order_id)
            .await?
            .into_iter()
            .map(|l| (l.id, l))
            .collect();

        // Billing consistency, per line, accumulated per order line so two
        // invoice lines cannot bill the same goods together.
        let mut billing: HashMap<Uuid, Decimal> = HashMap::new();
        for line in &lines {
            let ol_id = line
                .order_line_id
                .ok_or_else(|| Error::internal("invoice line without an order line"))?;
            let ol = order_lines.get(&ol_id).ok_or_else(|| {
                Error::Validation(format!(
                    "line {} does not belong to this order",
                    line.line_no
                ))
            })?;
            let billed = billing.entry(ol_id).or_default();
            *billed += line.qty;
            if ol.billed_qty + *billed > ol.delivered_qty {
                return Err(Error::Validation(format!(
                    "line {}: billing {} exceeds the {} delivered and not yet billed",
                    line.line_no,
                    line.qty,
                    ol.delivered_qty - ol.billed_qty
                )));
            }
            let invoice_price = effective_price(line.unit_price, line.discount_pct);
            let order_price = effective_price(ol.unit_price, ol.discount_pct);
            if invoice_price != order_price {
                return Err(Error::Validation(format!(
                    "line {}: price {} does not match the order's {} — amend the order or the invoice",
                    line.line_no, invoice_price, order_price
                )));
            }
        }
        for (ol_id, billed) in &billing {
            let ol = order_lines[ol_id].clone();
            let base = ol.billed_qty;
            let mut active: order_line::ActiveModel = ol.into();
            active.billed_qty = Set(base + billed);
            active.update(&txn).await?;
        }

        // Totals and the base-currency GL amounts.
        let customer = load_customer(&txn, invoice_row.customer_id).await?;
        let totals = totals_for(&txn, &invoice_row, &lines, customer.tax_exempt).await?;
        let rate = invoice_row.exchange_rate;
        let net_after_header = totals.total - totals.tax;
        let net_base = round_money(net_after_header * rate);
        let tax_base = round_money(totals.tax * rate);
        let gross_base = round_money(totals.total * rate);

        let number = numbering
            .next(&txn, crate::scm::SALES_INVOICE_SERIES)
            .await?;
        let now = chrono::Utc::now();
        let due_date = invoice_row.due_date.or_else(|| {
            invoice_row
                .payment_terms_days
                .map(|d| invoice_row.invoice_date + chrono::Duration::days(d as i64))
        });

        let request = gl::ar_invoice_request(
            format!("sales.invoice:{id}:post"),
            format!(
                "Sales invoice {} against {}",
                number.formatted,
                order_row.number.as_deref().unwrap_or("sales order")
            ),
            invoice_row.invoice_date,
            net_base,
            tax_base,
            gross_base,
            false,
            gl.tenant_id(),
        )?;
        if let Some(req) = &request {
            gl::stage(&txn, req).await?;
        }
        let mut active: invoice::ActiveModel = invoice_row.into();
        active.status = Set(InvoiceStatus::Posted.as_str().to_string());
        active.number = Set(Some(number.formatted));
        active.due_date = Set(due_date);
        active.posted_at = Set(Some(now));
        active.posted_by = Set(by);
        active.updated_at = Set(now);
        active.update(&txn).await?;
        txn.commit().await?;
        if let Some(req) = request {
            gl.publish(req).await;
        }
        self.view(id).await
    }

    /// Cancel a posted invoice while nothing is allocated against it;
    /// restores `billed_qty` under the order row lock and books the mirror
    /// of the posting entry. Credit notes against it block the cancel.
    pub async fn cancel(
        &self,
        id: Uuid,
        reason: &str,
        by: Option<Uuid>,
        gl: &gl::Gl,
    ) -> Result<InvoiceView> {
        let txn = self.db.begin().await?;
        let existing = load_invoice_locked(&txn, id).await?;
        if InvoiceStatus::parse(&existing.status)? != InvoiceStatus::Posted {
            return Err(Error::Validation(
                "only a posted invoice can be cancelled".into(),
            ));
        }
        let paid = super::payment::paid_amounts(&txn, &[id])
            .await?
            .get(&id)
            .copied()
            .unwrap_or(Decimal::ZERO);
        if paid > Decimal::ZERO {
            return Err(Error::Validation(
                "this invoice has payments allocated to it; reverse the payments first".into(),
            ));
        }
        if super::credit_note::has_credit_notes(&txn, id).await? {
            return Err(Error::Validation(
                "this invoice has credit notes against it; cancel them first".into(),
            ));
        }
        let order_id = existing
            .order_id
            .ok_or_else(|| Error::internal("invoice without an order"))?;
        load_order_locked(&txn, order_id).await?;
        let lines = load_invoice_lines(&txn, id).await?;
        let order_lines: HashMap<Uuid, order_line::Model> = load_order_lines(&txn, order_id)
            .await?
            .into_iter()
            .map(|l| (l.id, l))
            .collect();
        let mut billing: HashMap<Uuid, Decimal> = HashMap::new();
        for line in &lines {
            if let Some(ol_id) = line.order_line_id {
                *billing.entry(ol_id).or_default() += line.qty;
            }
        }
        for (ol_id, billed) in &billing {
            let ol = order_lines
                .get(ol_id)
                .ok_or_else(|| Error::internal("invoice line lost its order line"))?
                .clone();
            let base = ol.billed_qty;
            let mut active: order_line::ActiveModel = ol.into();
            active.billed_qty = Set(base - billed);
            active.update(&txn).await?;
        }

        let customer = load_customer(&txn, existing.customer_id).await?;
        let totals = totals_for(&txn, &existing, &lines, customer.tax_exempt).await?;
        let rate = existing.exchange_rate;
        let net_base = round_money((totals.total - totals.tax) * rate);
        let tax_base = round_money(totals.tax * rate);
        let gross_base = round_money(totals.total * rate);
        let now = chrono::Utc::now();
        let request = gl::ar_invoice_request(
            format!("sales.invoice:{id}:cancel"),
            format!(
                "Cancellation of sales invoice {}",
                existing.number.as_deref().unwrap_or("?")
            ),
            now.date_naive(),
            net_base,
            tax_base,
            gross_base,
            true,
            gl.tenant_id(),
        )?;
        if let Some(req) = &request {
            gl::stage(&txn, req).await?;
        }
        let mut active: invoice::ActiveModel = existing.into();
        active.status = Set(InvoiceStatus::Cancelled.as_str().to_string());
        active.cancelled_at = Set(Some(now));
        active.cancelled_by = Set(by);
        active.cancel_reason = Set(Some(reason.trim().to_string()).filter(|r| !r.is_empty()));
        active.updated_at = Set(now);
        active.update(&txn).await?;
        txn.commit().await?;
        if let Some(req) = request {
            gl.publish(req).await;
        }
        self.view(id).await
    }

    pub async fn list(&self, filter: InvoiceFilter) -> Result<Vec<InvoiceHeader>> {
        let mut query = invoice::Entity::find();
        if let Some(customer_id) = filter.customer_id {
            query = query.filter(invoice::Column::CustomerId.eq(customer_id));
        }
        if let Some(order_id) = filter.order_id {
            query = query.filter(invoice::Column::OrderId.eq(order_id));
        }
        if let Some(s) = filter.status {
            query = query.filter(invoice::Column::Status.eq(s.as_str()));
        }
        if let Some(from) = filter.from {
            query = query.filter(invoice::Column::InvoiceDate.gte(from));
        }
        if let Some(to) = filter.to {
            query = query.filter(invoice::Column::InvoiceDate.lte(to));
        }
        let rows = query
            .order_by_desc(invoice::Column::InvoiceDate)
            .order_by_desc(invoice::Column::CreatedAt)
            .all(&self.db)
            .await?;
        let customer_ids: Vec<Uuid> = rows.iter().map(|r| r.customer_id).collect();
        let customers: HashMap<Uuid, customer::Model> = customer::Entity::find()
            .filter(customer::Column::Id.is_in(customer_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|c| (c.id, c))
            .collect();
        let invoice_ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
        let paid = super::payment::paid_amounts(&self.db, &invoice_ids).await?;
        let mut headers = Vec::with_capacity(rows.len());
        for r in rows {
            let status = InvoiceStatus::parse(&r.status)?;
            let cust = customers.get(&r.customer_id);
            let lines = load_invoice_lines(&self.db, r.id).await?;
            let totals = totals_for(
                &self.db,
                &r,
                &lines,
                cust.map(|c| c.tax_exempt).unwrap_or(false),
            )
            .await?;
            let paid_amt = paid.get(&r.id).copied().unwrap_or(Decimal::ZERO);
            let outstanding = if status == InvoiceStatus::Posted {
                (totals.total - paid_amt).max(Decimal::ZERO)
            } else {
                Decimal::ZERO
            };
            headers.push(InvoiceHeader {
                id: r.id,
                number: r.number.clone(),
                customer_id: r.customer_id,
                customer_name: cust.map(|c| c.name.clone()).unwrap_or_default(),
                invoice_date: r.invoice_date,
                due_date: r.due_date,
                currency: r.currency.clone(),
                total: totals.total,
                status,
                outstanding,
                settlement: settlement_status(paid_amt, totals.total, status),
            });
        }
        Ok(headers)
    }

    /// Load a full invoice with lines, labels and computed totals.
    pub async fn view(&self, id: Uuid) -> Result<InvoiceView> {
        let row = load_invoice(&self.db, id).await?;
        let lines = load_invoice_lines(&self.db, id).await?;
        let customer = customer::Entity::find_by_id(row.customer_id)
            .one(&self.db)
            .await?;
        let order_row = match row.order_id {
            Some(order_id) => Some(load_order(&self.db, order_id).await?),
            None => None,
        };
        let order_lines: HashMap<Uuid, order_line::Model> = match row.order_id {
            Some(order_id) => load_order_lines(&self.db, order_id)
                .await?
                .into_iter()
                .map(|l| (l.id, l))
                .collect(),
            None => HashMap::new(),
        };
        let items: HashMap<Uuid, item::Model> = item::Entity::find()
            .filter(
                item::Column::Id.is_in(
                    order_lines
                        .values()
                        .map(|l| l.item_id)
                        .collect::<Vec<Uuid>>(),
                ),
            )
            .all(&self.db)
            .await?
            .into_iter()
            .map(|i| (i.id, i))
            .collect();

        let tax_exempt = customer.as_ref().map(|c| c.tax_exempt).unwrap_or(false);
        let rate_ids: Vec<Uuid> = lines.iter().filter_map(|l| l.tax_code_id).collect();
        let rates = tax_rates(&self.db, &rate_ids).await?;

        let mut subtotal = Decimal::ZERO;
        let mut tax_total = Decimal::ZERO;
        let line_views: Vec<InvoiceLineView> = lines
            .iter()
            .map(|l| {
                let ol = l.order_line_id.and_then(|id| order_lines.get(&id));
                let item = ol.and_then(|ol| items.get(&ol.item_id));
                let rate = if tax_exempt {
                    Decimal::ZERO
                } else {
                    l.tax_code_id
                        .and_then(|id| rates.get(&id).copied())
                        .unwrap_or(Decimal::ZERO)
                };
                let line_amt = round_money(l.qty * effective_price(l.unit_price, l.discount_pct));
                let (net, tax) = if row.tax_inclusive {
                    let n = round_money(line_amt / (Decimal::ONE + rate / Decimal::ONE_HUNDRED));
                    (n, line_amt - n)
                } else {
                    (
                        line_amt,
                        round_money(line_amt * rate / Decimal::ONE_HUNDRED),
                    )
                };
                subtotal += net;
                tax_total += tax;
                InvoiceLineView {
                    id: l.id,
                    line_no: l.line_no,
                    order_line_id: l.order_line_id,
                    item_id: ol.map(|ol| ol.item_id),
                    sku: item.map(|i| i.sku.clone()).unwrap_or_default(),
                    description: l.description.clone(),
                    qty: l.qty,
                    unit_price: l.unit_price,
                    discount_pct: l.discount_pct,
                    tax_code_id: l.tax_code_id,
                    net,
                    tax,
                    memo: l.memo.clone(),
                }
            })
            .collect();

        let mut net_after_header = subtotal;
        if let Some(pct) = row.discount_pct {
            net_after_header -= round_money(subtotal * pct / Decimal::ONE_HUNDRED);
        }
        if let Some(a) = row.discount_amount {
            net_after_header -= a;
        }
        if let Some(c) = row.other_charges {
            net_after_header += c;
        }
        let total = round_money(net_after_header + tax_total);

        let status = InvoiceStatus::parse(&row.status)?;
        let paid_amount = super::payment::paid_amounts(&self.db, &[row.id])
            .await?
            .get(&row.id)
            .copied()
            .unwrap_or(Decimal::ZERO);
        let outstanding = if status == InvoiceStatus::Posted {
            (total - paid_amount).max(Decimal::ZERO)
        } else {
            Decimal::ZERO
        };

        Ok(InvoiceView {
            id: row.id,
            number: row.number,
            customer_id: row.customer_id,
            customer_name: customer.map(|c| c.name).unwrap_or_default(),
            order_id: row.order_id,
            order_number: order_row.and_then(|o| o.number),
            invoice_date: row.invoice_date,
            due_date: row.due_date,
            payment_terms_days: row.payment_terms_days,
            currency: row.currency,
            exchange_rate: row.exchange_rate,
            tax_inclusive: row.tax_inclusive,
            discount_pct: row.discount_pct,
            discount_amount: row.discount_amount,
            other_charges: row.other_charges,
            customer_po_no: row.customer_po_no,
            attachment_file_id: row.attachment_file_id,
            memo: row.memo,
            status,
            cancel_reason: row.cancel_reason,
            subtotal,
            tax: tax_total,
            total,
            paid_amount,
            outstanding,
            settlement: settlement_status(paid_amount, total, status),
            posted_at: row.posted_at,
            created_at: row.created_at,
            lines: line_views,
        })
    }
}

fn clean(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// Draft-time validation. Returns the order so callers can snapshot its
/// customer, currency and rate.
async fn validate_invoice<C: ConnectionTrait>(
    conn: &C,
    new: &NewInvoice,
) -> Result<order::order::Model> {
    if new.lines.is_empty() {
        return Err(Error::Validation(
            "an invoice needs at least one line".into(),
        ));
    }
    if new.exchange_rate.is_some_and(|r| r <= Decimal::ZERO) {
        return Err(Error::Validation("exchange rate must be positive".into()));
    }
    if new.payment_terms_days.is_some_and(|d| d < 0) {
        return Err(Error::Validation(
            "payment terms must not be negative".into(),
        ));
    }
    let order_row = load_order(conn, new.order_id).await?;
    let status = OrderStatus::parse(&order_row.status)?;
    if matches!(status, OrderStatus::Draft | OrderStatus::Cancelled) {
        return Err(Error::Validation(format!(
            "sales order {} is {} and cannot be billed",
            order_row.number.as_deref().unwrap_or("?"),
            status.as_str()
        )));
    }
    let order_lines: HashMap<Uuid, order_line::Model> = load_order_lines(conn, new.order_id)
        .await?
        .into_iter()
        .map(|l| (l.id, l))
        .collect();
    for (i, l) in new.lines.iter().enumerate() {
        let line_no = i + 1;
        match l.order_line_id {
            Some(ol_id) if order_lines.contains_key(&ol_id) => {}
            _ => {
                return Err(Error::Validation(format!(
                    "line {line_no} does not belong to this order"
                )));
            }
        }
        if l.qty <= Decimal::ZERO {
            return Err(Error::Validation(format!(
                "line {line_no}: quantity must be positive"
            )));
        }
        if l.unit_price < Decimal::ZERO {
            return Err(Error::Validation(format!(
                "line {line_no}: unit price must not be negative"
            )));
        }
        if let Some(pct) = l.discount_pct {
            if pct < Decimal::ZERO || pct > Decimal::ONE_HUNDRED {
                return Err(Error::Validation(format!(
                    "line {line_no}: discount must be between 0 and 100 percent"
                )));
            }
        }
    }
    Ok(order_row)
}

/// Insert lines, defaulting description, price, discount and tax code from
/// the order line so a bill can be raised by naming quantities alone.
async fn insert_lines(
    txn: &DatabaseTransaction,
    invoice_id: Uuid,
    lines: &[InvoiceLineInput],
    order_row: &order::order::Model,
) -> Result<()> {
    let order_lines: HashMap<Uuid, order_line::Model> = load_order_lines(txn, order_row.id)
        .await?
        .into_iter()
        .map(|l| (l.id, l))
        .collect();
    let items: HashMap<Uuid, item::Model> = item::Entity::find()
        .filter(
            item::Column::Id.is_in(
                order_lines
                    .values()
                    .map(|l| l.item_id)
                    .collect::<Vec<Uuid>>(),
            ),
        )
        .all(txn)
        .await?
        .into_iter()
        .map(|i| (i.id, i))
        .collect();
    let now = chrono::Utc::now();
    for (i, l) in lines.iter().enumerate() {
        let ol = l.order_line_id.and_then(|id| order_lines.get(&id));
        let description = match l.description.clone().filter(|d| !d.trim().is_empty()) {
            Some(d) => d,
            None => ol
                .and_then(|ol| {
                    ol.description
                        .clone()
                        .or_else(|| items.get(&ol.item_id).map(|i| i.name.clone()))
                })
                .unwrap_or_else(|| "Line".to_string()),
        };
        invoice_line::ActiveModel {
            id: Set(Uuid::new_v4()),
            invoice_id: Set(invoice_id),
            order_line_id: Set(l.order_line_id),
            line_no: Set((i + 1) as i32),
            description: Set(description),
            qty: Set(l.qty),
            unit_price: Set(l.unit_price),
            discount_pct: Set(l.discount_pct.or_else(|| ol.and_then(|o| o.discount_pct))),
            tax_code_id: Set(l.tax_code_id.or_else(|| ol.and_then(|o| o.tax_code_id))),
            memo: Set(l.memo.clone().filter(|m| !m.trim().is_empty())),
            created_at: Set(now),
        }
        .insert(txn)
        .await?;
    }
    Ok(())
}

async fn load_customer<C: ConnectionTrait>(conn: &C, id: Uuid) -> Result<customer::Model> {
    customer::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("customer {id}")))
}

async fn load_invoice<C: ConnectionTrait>(conn: &C, id: Uuid) -> Result<invoice::Model> {
    invoice::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("sales invoice {id}")))
}

pub(crate) async fn load_invoice_locked(
    txn: &DatabaseTransaction,
    id: Uuid,
) -> Result<invoice::Model> {
    invoice::Entity::find_by_id(id)
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("sales invoice {id}")))
}

pub(crate) async fn load_invoice_lines<C: ConnectionTrait>(
    conn: &C,
    invoice_id: Uuid,
) -> Result<Vec<invoice_line::Model>> {
    invoice_line::Entity::find()
        .filter(invoice_line::Column::InvoiceId.eq(invoice_id))
        .order_by_asc(invoice_line::Column::LineNo)
        .all(conn)
        .await
        .map_err(Error::from)
}

/// The document totals for an invoice row and its lines, resolving the tax
/// rates over the seam.
pub(crate) async fn totals_for<C: ConnectionTrait>(
    conn: &C,
    row: &invoice::Model,
    lines: &[invoice_line::Model],
    tax_exempt: bool,
) -> Result<Totals> {
    let rate_ids: Vec<Uuid> = lines.iter().filter_map(|l| l.tax_code_id).collect();
    let rates = tax_rates(conn, &rate_ids).await?;
    let tax_lines: Vec<TaxLine> = lines
        .iter()
        .map(|l| TaxLine {
            qty: l.qty,
            unit_price: l.unit_price,
            discount_pct: l.discount_pct,
            tax_code_id: l.tax_code_id,
        })
        .collect();
    Ok(compute_totals(
        &tax_lines,
        &rates,
        row.tax_inclusive,
        tax_exempt,
        row.discount_pct,
        row.discount_amount,
        row.other_charges,
    ))
}

/// The invoice's gross total in its own currency — shared with the payment
/// module so settlement checks use exactly the figure the view shows.
pub(crate) async fn invoice_total<C: ConnectionTrait>(conn: &C, id: Uuid) -> Result<Decimal> {
    let row = load_invoice(conn, id).await?;
    let lines = load_invoice_lines(conn, id).await?;
    let customer = customer::Entity::find_by_id(row.customer_id)
        .one(conn)
        .await?;
    let totals = totals_for(
        conn,
        &row,
        &lines,
        customer.map(|c| c.tax_exempt).unwrap_or(false),
    )
    .await?;
    Ok(totals.total)
}

/// How much of a posted invoice has been settled.
fn settlement_status(paid: Decimal, total: Decimal, status: InvoiceStatus) -> SettlementStatus {
    if status != InvoiceStatus::Posted || paid <= Decimal::ZERO {
        SettlementStatus::Unpaid
    } else if paid < total {
        SettlementStatus::PartiallyPaid
    } else {
        SettlementStatus::Paid
    }
}

// ---------------------------------------------------------------------------
// Views (API DTOs)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = SalesInvoiceLineView)]
pub struct InvoiceLineView {
    pub id: Uuid,
    pub line_no: i32,
    pub order_line_id: Option<Uuid>,
    pub item_id: Option<Uuid>,
    pub sku: String,
    pub description: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub unit_price: Decimal,
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub discount_pct: Option<Decimal>,
    pub tax_code_id: Option<Uuid>,
    /// Line net after the line discount, before tax.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub net: Decimal,
    /// Tax on the line.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub tax: Decimal,
    pub memo: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = SalesInvoiceView)]
pub struct InvoiceView {
    pub id: Uuid,
    pub number: Option<String>,
    pub customer_id: Uuid,
    pub customer_name: String,
    pub order_id: Option<Uuid>,
    pub order_number: Option<String>,
    #[schema(value_type = String, format = Date)]
    pub invoice_date: chrono::NaiveDate,
    #[schema(value_type = Option<String>, format = Date)]
    pub due_date: Option<chrono::NaiveDate>,
    pub payment_terms_days: Option<i32>,
    pub currency: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub exchange_rate: Decimal,
    pub tax_inclusive: bool,
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub discount_pct: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub discount_amount: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub other_charges: Option<Decimal>,
    pub customer_po_no: Option<String>,
    pub attachment_file_id: Option<Uuid>,
    pub memo: Option<String>,
    pub status: InvoiceStatus,
    pub cancel_reason: Option<String>,
    /// Sum of line nets, invoice currency (before header effects).
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub subtotal: Decimal,
    /// Total tax.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub tax: Decimal,
    /// Gross payable: net after header effects, plus tax.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total: Decimal,
    /// Sum of posted payment allocations against this invoice.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub paid_amount: Decimal,
    /// `total − paid_amount` while posted; zero for drafts and cancellations.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub outstanding: Decimal,
    pub settlement: SettlementStatus,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub posted_at: Option<chrono::DateTime<chrono::Utc>>,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub lines: Vec<InvoiceLineView>,
}

/// A row of the invoice register.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = SalesInvoiceHeader)]
pub struct InvoiceHeader {
    pub id: Uuid,
    pub number: Option<String>,
    pub customer_id: Uuid,
    pub customer_name: String,
    #[schema(value_type = String, format = Date)]
    pub invoice_date: chrono::NaiveDate,
    #[schema(value_type = Option<String>, format = Date)]
    pub due_date: Option<chrono::NaiveDate>,
    pub currency: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total: Decimal,
    pub status: InvoiceStatus,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub outstanding: Decimal,
    pub settlement: SettlementStatus,
}

pub struct InvoiceFilter {
    pub customer_id: Option<Uuid>,
    pub order_id: Option<Uuid>,
    pub status: Option<InvoiceStatus>,
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = SalesInvoiceLineRequest)]
pub struct InvoiceLineRequest {
    pub order_line_id: Uuid,
    /// Defaults from the order line's description or item name.
    pub description: Option<String>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub unit_price: Decimal,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub discount_pct: Option<Decimal>,
    pub tax_code_id: Option<Uuid>,
    pub memo: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = CreateSalesInvoiceRequest)]
pub struct CreateInvoiceRequest {
    pub order_id: Uuid,
    #[schema(value_type = String, format = Date)]
    pub invoice_date: chrono::NaiveDate,
    #[schema(value_type = Option<String>, format = Date)]
    pub due_date: Option<chrono::NaiveDate>,
    pub payment_terms_days: Option<i32>,
    /// Defaults to the order's rate.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub exchange_rate: Option<Decimal>,
    #[serde(default)]
    pub tax_inclusive: bool,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub discount_pct: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub discount_amount: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub other_charges: Option<Decimal>,
    pub customer_po_no: Option<String>,
    pub attachment_file_id: Option<Uuid>,
    pub memo: Option<String>,
    pub lines: Vec<InvoiceLineRequest>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = CancelSalesInvoiceRequest)]
pub struct CancelInvoiceRequest {
    #[serde(default)]
    pub reason: String,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListInvoicesQuery {
    pub customer_id: Option<Uuid>,
    pub order_id: Option<Uuid>,
    pub status: Option<InvoiceStatus>,
    /// Invoice date range, inclusive.
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

fn new_invoice(req: CreateInvoiceRequest, created_by: Option<Uuid>) -> NewInvoice {
    NewInvoice {
        order_id: req.order_id,
        invoice_date: req.invoice_date,
        due_date: req.due_date,
        payment_terms_days: req.payment_terms_days,
        exchange_rate: req.exchange_rate,
        tax_inclusive: req.tax_inclusive,
        discount_pct: req.discount_pct,
        discount_amount: req.discount_amount,
        other_charges: req.other_charges,
        customer_po_no: req.customer_po_no,
        attachment_file_id: req.attachment_file_id,
        memo: req.memo,
        lines: req
            .lines
            .into_iter()
            .map(|l| InvoiceLineInput {
                order_line_id: Some(l.order_line_id),
                description: l.description,
                qty: l.qty,
                unit_price: l.unit_price,
                discount_pct: l.discount_pct,
                tax_code_id: l.tax_code_id,
                memo: l.memo,
            })
            .collect(),
        created_by,
    }
}

pub(crate) fn routes() -> Router {
    Router::new()
        .route("/sales/invoices", get(list_invoices).post(create_invoice))
        .route(
            "/sales/invoices/{id}",
            get(get_invoice).put(update_invoice).delete(delete_invoice),
        )
        .route("/sales/invoices/{id}/post", post(post_invoice))
        .route("/sales/invoices/{id}/cancel", post(cancel_invoice))
        .route("/sales/orders/{id}/invoices", get(order_invoices))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    list_invoices,
    get_invoice,
    create_invoice,
    update_invoice,
    delete_invoice,
    post_invoice,
    cancel_invoice,
    order_invoices
))]
struct ApiDoc;

#[utoipa::path(get, path = "/sales/invoices", tag = "sales",
    params(ListInvoicesQuery),
    responses((status = 200, body = Vec<InvoiceHeader>)))]
async fn list_invoices(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<ListInvoicesQuery>,
) -> Result<Json<Vec<InvoiceHeader>>> {
    authz.require(names::INVOICES_VIEW).await?;
    InvoiceService::new(db)
        .list(InvoiceFilter {
            customer_id: q.customer_id,
            order_id: q.order_id,
            status: q.status,
            from: q.from,
            to: q.to,
        })
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/sales/orders/{id}/invoices", tag = "sales",
    params(("id" = Uuid, Path, description = "Order id")),
    responses((status = 200, body = Vec<InvoiceHeader>)))]
async fn order_invoices(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<InvoiceHeader>>> {
    authz.require(names::INVOICES_VIEW).await?;
    InvoiceService::new(db)
        .list(InvoiceFilter {
            customer_id: None,
            order_id: Some(id),
            status: None,
            from: None,
            to: None,
        })
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/sales/invoices/{id}", tag = "sales",
    params(("id" = Uuid, Path, description = "Invoice id")),
    responses((status = 200, body = InvoiceView)))]
async fn get_invoice(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<InvoiceView>> {
    authz.require(names::INVOICES_VIEW).await?;
    InvoiceService::new(db).view(id).await.map(Json)
}

#[utoipa::path(post, path = "/sales/invoices", tag = "sales",
    request_body = CreateInvoiceRequest,
    responses((status = 200, body = InvoiceView)))]
async fn create_invoice(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(req): Json<CreateInvoiceRequest>,
) -> Result<Json<InvoiceView>> {
    authz.require(names::INVOICES_CREATE).await?;
    let view = InvoiceService::new(db)
        .create_draft(new_invoice(req, Some(authz.user.id)))
        .await?;
    audit.0.created("scm.sales_invoice", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(put, path = "/sales/invoices/{id}", tag = "sales",
    params(("id" = Uuid, Path, description = "Invoice id")),
    request_body = CreateInvoiceRequest,
    responses((status = 200, body = InvoiceView)))]
async fn update_invoice(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<CreateInvoiceRequest>,
) -> Result<Json<InvoiceView>> {
    authz.require(names::INVOICES_CREATE).await?;
    let service = InvoiceService::new(db);
    let before = service.view(id).await?;
    let after = service.update_draft(id, new_invoice(req, None)).await?;
    audit
        .0
        .updated("scm.sales_invoice", id, &before, &after)
        .await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/sales/invoices/{id}", tag = "sales",
    params(("id" = Uuid, Path, description = "Invoice id")),
    responses((status = 200, body = InvoiceView)))]
async fn delete_invoice(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<InvoiceView>> {
    authz.require(names::INVOICES_CREATE).await?;
    let view = InvoiceService::new(db).delete_draft(id).await?;
    audit.0.deleted("scm.sales_invoice", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/sales/invoices/{id}/post", tag = "sales",
    params(("id" = Uuid, Path, description = "Invoice id")),
    responses((status = 200, body = InvoiceView)))]
async fn post_invoice(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    CurrentTenant(tenant): CurrentTenant,
    Extension(numbering): Extension<Numbering>,
    Extension(events): Extension<Events>,
    Path(id): Path<Uuid>,
) -> Result<Json<InvoiceView>> {
    authz.require(names::INVOICES_POST).await?;
    let gl = gl::Gl::new(events, tenant.map(|t| t.id));
    let view = InvoiceService::new(db)
        .post(id, &numbering, Some(authz.user.id), &gl)
        .await?;
    audit
        .0
        .event(format!(
            "posted sales invoice {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/sales/invoices/{id}/cancel", tag = "sales",
    params(("id" = Uuid, Path, description = "Invoice id")),
    request_body = CancelInvoiceRequest,
    responses((status = 200, body = InvoiceView)))]
async fn cancel_invoice(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    CurrentTenant(tenant): CurrentTenant,
    Extension(events): Extension<Events>,
    Path(id): Path<Uuid>,
    Json(req): Json<CancelInvoiceRequest>,
) -> Result<Json<InvoiceView>> {
    authz.require(names::INVOICES_CANCEL).await?;
    let gl = gl::Gl::new(events, tenant.map(|t| t.id));
    let view = InvoiceService::new(db)
        .cancel(id, &req.reason, Some(authz.user.id), &gl)
        .await?;
    audit
        .0
        .event(format!(
            "cancelled sales invoice {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}
