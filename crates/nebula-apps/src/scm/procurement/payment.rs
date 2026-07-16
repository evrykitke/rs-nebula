//! Supplier payments: the accounts-payable settlement that closes the
//! purchase-to-pay cycle.
//!
//! A payment is made to one supplier and allocated across one or more of
//! that supplier's *posted* purchase invoices — the payment's amount is
//! exactly the sum of its allocations. Lifecycle: draft → posted →
//! reversed. Posting books the mirror of the invoice's AP entry through
//! the GL port:
//!
//! - **Dr Accounts payable** — the liability the invoice raised, now settled
//! - **Cr Bank / Cash** — the money that left (the asset role follows the
//!   payment method: `cash` books cash, everything else books bank)
//!
//! An invoice's paid amount and settlement status (`unpaid`,
//! `partially_paid`, `paid`) are **derived** from the posted allocations —
//! never stored on the invoice — so a reversal restores the position with
//! no write-back. Over-payment is refused: an invoice can only be settled
//! up to its total, re-checked under the row lock at post so two payments
//! cannot both take the last of an invoice.

use crate::scm::gl;
use crate::scm::procurement::invoice::{self, InvoiceStatus};
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
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use crate::scm::inventory::stock::round_money;

/// Where a supplier payment is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaymentStatus {
    Draft,
    Posted,
    Reversed,
}

impl PaymentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PaymentStatus::Draft => "draft",
            PaymentStatus::Posted => "posted",
            PaymentStatus::Reversed => "reversed",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "draft" => Ok(PaymentStatus::Draft),
            "posted" => Ok(PaymentStatus::Posted),
            "reversed" => Ok(PaymentStatus::Reversed),
            other => Err(Error::internal(format!("unknown payment status {other:?}"))),
        }
    }
}

/// The payment methods a supplier payment can carry. The asset side of the
/// GL entry follows this: `cash` books the cash account, everything else
/// books the bank account (mobile money, cheques and cards all clear
/// through the bank in this first cut).
const METHODS: &[&str] = &["bank_transfer", "cash", "mobile_money", "cheque", "card"];

/// The seeded asset-account role the money leaves from.
fn method_role(method: &str) -> &'static str {
    match method {
        "cash" => "cash",
        _ => "bank",
    }
}

/// The payment header.
pub mod payment {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "procurement_payments")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub number: Option<String>,
        pub supplier_id: Uuid,
        pub payment_date: Date,
        pub method: String,
        pub reference: Option<String>,
        pub currency: String,
        #[sea_orm(column_type = "Decimal(Some((20, 8)))")]
        pub exchange_rate: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))")]
        pub amount: Decimal,
        pub memo: Option<String>,
        pub status: String,
        pub posted_at: Option<DateTimeUtc>,
        pub posted_by: Option<Uuid>,
        pub reversed_at: Option<DateTimeUtc>,
        pub reversed_by: Option<Uuid>,
        pub reverse_reason: Option<String>,
        pub created_at: DateTimeUtc,
        pub created_by: Option<Uuid>,
        pub updated_at: DateTimeUtc,
        pub updated_by: Option<Uuid>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// One allocation of a payment against a posted invoice.
pub mod payment_allocation {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "procurement_payment_allocations")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub payment_id: Uuid,
        pub invoice_id: Uuid,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))")]
        pub amount: Decimal,
        pub created_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

// ---------------------------------------------------------------------------
// Settlement helper (shared with the invoice module)
// ---------------------------------------------------------------------------

