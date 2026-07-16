//! Purchase invoices (vendor bills) and the three-way match.
//!
//! Posting validates every line against the purchase order under the order
//! row lock — the classic control against paying for goods never received:
//!
//! 1. **Authorized** — the line references a line of the named PO, and the
//!    PO has been approved.
//! 2. **Received** — `billed_qty + qty ≤ received_qty`: only what has
//!    actually arrived can be billed (strict-received keeps the GRNI
//!    report meaningful).
//! 3. **Priced** — the line's effective price equals the order line's
//!    exactly (a tolerance percentage becomes a tenant setting later; the
//!    check lives in one place).
//!
//! A supplier cannot bill the same document twice —
//! `(supplier_id, supplier_invoice_no)` conflicts. Cancelling a posted
//! invoice restores `billed_qty` (payments arrive with accounting's
//! payment phase; until then the AP side is the invoice register).
//! Totals are computed on the view, never persisted redundantly.

use crate::scm::gl;
use crate::scm::inventory::item::item;
use crate::scm::inventory::stock;
use crate::scm::procurement::order::{
    self, OrderStatus, effective_price, load_lines as load_order_lines, load_order,
    load_order_locked, order_line,
};
use crate::scm::procurement::permissions::names;
use crate::scm::procurement::supplier::supplier;
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
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, QueryOrder, QuerySelect, Set,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Where a purchase invoice is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
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

/// How much of a posted invoice has been paid: `unpaid` (nothing settled
/// or the invoice is not posted), `partially_paid`, or `paid` in full.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SettlementStatus {
    Unpaid,
    PartiallyPaid,
    Paid,
}

/// The purchase invoice header.
pub mod invoice {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "procurement_invoices")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub number: Option<String>,
        pub supplier_id: Uuid,
        pub order_id: Option<Uuid>,
        pub supplier_invoice_no: String,
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

/// One purchase invoice line, against an order line.
pub mod invoice_line {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "procurement_invoice_lines")]
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
// Service
// ---------------------------------------------------------------------------

/// An invoice line as supplied by a caller. `qty` and price are in the
/// order line's UoM and currency.
pub struct InvoiceLineInput {
    pub order_line_id: Uuid,
    pub description: Option<String>,
    pub qty: Decimal,
    pub unit_price: Decimal,
    pub discount_pct: Option<Decimal>,
    pub tax_code_id: Option<Uuid>,
    pub memo: Option<String>,
}

/// A new draft purchase invoice as supplied by a caller. Direct bills
/// (no purchase order) are a later phase — `order_id` is required.
pub struct NewInvoice {
    pub supplier_id: Uuid,
    pub order_id: Uuid,
    pub supplier_invoice_no: String,
    pub invoice_date: chrono::NaiveDate,
    pub due_date: Option<chrono::NaiveDate>,
    pub payment_terms_days: Option<i32>,
    pub exchange_rate: Option<Decimal>,
    pub tax_inclusive: bool,
    pub discount_pct: Option<Decimal>,
    pub discount_amount: Option<Decimal>,
    pub other_charges: Option<Decimal>,
    pub attachment_file_id: Option<Uuid>,
    pub memo: Option<String>,
    pub lines: Vec<InvoiceLineInput>,
    pub created_by: Option<Uuid>,
}

/// The purchase invoice service over one (tenant) connection.
pub struct InvoiceService {
    db: DatabaseConnection,
}