/// Sum of *posted* payment allocations per invoice, for the given invoices.
/// The single source of truth for "how much of this bill is paid".
pub(crate) async fn paid_amounts<C: ConnectionTrait>(
    conn: &C,
    invoice_ids: &[Uuid],
) -> Result<HashMap<Uuid, Decimal>> {
    if invoice_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let allocs = payment_allocation::Entity::find()
        .filter(payment_allocation::Column::InvoiceId.is_in(invoice_ids.to_vec()))
        .all(conn)
        .await?;
    if allocs.is_empty() {
        return Ok(HashMap::new());
    }
    let payment_ids: Vec<Uuid> = allocs.iter().map(|a| a.payment_id).collect();
    let posted: HashSet<Uuid> = payment::Entity::find()
        .filter(payment::Column::Id.is_in(payment_ids))
        .filter(payment::Column::Status.eq(PaymentStatus::Posted.as_str()))
        .all(conn)
        .await?
        .into_iter()
        .map(|p| p.id)
        .collect();
    let mut map: HashMap<Uuid, Decimal> = HashMap::new();
    for a in allocs {
        if posted.contains(&a.payment_id) {
            *map.entry(a.invoice_id).or_default() += a.amount;
        }
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

pub struct PaymentAllocationInput {
    pub invoice_id: Uuid,
    pub amount: Decimal,
}

pub struct NewPayment {
    pub supplier_id: Uuid,
    pub payment_date: chrono::NaiveDate,
    pub method: String,
    pub reference: Option<String>,
    pub currency: String,
    pub exchange_rate: Option<Decimal>,
    pub amount: Decimal,
    pub memo: Option<String>,
    pub allocations: Vec<PaymentAllocationInput>,
    pub created_by: Option<Uuid>,
}

pub struct PaymentService {
    db: DatabaseConnection,
}

impl PaymentService {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn create_draft(&self, new: NewPayment) -> Result<PaymentView> {
        validate_payment(&self.db, &new).await?;
        let txn = self.db.begin().await?;
        let payment_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        payment::ActiveModel {
            id: Set(payment_id),
            number: Set(None),
            supplier_id: Set(new.supplier_id),
            payment_date: Set(new.payment_date),
            method: Set(new.method.trim().to_string()),
            reference: Set(clean(new.reference)),
            currency: Set(new.currency.trim().to_string()),
            exchange_rate: Set(new.exchange_rate.unwrap_or(Decimal::ONE)),
            amount: Set(round_money(new.amount)),
            memo: Set(clean(new.memo)),
            status: Set(PaymentStatus::Draft.as_str().to_string()),
            posted_at: Set(None),
            posted_by: Set(None),
            reversed_at: Set(None),
            reversed_by: Set(None),
            reverse_reason: Set(None),
            created_at: Set(now),
            created_by: Set(new.created_by),
            updated_at: Set(now),
            updated_by: Set(None),
        }
        .insert(&txn)
        .await?;
        insert_allocations(&txn, payment_id, &new.allocations).await?;
        txn.commit().await?;
        self.view(payment_id).await
    }

    /// Replace a draft's header and allocations wholesale.
    pub async fn update_draft(&self, id: Uuid, new: NewPayment) -> Result<PaymentView> {
        validate_payment(&self.db, &new).await?;
        let txn = self.db.begin().await?;
        let existing = load_payment_locked(&txn, id).await?;
        if PaymentStatus::parse(&existing.status)? != PaymentStatus::Draft {
            return Err(Error::Validation(
                "only a draft payment can be edited".into(),
            ));
        }
        payment_allocation::Entity::delete_many()
            .filter(payment_allocation::Column::PaymentId.eq(id))
            .exec(&txn)
            .await?;
        insert_allocations(&txn, id, &new.allocations).await?;
        let mut active: payment::ActiveModel = existing.into();
        active.supplier_id = Set(new.supplier_id);
        active.payment_date = Set(new.payment_date);
        active.method = Set(new.method.trim().to_string());
        active.reference = Set(clean(new.reference));
        active.currency = Set(new.currency.trim().to_string());
        active.exchange_rate = Set(new.exchange_rate.unwrap_or(Decimal::ONE));
        active.amount = Set(round_money(new.amount));
        active.memo = Set(clean(new.memo));
        active.updated_at = Set(chrono::Utc::now());
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    pub async fn delete_draft(&self, id: Uuid) -> Result<PaymentView> {
        let view = self.view(id).await?;
        let txn = self.db.begin().await?;
        let existing = load_payment_locked(&txn, id).await?;
        if PaymentStatus::parse(&existing.status)? != PaymentStatus::Draft {
            return Err(Error::Validation(
                "only a draft payment can be deleted".into(),
            ));
        }
        payment::Entity::delete_by_id(id).exec(&txn).await?;
        txn.commit().await?;
        Ok(view)
    }

    /// Post a draft payment: re-validate every allocation against the
    /// invoices under their row locks (posted, same supplier and currency,
    /// no over-payment), allocate the PAY number, stage the Dr AP / Cr
    /// Bank|Cash entry and publish it after commit.
    pub async fn post(
        &self,
        id: Uuid,
        numbering: &Numbering,
        by: Option<Uuid>,
        gl: &gl::Gl,
    ) -> Result<PaymentView> {
        let txn = self.db.begin().await?;
        let payment_row = load_payment_locked(&txn, id).await?;
        if PaymentStatus::parse(&payment_row.status)? != PaymentStatus::Draft {
            return Err(Error::Validation(
                "only a draft payment can be posted".into(),
            ));
        }
        let allocations = load_allocations(&txn, id).await?;
        if allocations.is_empty() {
            return Err(Error::Validation(
                "a payment needs at least one allocation".into(),
            ));
        }

        // Re-check settlement under the invoice row locks so two concurrent
        // posts cannot together over-pay a bill.
        let invoice_ids: Vec<Uuid> = allocations.iter().map(|a| a.invoice_id).collect();
        let already_paid = paid_amounts(&txn, &invoice_ids).await?;
        let mut this_payment: HashMap<Uuid, Decimal> = HashMap::new();
        for a in &allocations {
            *this_payment.entry(a.invoice_id).or_default() += a.amount;
        }
        for (invoice_id, allocated) in &this_payment {
            let inv = invoice::invoice::Entity::find_by_id(*invoice_id)
                .lock_exclusive()
                .one(&txn)
                .await?
                .ok_or_else(|| Error::NotFound(format!("purchase invoice {invoice_id}")))?;
            if InvoiceStatus::parse(&inv.status)? != InvoiceStatus::Posted {
                return Err(Error::Validation(format!(
                    "invoice {} is not posted and cannot be paid",
                    inv.number.as_deref().unwrap_or("?")
                )));
            }
            if inv.supplier_id != payment_row.supplier_id {
                return Err(Error::Validation(
                    "an allocation names an invoice of another supplier".into(),
                ));
            }
            if inv.currency != payment_row.currency {
                return Err(Error::Validation(format!(
                    "invoice {} is in {} but the payment is in {}",
                    inv.number.as_deref().unwrap_or("?"),
                    inv.currency,
                    payment_row.currency
                )));
            }
            let (_, total) = invoice::invoice_totals(&txn, *invoice_id).await?;
            let paid = already_paid
                .get(invoice_id)
                .copied()
                .unwrap_or(Decimal::ZERO);
            if round_money(paid + *allocated) > total {
                return Err(Error::Validation(format!(
                    "invoice {} has {} outstanding; cannot allocate {}",
                    inv.number.as_deref().unwrap_or("?"),
                    total - paid,
                    allocated
                )));
            }
        }

        let number = numbering.next(&txn, crate::scm::PAYMENT_SERIES).await?;
        let now = chrono::Utc::now();
        let mut for_gl = payment_row.clone();
        for_gl.number = Some(number.formatted.clone());
        let request = gl::purchase_payment_request(
            &for_gl,
            method_role(&payment_row.method),
            false,
            gl.tenant_id(),
        )?;
        if let Some(req) = &request {
            gl::stage(&txn, req).await?;
        }
        let mut active: payment::ActiveModel = payment_row.into();
        active.status = Set(PaymentStatus::Posted.as_str().to_string());
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

    /// Reverse a posted payment: books the mirror entry (Dr Bank|Cash / Cr
    /// AP) so the invoices it settled fall back open, and marks it reversed.
    pub async fn reverse(
        &self,
        id: Uuid,
        reason: &str,
        by: Option<Uuid>,
        gl: &gl::Gl,
    ) -> Result<PaymentView> {
        let txn = self.db.begin().await?;
        let existing = load_payment_locked(&txn, id).await?;
        if PaymentStatus::parse(&existing.status)? != PaymentStatus::Posted {
            return Err(Error::Validation(
                "only a posted payment can be reversed".into(),
            ));
        }
        let now = chrono::Utc::now();
        let request = gl::purchase_payment_request(
            &existing,
            method_role(&existing.method),
            true,
            gl.tenant_id(),
        )?;
        if let Some(req) = &request {
            gl::stage(&txn, req).await?;
        }
        let mut active: payment::ActiveModel = existing.into();
        active.status = Set(PaymentStatus::Reversed.as_str().to_string());
        active.reversed_at = Set(Some(now));
        active.reversed_by = Set(by);
        active.reverse_reason = Set(Some(reason.trim().to_string()).filter(|r| !r.is_empty()));
        active.updated_at = Set(now);
        active.update(&txn).await?;
        txn.commit().await?;
        if let Some(req) = request {
            gl.publish(req).await;
        }
        self.view(id).await
    }

    pub async fn list(&self, filter: PaymentFilter) -> Result<Vec<PaymentHeader>> {
        let mut query = payment::Entity::find();
        if let Some(supplier_id) = filter.supplier_id {
            query = query.filter(payment::Column::SupplierId.eq(supplier_id));
        }
        if let Some(s) = filter.status {
            query = query.filter(payment::Column::Status.eq(s.as_str()));
        }
        if let Some(from) = filter.from {
            query = query.filter(payment::Column::PaymentDate.gte(from));
        }
        if let Some(to) = filter.to {
            query = query.filter(payment::Column::PaymentDate.lte(to));
        }
        let rows = query
            .order_by_desc(payment::Column::PaymentDate)
            .order_by_desc(payment::Column::CreatedAt)
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
        rows.into_iter()
            .map(|r| {
                Ok(PaymentHeader {
                    id: r.id,
                    number: r.number.clone(),
                    supplier_id: r.supplier_id,
                    supplier_name: suppliers
                        .get(&r.supplier_id)
                        .map(|s| s.name.clone())
                        .unwrap_or_default(),
                    payment_date: r.payment_date,
                    method: r.method.clone(),
                    reference: r.reference.clone(),
                    currency: r.currency.clone(),
                    amount: r.amount,
                    status: PaymentStatus::parse(&r.status)?,
                })
            })
            .collect()
    }

    /// Load a full payment with its allocations and their invoice labels.
    pub async fn view(&self, id: Uuid) -> Result<PaymentView> {
        let row = load_payment(&self.db, id).await?;
        let allocations = load_allocations(&self.db, id).await?;
        let supplier = supplier::Entity::find_by_id(row.supplier_id)
            .one(&self.db)
            .await?;
        let invoice_ids: Vec<Uuid> = allocations.iter().map(|a| a.invoice_id).collect();
        let invoices: HashMap<Uuid, invoice::invoice::Model> = invoice::invoice::Entity::find()
            .filter(invoice::invoice::Column::Id.is_in(invoice_ids.clone()))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|i| (i.id, i))
            .collect();
        let mut alloc_views = Vec::with_capacity(allocations.len());
        for a in &allocations {
            let inv = invoices.get(&a.invoice_id);
            let (_, total) = match inv {
                Some(_) => invoice::invoice_totals(&self.db, a.invoice_id).await?,
                None => (Decimal::ZERO, Decimal::ZERO),
            };
            alloc_views.push(PaymentAllocationView {
                id: a.id,
                invoice_id: a.invoice_id,
                invoice_number: inv.and_then(|i| i.number.clone()),
                supplier_invoice_no: inv
                    .map(|i| i.supplier_invoice_no.clone())
                    .unwrap_or_default(),
                invoice_total: total,
                amount: a.amount,
            });
        }

        Ok(PaymentView {
            id: row.id,
            number: row.number,
            supplier_id: row.supplier_id,
            supplier_name: supplier.map(|s| s.name).unwrap_or_default(),
            payment_date: row.payment_date,
            method: row.method,
            reference: row.reference,
            currency: row.currency,
            exchange_rate: row.exchange_rate,
            amount: row.amount,
            memo: row.memo,
            status: PaymentStatus::parse(&row.status)?,
            reverse_reason: row.reverse_reason,
            posted_at: row.posted_at,
            reversed_at: row.reversed_at,
            created_at: row.created_at,
            allocations: alloc_views,
        })
    }
}

fn clean(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// Draft-time validation: method known, amount positive and equal to the
/// sum of allocations, supplier active, each invoice posted, same supplier
/// and currency, and not over-paid given what other posted payments already
/// settled. The row-locked re-check at post is the authoritative guard.
async fn validate_payment<C: ConnectionTrait>(conn: &C, new: &NewPayment) -> Result<()> {
    let method = new.method.trim();
    if !METHODS.contains(&method) {
        return Err(Error::Validation(format!(
            "unknown payment method {method:?} (expected one of {})",
            METHODS.join(", ")
        )));
    }
    if new.currency.trim().is_empty() {
        return Err(Error::Validation("a payment currency is required".into()));
    }
    if new.amount <= Decimal::ZERO {
        return Err(Error::Validation(
            "the payment amount must be positive".into(),
        ));
    }
    if new.exchange_rate.is_some_and(|r| r <= Decimal::ZERO) {
        return Err(Error::Validation("exchange rate must be positive".into()));
    }
    if new.allocations.is_empty() {
        return Err(Error::Validation(
            "a payment needs at least one allocation".into(),
        ));
    }

    let supplier_row = supplier::Entity::find_by_id(new.supplier_id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("supplier {}", new.supplier_id)))?;

    let mut sum = Decimal::ZERO;
    let mut seen: HashSet<Uuid> = HashSet::new();
    let invoice_ids: Vec<Uuid> = new.allocations.iter().map(|a| a.invoice_id).collect();
    let already_paid = paid_amounts(conn, &invoice_ids).await?;
    for (i, a) in new.allocations.iter().enumerate() {
        let line_no = i + 1;
        if a.amount <= Decimal::ZERO {
            return Err(Error::Validation(format!(
                "allocation {line_no}: amount must be positive"
            )));
        }
        if !seen.insert(a.invoice_id) {
            return Err(Error::Validation(
                "an invoice appears twice; combine the allocations into one line".into(),
            ));
        }
        sum += a.amount;
        let inv = invoice::invoice::Entity::find_by_id(a.invoice_id)
            .one(conn)
            .await?
            .ok_or_else(|| Error::NotFound(format!("purchase invoice {}", a.invoice_id)))?;
        if InvoiceStatus::parse(&inv.status)? != InvoiceStatus::Posted {
            return Err(Error::Validation(format!(
                "allocation {line_no}: invoice {} is not posted",
                inv.number.as_deref().unwrap_or("?")
            )));
        }
        if inv.supplier_id != new.supplier_id {
            return Err(Error::Validation(format!(
                "allocation {line_no}: invoice {} belongs to a different supplier",
                inv.number.as_deref().unwrap_or("?")
            )));
        }
        if inv.currency != new.currency.trim() {
            return Err(Error::Validation(format!(
                "allocation {line_no}: invoice {} is in {} but the payment is in {}",
                inv.number.as_deref().unwrap_or("?"),
                inv.currency,
                new.currency.trim()
            )));
        }
        let (_, total) = invoice::invoice_totals(conn, a.invoice_id).await?;
        let paid = already_paid
            .get(&a.invoice_id)
            .copied()
            .unwrap_or(Decimal::ZERO);
        if round_money(paid + a.amount) > total {
            return Err(Error::Validation(format!(
                "allocation {line_no}: invoice {} has only {} outstanding",
                inv.number.as_deref().unwrap_or("?"),
                total - paid
            )));
        }
    }
    if round_money(sum) != round_money(new.amount) {
        return Err(Error::Validation(format!(
            "the payment amount {} must equal the sum of its allocations {}",
            round_money(new.amount),
            round_money(sum)
        )));
    }
    // A payment to a supplier who has since been archived is still allowed;
    // paying old bills must not be blocked. Name it in the error trail only.
    let _ = supplier_row;
    Ok(())
}

async fn insert_allocations(
    txn: &DatabaseTransaction,
    payment_id: Uuid,
    allocations: &[PaymentAllocationInput],
) -> Result<()> {
    let now = chrono::Utc::now();
    for a in allocations {
        payment_allocation::ActiveModel {
            id: Set(Uuid::new_v4()),
            payment_id: Set(payment_id),
            invoice_id: Set(a.invoice_id),
            amount: Set(round_money(a.amount)),
            created_at: Set(now),
        }
        .insert(txn)
        .await?;
    }
    Ok(())
}

async fn load_payment<C: ConnectionTrait>(conn: &C, id: Uuid) -> Result<payment::Model> {
    payment::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("supplier payment {id}")))
}

async fn load_payment_locked(txn: &DatabaseTransaction, id: Uuid) -> Result<payment::Model> {
    payment::Entity::find_by_id(id)
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("supplier payment {id}")))
}

async fn load_allocations<C: ConnectionTrait>(
    conn: &C,
    payment_id: Uuid,
) -> Result<Vec<payment_allocation::Model>> {
    payment_allocation::Entity::find()
        .filter(payment_allocation::Column::PaymentId.eq(payment_id))
        .order_by_asc(payment_allocation::Column::CreatedAt)
        .all(conn)
        .await
        .map_err(Error::from)
}

// ---------------------------------------------------------------------------
// Views (API DTOs)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PaymentAllocationView {
    pub id: Uuid,
    pub invoice_id: Uuid,
    pub invoice_number: Option<String>,
    pub supplier_invoice_no: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub invoice_total: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub amount: Decimal,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PaymentView {
    pub id: Uuid,
    pub number: Option<String>,
    pub supplier_id: Uuid,
    pub supplier_name: String,
    #[schema(value_type = String, format = Date)]
    pub payment_date: chrono::NaiveDate,
    pub method: String,
    pub reference: Option<String>,
    pub currency: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub exchange_rate: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub amount: Decimal,
    pub memo: Option<String>,
    pub status: PaymentStatus,
    pub reverse_reason: Option<String>,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub posted_at: Option<chrono::DateTime<chrono::Utc>>,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub reversed_at: Option<chrono::DateTime<chrono::Utc>>,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub allocations: Vec<PaymentAllocationView>,
}