impl InvoiceService {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn create_draft(&self, new: NewInvoice) -> Result<InvoiceView> {
        let order_row = validate_invoice(&self.db, &new, None).await?;
        let txn = self.db.begin().await?;
        let invoice_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        invoice::ActiveModel {
            id: Set(invoice_id),
            number: Set(None),
            supplier_id: Set(new.supplier_id),
            order_id: Set(Some(new.order_id)),
            supplier_invoice_no: Set(new.supplier_invoice_no.trim().to_string()),
            invoice_date: Set(new.invoice_date),
            due_date: Set(new.due_date),
            payment_terms_days: Set(new.payment_terms_days),
            currency: Set(order_row.currency.clone()),
            exchange_rate: Set(new.exchange_rate.unwrap_or(order_row.exchange_rate)),
            tax_inclusive: Set(new.tax_inclusive),
            discount_pct: Set(new.discount_pct),
            discount_amount: Set(new.discount_amount),
            other_charges: Set(new.other_charges),
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

    /// Replace a draft's header and lines wholesale. Supplier and order
    /// are fixed at creation.
    pub async fn update_draft(&self, id: Uuid, new: NewInvoice) -> Result<InvoiceView> {
        let txn = self.db.begin().await?;
        let existing = load_invoice_locked(&txn, id).await?;
        if InvoiceStatus::parse(&existing.status)? != InvoiceStatus::Draft {
            return Err(Error::Validation(
                "only a draft invoice can be edited".into(),
            ));
        }
        if existing.supplier_id != new.supplier_id || existing.order_id != Some(new.order_id) {
            return Err(Error::Validation(
                "an invoice's supplier and order cannot change; delete the draft and create a new one"
                    .into(),
            ));
        }
        let order_row = validate_invoice(&txn, &new, Some(existing.id)).await?;
        invoice_line::Entity::delete_many()
            .filter(invoice_line::Column::InvoiceId.eq(id))
            .exec(&txn)
            .await?;
        insert_lines(&txn, id, &new.lines, &order_row).await?;
        let mut active: invoice::ActiveModel = existing.into();
        active.supplier_invoice_no = Set(new.supplier_invoice_no.trim().to_string());
        active.invoice_date = Set(new.invoice_date);
        active.due_date = Set(new.due_date);
        active.payment_terms_days = Set(new.payment_terms_days);
        active.exchange_rate = Set(new.exchange_rate.unwrap_or(order_row.exchange_rate));
        active.tax_inclusive = Set(new.tax_inclusive);
        active.discount_pct = Set(new.discount_pct);
        active.discount_amount = Set(new.discount_amount);
        active.other_charges = Set(new.other_charges);
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

    /// Post a draft invoice: the three-way match runs under the order row
    /// lock, `billed_qty` bumps, the PINV number lands — one transaction.
    /// The GL request (Dr GRNI / Cr AP) is staged with it and published
    /// after commit.
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
        let order_status = OrderStatus::parse(&order_row.status)?;
        if matches!(
            order_status,
            OrderStatus::Draft | OrderStatus::Submitted | OrderStatus::Cancelled
        ) {
            return Err(Error::Validation(format!(
                "purchase order {} is {} and cannot be billed",
                order_row.number.as_deref().unwrap_or("?"),
                order_status.as_str()
            )));
        }

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

        // The three-way match, per line, accumulated per order line so two
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
            if ol.billed_qty + *billed > ol.received_qty {
                return Err(Error::Validation(format!(
                    "line {}: billing {} exceeds the {} received and not yet billed",
                    line.line_no,
                    line.qty,
                    ol.received_qty - ol.billed_qty
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

        let number = numbering.next(&txn, crate::scm::INVOICE_SERIES).await?;
        let now = chrono::Utc::now();
        // The GL memo names the fresh PINV number, which only lands on the
        // row below — hand the builder a copy that already carries it.
        let mut for_gl = invoice_row.clone();
        for_gl.number = Some(number.formatted.clone());
        let request = gl::purchase_invoice_request(&txn, &for_gl, false, gl.tenant_id()).await?;
        if let Some(req) = &request {
            gl::stage(&txn, req).await?;
        }
        let mut active: invoice::ActiveModel = invoice_row.into();
        active.status = Set(InvoiceStatus::Posted.as_str().to_string());
        active.number = Set(Some(number.formatted));
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

    /// Cancel a posted invoice while nothing references it (payments are a
    /// later concern); restores `billed_qty` under the order row lock and
    /// books the mirror of the posting entry.
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
        let now = chrono::Utc::now();
        let request = gl::purchase_invoice_request(&txn, &existing, true, gl.tenant_id()).await?;
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
        if let Some(supplier_id) = filter.supplier_id {
            query = query.filter(invoice::Column::SupplierId.eq(supplier_id));
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
        let supplier_ids: Vec<Uuid> = rows.iter().map(|r| r.supplier_id).collect();
        let suppliers: HashMap<Uuid, supplier::Model> = supplier::Entity::find()
            .filter(supplier::Column::Id.is_in(supplier_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|s| (s.id, s))
            .collect();
        let invoice_ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
        let paid = super::payment::paid_amounts(&self.db, &invoice_ids).await?;
        let all_lines = invoice_line::Entity::find()
            .filter(invoice_line::Column::InvoiceId.is_in(invoice_ids.clone()))
            .all(&self.db)
            .await?;
        let mut subtotals: HashMap<Uuid, Decimal> = HashMap::new();
        for l in &all_lines {
            let price = effective_price(l.unit_price, l.discount_pct);
            *subtotals.entry(l.invoice_id).or_default() += stock::round_money(l.qty * price);
        }
        rows.into_iter()
            .map(|r| {
                let status = InvoiceStatus::parse(&r.status)?;
                let total =
                    apply_header(&r, subtotals.get(&r.id).copied().unwrap_or(Decimal::ZERO));
                let paid_amt = paid.get(&r.id).copied().unwrap_or(Decimal::ZERO);
                let outstanding = if status == InvoiceStatus::Posted {
                    (total - paid_amt).max(Decimal::ZERO)
                } else {
                    Decimal::ZERO
                };
                Ok(InvoiceHeader {
                    id: r.id,
                    number: r.number.clone(),
                    supplier_id: r.supplier_id,
                    supplier_name: suppliers
                        .get(&r.supplier_id)
                        .map(|s| s.name.clone())
                        .unwrap_or_default(),
                    supplier_invoice_no: r.supplier_invoice_no.clone(),
                    invoice_date: r.invoice_date,
                    due_date: r.due_date,
                    currency: r.currency.clone(),
                    status,
                    outstanding,
                    settlement: settlement_status(paid_amt, total, status),
                })
            })
            .collect()
    }

    /// Load a full invoice with lines, labels and computed totals.
    pub async fn view(&self, id: Uuid) -> Result<InvoiceView> {
        let row = load_invoice(&self.db, id).await?;
        let lines = load_invoice_lines(&self.db, id).await?;
        let supplier = supplier::Entity::find_by_id(row.supplier_id)
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

        let mut subtotal = Decimal::ZERO;
        let line_views: Vec<InvoiceLineView> = lines
            .into_iter()
            .map(|l| {
                let ol = l.order_line_id.and_then(|id| order_lines.get(&id));
                let item = ol.and_then(|ol| items.get(&ol.item_id));
                let price = effective_price(l.unit_price, l.discount_pct);
                let net = stock::round_money(l.qty * price);
                subtotal += net;
                InvoiceLineView {
                    id: l.id,
                    line_no: l.line_no,
                    order_line_id: l.order_line_id,
                    item_id: ol.map(|ol| ol.item_id),
                    sku: item.map(|i| i.sku.clone()).unwrap_or_default(),
                    description: l.description,
                    qty: l.qty,
                    unit_price: l.unit_price,
                    discount_pct: l.discount_pct,
                    net,
                    memo: l.memo,
                }
            })
            .collect();

        let mut total = subtotal;
        if let Some(pct) = row.discount_pct {
            total -= stock::round_money(subtotal * pct / Decimal::ONE_HUNDRED);
        }
        if let Some(amount) = row.discount_amount {
            total -= amount;
        }
        if let Some(charges) = row.other_charges {
            total += charges;
        }
        let total = stock::round_money(total);

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
        let settlement = settlement_status(paid_amount, total, status);

        Ok(InvoiceView {
            id: row.id,
            number: row.number,
            supplier_id: row.supplier_id,
            supplier_name: supplier.map(|s| s.name).unwrap_or_default(),
            order_id: row.order_id,
            order_number: order_row.and_then(|o| o.number),
            supplier_invoice_no: row.supplier_invoice_no,
            invoice_date: row.invoice_date,
            due_date: row.due_date,
            payment_terms_days: row.payment_terms_days,
            currency: row.currency,
            exchange_rate: row.exchange_rate,
            tax_inclusive: row.tax_inclusive,
            discount_pct: row.discount_pct,
            discount_amount: row.discount_amount,
            other_charges: row.other_charges,
            attachment_file_id: row.attachment_file_id,
            memo: row.memo,
            status,
            cancel_reason: row.cancel_reason,
            subtotal,
            total,
            paid_amount,
            outstanding,
            settlement,
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
/// currency and rate. The duplicate supplier-document check runs here (and
/// the unique index backstops racing drafts).
async fn validate_invoice<C: ConnectionTrait>(
    conn: &C,
    new: &NewInvoice,
    existing_id: Option<Uuid>,
) -> Result<order::order::Model> {
    if new.supplier_invoice_no.trim().is_empty() {
        return Err(Error::Validation(
            "the supplier's invoice number is required".into(),
        ));
    }
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
    let supplier_row = supplier::Entity::find_by_id(new.supplier_id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("supplier {}", new.supplier_id)))?;
    if !supplier_row.is_active {
        return Err(Error::Validation(format!(
            "supplier {} is inactive",
            supplier_row.code
        )));
    }
    let order_row = load_order(conn, new.order_id).await?;
    if order_row.supplier_id != new.supplier_id {
        return Err(Error::Validation(
            "the order belongs to a different supplier".into(),
        ));
    }
    let order_lines: HashMap<Uuid, order_line::Model> = load_order_lines(conn, new.order_id)
        .await?
        .into_iter()
        .map(|l| (l.id, l))
        .collect();
    for (i, l) in new.lines.iter().enumerate() {
        let line_no = i + 1;
        if !order_lines.contains_key(&l.order_line_id) {
            return Err(Error::Validation(format!(
                "line {line_no} does not belong to this order"
            )));
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
    let dup = invoice::Entity::find()
        .filter(invoice::Column::SupplierId.eq(new.supplier_id))
        .filter(invoice::Column::SupplierInvoiceNo.eq(new.supplier_invoice_no.trim()))
        .one(conn)
        .await?;
    if dup.is_some_and(|d| existing_id.is_none_or(|e| e != d.id)) {
        return Err(Error::Conflict(format!(
            "supplier document {:?} has already been entered",
            new.supplier_invoice_no.trim()
        )));
    }
    Ok(order_row)
}

/// Insert a set of lines, defaulting descriptions from the order lines'
/// items so the paper reads well.
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
        let description = match l.description.clone().filter(|d| !d.trim().is_empty()) {
            Some(d) => d,
            None => order_lines
                .get(&l.order_line_id)
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
            order_line_id: Set(Some(l.order_line_id)),
            line_no: Set((i + 1) as i32),
            description: Set(description),
            qty: Set(l.qty),
            unit_price: Set(l.unit_price),
            discount_pct: Set(l.discount_pct),
            tax_code_id: Set(l.tax_code_id),
            memo: Set(l.memo.clone().filter(|m| !m.trim().is_empty())),
            created_at: Set(now),
        }
        .insert(txn)
        .await?;
    }
    Ok(())
}

async fn load_invoice<C: ConnectionTrait>(conn: &C, id: Uuid) -> Result<invoice::Model> {
    invoice::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("purchase invoice {id}")))
}

async fn load_invoice_locked(txn: &DatabaseTransaction, id: Uuid) -> Result<invoice::Model> {
    invoice::Entity::find_by_id(id)
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("purchase invoice {id}")))
}

async fn load_invoice_lines<C: ConnectionTrait>(
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

/// Apply the header discounts and other charges to a line subtotal.
fn apply_header(row: &invoice::Model, subtotal: Decimal) -> Decimal {
    let mut total = subtotal;
    if let Some(pct) = row.discount_pct {
        total -= stock::round_money(subtotal * pct / Decimal::ONE_HUNDRED);
    }
    if let Some(amount) = row.discount_amount {
        total -= amount;
    }
    if let Some(charges) = row.other_charges {
        total += charges;
    }
    stock::round_money(total)
}

/// The invoice's subtotal (Σ line nets) and total (after header discounts
/// and other charges), in invoice currency. Shared with the payment module
/// so settlement checks use exactly the figure the view shows.
pub(crate) async fn invoice_totals<C: ConnectionTrait>(
    conn: &C,
    id: Uuid,
) -> Result<(Decimal, Decimal)> {
    let row = load_invoice(conn, id).await?;
    let lines = load_invoice_lines(conn, id).await?;
    let mut subtotal = Decimal::ZERO;
    for l in &lines {
        let price = effective_price(l.unit_price, l.discount_pct);
        subtotal += stock::round_money(l.qty * price);
    }
    Ok((subtotal, apply_header(&row, subtotal)))
}

/// How much of a posted invoice has been settled by payments.
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
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub net: Decimal,
    pub memo: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct InvoiceView {
    pub id: Uuid,
    pub number: Option<String>,
    pub supplier_id: Uuid,
    pub supplier_name: String,
    pub order_id: Option<Uuid>,
    pub order_number: Option<String>,
    pub supplier_invoice_no: String,
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
    pub attachment_file_id: Option<Uuid>,
    pub memo: Option<String>,
    pub status: InvoiceStatus,
    pub cancel_reason: Option<String>,
    /// Sum of line nets, invoice currency.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub subtotal: Decimal,
    /// After header discounts and other charges, before tax.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total: Decimal,
    /// Sum of posted supplier-payment allocations against this invoice.
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
pub struct InvoiceHeader {
    pub id: Uuid,
    pub number: Option<String>,
    pub supplier_id: Uuid,
    pub supplier_name: String,
    pub supplier_invoice_no: String,
    #[schema(value_type = String, format = Date)]
    pub invoice_date: chrono::NaiveDate,
    #[schema(value_type = Option<String>, format = Date)]
    pub due_date: Option<chrono::NaiveDate>,
    pub currency: String,
    pub status: InvoiceStatus,
    /// Outstanding balance (posted invoices only; zero otherwise).
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub outstanding: Decimal,
    pub settlement: SettlementStatus,
}

pub struct InvoiceFilter {
    pub supplier_id: Option<Uuid>,
    pub order_id: Option<Uuid>,
    pub status: Option<InvoiceStatus>,
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
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
pub struct CreateInvoiceRequest {
    pub supplier_id: Uuid,
    pub order_id: Uuid,
    /// The supplier's document number — one entry per document.
    pub supplier_invoice_no: String,
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
    pub attachment_file_id: Option<Uuid>,
    pub memo: Option<String>,
    pub lines: Vec<InvoiceLineRequest>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CancelInvoiceRequest {
    #[serde(default)]
    pub reason: String,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListInvoicesQuery {
    pub supplier_id: Option<Uuid>,
    pub order_id: Option<Uuid>,
    pub status: Option<InvoiceStatus>,
    /// Invoice date range, inclusive.
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

fn new_invoice(req: CreateInvoiceRequest, created_by: Option<Uuid>) -> NewInvoice {
    NewInvoice {
        supplier_id: req.supplier_id,
        order_id: req.order_id,
        supplier_invoice_no: req.supplier_invoice_no,
        invoice_date: req.invoice_date,
        due_date: req.due_date,
        payment_terms_days: req.payment_terms_days,
        exchange_rate: req.exchange_rate,
        tax_inclusive: req.tax_inclusive,
        discount_pct: req.discount_pct,
        discount_amount: req.discount_amount,
        other_charges: req.other_charges,
        attachment_file_id: req.attachment_file_id,
        memo: req.memo,
        lines: req
            .lines
            .into_iter()
            .map(|l| InvoiceLineInput {
                order_line_id: l.order_line_id,
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
        .route(
            "/procurement/invoices",
            get(list_invoices).post(create_invoice),
        )
        .route(
            "/procurement/invoices/{id}",
            get(get_invoice).put(update_invoice).delete(delete_invoice),
        )
        .route("/procurement/invoices/{id}/post", post(post_invoice))
        .route("/procurement/invoices/{id}/cancel", post(cancel_invoice))
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
    cancel_invoice
))]
struct ApiDoc;

#[utoipa::path(get, path = "/procurement/invoices", tag = "procurement",
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
            supplier_id: q.supplier_id,
            order_id: q.order_id,
            status: q.status,
            from: q.from,
            to: q.to,
        })
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/procurement/invoices/{id}", tag = "procurement",
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

#[utoipa::path(post, path = "/procurement/invoices", tag = "procurement",
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
    audit.0.created("scm.invoice", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(put, path = "/procurement/invoices/{id}", tag = "procurement",
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
    audit.0.updated("scm.invoice", id, &before, &after).await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/procurement/invoices/{id}", tag = "procurement",
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
    audit.0.deleted("scm.invoice", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/invoices/{id}/post", tag = "procurement",
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
            "posted purchase invoice {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/invoices/{id}/cancel", tag = "procurement",
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
            "cancelled purchase invoice {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}