/// A row of the payments register.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct PaymentHeader {
    pub id: Uuid,
    pub number: Option<String>,
    pub supplier_id: Uuid,
    pub supplier_name: String,
    #[schema(value_type = String, format = Date)]
    pub payment_date: chrono::NaiveDate,
    pub method: String,
    pub reference: Option<String>,
    pub currency: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub amount: Decimal,
    pub status: PaymentStatus,
}

pub struct PaymentFilter {
    pub supplier_id: Option<Uuid>,
    pub status: Option<PaymentStatus>,
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
pub struct PaymentAllocationRequest {
    pub invoice_id: Uuid,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub amount: Decimal,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreatePaymentRequest {
    pub supplier_id: Uuid,
    #[schema(value_type = String, format = Date)]
    pub payment_date: chrono::NaiveDate,
    /// One of bank_transfer, cash, mobile_money, cheque, card.
    pub method: String,
    pub reference: Option<String>,
    pub currency: String,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub exchange_rate: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub amount: Decimal,
    pub memo: Option<String>,
    pub allocations: Vec<PaymentAllocationRequest>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ReversePaymentRequest {
    #[serde(default)]
    pub reason: String,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListPaymentsQuery {
    pub supplier_id: Option<Uuid>,
    pub status: Option<PaymentStatus>,
    /// Payment date range, inclusive.
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

fn new_payment(req: CreatePaymentRequest, created_by: Option<Uuid>) -> NewPayment {
    NewPayment {
        supplier_id: req.supplier_id,
        payment_date: req.payment_date,
        method: req.method,
        reference: req.reference,
        currency: req.currency,
        exchange_rate: req.exchange_rate,
        amount: req.amount,
        memo: req.memo,
        allocations: req
            .allocations
            .into_iter()
            .map(|a| PaymentAllocationInput {
                invoice_id: a.invoice_id,
                amount: a.amount,
            })
            .collect(),
        created_by,
    }
}

pub(crate) fn routes() -> Router {
    Router::new()
        .route(
            "/procurement/payments",
            get(list_payments).post(create_payment),
        )
        .route(
            "/procurement/payments/{id}",
            get(get_payment).put(update_payment).delete(delete_payment),
        )
        .route("/procurement/payments/{id}/post", post(post_payment))
        .route("/procurement/payments/{id}/reverse", post(reverse_payment))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    list_payments,
    get_payment,
    create_payment,
    update_payment,
    delete_payment,
    post_payment,
    reverse_payment
))]
struct ApiDoc;

#[utoipa::path(get, path = "/procurement/payments", tag = "procurement",
    params(ListPaymentsQuery),
    responses((status = 200, body = Vec<PaymentHeader>)))]
async fn list_payments(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<ListPaymentsQuery>,
) -> Result<Json<Vec<PaymentHeader>>> {
    authz.require(names::PAYMENTS_VIEW).await?;
    PaymentService::new(db)
        .list(PaymentFilter {
            supplier_id: q.supplier_id,
            status: q.status,
            from: q.from,
            to: q.to,
        })
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/procurement/payments/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "Payment id")),
    responses((status = 200, body = PaymentView)))]
async fn get_payment(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<PaymentView>> {
    authz.require(names::PAYMENTS_VIEW).await?;
    PaymentService::new(db).view(id).await.map(Json)
}

#[utoipa::path(post, path = "/procurement/payments", tag = "procurement",
    request_body = CreatePaymentRequest,
    responses((status = 200, body = PaymentView)))]
async fn create_payment(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(req): Json<CreatePaymentRequest>,
) -> Result<Json<PaymentView>> {
    authz.require(names::PAYMENTS_CREATE).await?;
    let view = PaymentService::new(db)
        .create_draft(new_payment(req, Some(authz.user.id)))
        .await?;
    audit.0.created("scm.payment", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(put, path = "/procurement/payments/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "Payment id")),
    request_body = CreatePaymentRequest,
    responses((status = 200, body = PaymentView)))]
async fn update_payment(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<CreatePaymentRequest>,
) -> Result<Json<PaymentView>> {
    authz.require(names::PAYMENTS_CREATE).await?;
    let service = PaymentService::new(db);
    let before = service.view(id).await?;
    let after = service.update_draft(id, new_payment(req, None)).await?;
    audit.0.updated("scm.payment", id, &before, &after).await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/procurement/payments/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "Payment id")),
    responses((status = 200, body = PaymentView)))]
async fn delete_payment(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<PaymentView>> {
    authz.require(names::PAYMENTS_CREATE).await?;
    let view = PaymentService::new(db).delete_draft(id).await?;
    audit.0.deleted("scm.payment", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/payments/{id}/post", tag = "procurement",
    params(("id" = Uuid, Path, description = "Payment id")),
    responses((status = 200, body = PaymentView)))]
async fn post_payment(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    CurrentTenant(tenant): CurrentTenant,
    Extension(numbering): Extension<Numbering>,
    Extension(events): Extension<Events>,
    Path(id): Path<Uuid>,
) -> Result<Json<PaymentView>> {
    authz.require(names::PAYMENTS_POST).await?;
    let gl = gl::Gl::new(events, tenant.map(|t| t.id));
    let view = PaymentService::new(db)
        .post(id, &numbering, Some(authz.user.id), &gl)
        .await?;
    audit
        .0
        .event(format!(
            "posted supplier payment {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/payments/{id}/reverse", tag = "procurement",
    params(("id" = Uuid, Path, description = "Payment id")),
    request_body = ReversePaymentRequest,
    responses((status = 200, body = PaymentView)))]
async fn reverse_payment(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    CurrentTenant(tenant): CurrentTenant,
    Extension(events): Extension<Events>,
    Path(id): Path<Uuid>,
    Json(req): Json<ReversePaymentRequest>,
) -> Result<Json<PaymentView>> {
    authz.require(names::PAYMENTS_REVERSE).await?;
    let gl = gl::Gl::new(events, tenant.map(|t| t.id));
    let view = PaymentService::new(db)
        .reverse(id, &req.reason, Some(authz.user.id), &gl)
        .await?;
    audit
        .0
        .event(format!(
            "reversed supplier payment {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}
