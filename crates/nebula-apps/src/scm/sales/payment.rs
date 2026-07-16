//! Customer payments (receipts): the accounts-receivable settlement that
//! closes the order-to-cash cycle.
//!
//! A payment is received from one customer and allocated across that
//! customer's *posted* invoices — the amount equals the sum of its
//! allocations, and any unallocated remainder stands as customer credit
//! that lowers their exposure. Lifecycle: draft → posted → reversed.
//! Posting books the mirror of the invoice's AR entry:
//!
//! - **Dr Bank / Cash** — the money received (the asset role follows the
//!   method: `cash` books cash, everything else books bank)
//! - **Cr Accounts receivable** — the receivable now settled
//!
//! An invoice's settled amount (posted payment allocations **plus** posted
//! credit notes against it) and settlement status are **derived**, never
//! stored, so a reversal restores the position with no write-back.
//! Over-settlement is refused: an invoice can only be paid down to zero,
//! re-checked under the row lock at post so two payments cannot both take
//! the last of a bill.

use crate::scm::gl;
use crate::scm::sales::credit_note::{self, CreditNoteStatus};
use crate::scm::sales::customer::customer;
use crate::scm::sales::invoice::{self, InvoiceStatus};
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
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, QueryOrder, QuerySelect, Set,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use crate::scm::inventory::stock::round_money;

/// Where a customer payment is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = SalesPaymentStatus)]
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

/// The payment methods a customer payment can carry. The asset side of the
/// GL entry follows this: `cash` books the cash account, everything else
/// the bank account.
const METHODS: &[&str] = &["bank_transfer", "cash", "mobile_money", "cheque", "card"];

/// The seeded asset-account role the money lands in.
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
    #[sea_orm(table_name = "sales_payments")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub number: Option<String>,
        pub customer_id: Uuid,
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
    #[sea_orm(table_name = "sales_payment_allocations")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub payment_id: Uuid,
        pub invoice_id: Option<Uuid>,
        pub credit_note_id: Option<Uuid>,
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

/// The settled amount per invoice: posted payment allocations *plus* the
/// gross of posted credit notes raised against the invoice. The single
/// source of truth for "how much of this bill is cleared", so the invoice
/// view and the credit check agree.
pub(crate) async fn paid_amounts<C: ConnectionTrait>(
    conn: &C,
    invoice_ids: &[Uuid],
) -> Result<HashMap<Uuid, Decimal>> {
    if invoice_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut map: HashMap<Uuid, Decimal> = HashMap::new();

    // Posted payment allocations.
    let allocs = payment_allocation::Entity::find()
        .filter(payment_allocation::Column::InvoiceId.is_in(invoice_ids.to_vec()))
        .all(conn)
        .await?;
    if !allocs.is_empty() {
        let payment_ids: Vec<Uuid> = allocs.iter().map(|a| a.payment_id).collect();
        let posted: HashSet<Uuid> = payment::Entity::find()
            .filter(payment::Column::Id.is_in(payment_ids))
            .filter(payment::Column::Status.eq(PaymentStatus::Posted.as_str()))
            .all(conn)
            .await?
            .into_iter()
            .map(|p| p.id)
            .collect();
        for a in allocs {
            if let Some(inv_id) = a.invoice_id {
                if posted.contains(&a.payment_id) {
                    *map.entry(inv_id).or_default() += a.amount;
                }
            }
        }
    }

    // Posted credit notes against these invoices reduce the balance too.
    let notes = credit_note::credit_note::Entity::find()
        .filter(credit_note::credit_note::Column::InvoiceId.is_in(invoice_ids.to_vec()))
        .filter(credit_note::credit_note::Column::Status.eq(CreditNoteStatus::Posted.as_str()))
        .all(conn)
        .await?;
    for n in notes {
        let total = credit_note::credit_note_total(conn, n.id).await?;
        *map.entry(n.invoice_id).or_default() += total;
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
    pub customer_id: Uuid,
    pub payment_date: chrono::NaiveDate,
    pub method: String,
    pub reference: Option<String>,
    pub currency: Option<String>,
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
        let currency = validate_payment(&self.db, &new).await?;
        let txn = self.db.begin().await?;
        let payment_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        payment::ActiveModel {
            id: Set(payment_id),
            number: Set(None),
            customer_id: Set(new.customer_id),
            payment_date: Set(new.payment_date),
            method: Set(new.method.trim().to_string()),
            reference: Set(clean(new.reference)),
            currency: Set(currency),
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

    pub async fn update_draft(&self, id: Uuid, new: NewPayment) -> Result<PaymentView> {
        let currency = validate_payment(&self.db, &new).await?;
        let txn = self.db.begin().await?;
        let existing = load_payment_locked(&txn, id).await?;
        if PaymentStatus::parse(&existing.status)? != PaymentStatus::Draft {
            return Err(Error::Validation("only a draft payment can be edited".into()));
        }
        payment_allocation::Entity::delete_many()
            .filter(payment_allocation::Column::PaymentId.eq(id))
            .exec(&txn)
            .await?;
        insert_allocations(&txn, id, &new.allocations).await?;
        let mut active: payment::ActiveModel = existing.into();
        active.customer_id = Set(new.customer_id);
        active.payment_date = Set(new.payment_date);
        active.method = Set(new.method.trim().to_string());
        active.reference = Set(clean(new.reference));
        active.currency = Set(currency);
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
            return Err(Error::Validation("only a draft payment can be deleted".into()));
        }
        payment::Entity::delete_by_id(id).exec(&txn).await?;
        txn.commit().await?;
        Ok(view)
    }

    /// Post a draft payment: re-validate every allocation against the
    /// invoices under their row locks (posted, same customer and currency,
    /// no over-settlement), allocate the RCT number, stage Dr Bank|Cash /
    /// Cr AR and publish it after commit.
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
            return Err(Error::Validation("only a draft payment can be posted".into()));
        }
        let allocations = load_allocations(&txn, id).await?;
        if allocations.is_empty() {
            return Err(Error::Validation(
                "a payment needs at least one allocation".into(),
            ));
        }

        // Re-check settlement under the invoice row locks so two concurrent
        // posts cannot together over-settle a bill.
        let invoice_ids: Vec<Uuid> = allocations.iter().filter_map(|a| a.invoice_id).collect();
        let already_settled = paid_amounts(&txn, &invoice_ids).await?;
        let mut this_payment: HashMap<Uuid, Decimal> = HashMap::new();
        for a in &allocations {
            if let Some(inv_id) = a.invoice_id {
                *this_payment.entry(inv_id).or_default() += a.amount;
            }
        }
        for (invoice_id, allocated) in &this_payment {
            let inv = invoice::load_invoice_locked(&txn, *invoice_id).await?;
            if InvoiceStatus::parse(&inv.status)? != InvoiceStatus::Posted {
                return Err(Error::Validation(format!(
                    "invoice {} is not posted and cannot be paid",
                    inv.number.as_deref().unwrap_or("?")
                )));
            }
            if inv.customer_id != payment_row.customer_id {
                return Err(Error::Validation(
                    "an allocation names an invoice of another customer".into(),
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
            let total = invoice::invoice_total(&txn, *invoice_id).await?;
            let settled = already_settled.get(invoice_id).copied().unwrap_or(Decimal::ZERO);
            if round_money(settled + *allocated) > total {
                return Err(Error::Validation(format!(
                    "invoice {} has {} outstanding; cannot allocate {}",
                    inv.number.as_deref().unwrap_or("?"),
                    total - settled,
                    allocated
                )));
            }
        }

        let number = numbering.next(&txn, crate::scm::SALES_PAYMENT_SERIES).await?;
        let now = chrono::Utc::now();
        let amount_base = round_money(payment_row.amount * payment_row.exchange_rate);
        let request = gl::ar_payment_request(
            format!("sales.payment:{id}:post"),
            format!(
                "Customer payment {}",
                number.formatted
            ),
            payment_row.payment_date,
            amount_base,
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

    /// Reverse a posted payment: books the mirror entry (Dr AR / Cr
    /// Bank|Cash) so the invoices it settled fall back open.
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
            return Err(Error::Validation("only a posted payment can be reversed".into()));
        }
        let now = chrono::Utc::now();
        let amount_base = round_money(existing.amount * existing.exchange_rate);
        let request = gl::ar_payment_request(
            format!("sales.payment:{id}:reverse"),
            format!(
                "Reversal of customer payment {}",
                existing.number.as_deref().unwrap_or("?")
            ),
            now.date_naive(),
            amount_base,
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
        if let Some(customer_id) = filter.customer_id {
            query = query.filter(payment::Column::CustomerId.eq(customer_id));
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
        let customer_ids: Vec<Uuid> = rows.iter().map(|r| r.customer_id).collect();
        let customers: HashMap<Uuid, customer::Model> = customer::Entity::find()
            .filter(customer::Column::Id.is_in(customer_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|c| (c.id, c))
            .collect();
        rows.into_iter()
            .map(|r| {
                Ok(PaymentHeader {
                    id: r.id,
                    number: r.number.clone(),
                    customer_id: r.customer_id,
                    customer_name: customers
                        .get(&r.customer_id)
                        .map(|c| c.name.clone())
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

    pub async fn view(&self, id: Uuid) -> Result<PaymentView> {
        let row = load_payment(&self.db, id).await?;
        let allocations = load_allocations(&self.db, id).await?;
        let customer = customer::Entity::find_by_id(row.customer_id)
            .one(&self.db)
            .await?;
        let invoice_ids: Vec<Uuid> = allocations.iter().filter_map(|a| a.invoice_id).collect();
        let invoices: HashMap<Uuid, invoice::invoice::Model> = invoice::invoice::Entity::find()
            .filter(invoice::invoice::Column::Id.is_in(invoice_ids.clone()))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|i| (i.id, i))
            .collect();
        let mut alloc_views = Vec::with_capacity(allocations.len());
        for a in &allocations {
            let inv = a.invoice_id.and_then(|iid| invoices.get(&iid));
            let total = match a.invoice_id {
                Some(iid) if inv.is_some() => invoice::invoice_total(&self.db, iid).await?,
                _ => Decimal::ZERO,
            };
            alloc_views.push(PaymentAllocationView {
                id: a.id,
                invoice_id: a.invoice_id,
                invoice_number: inv.and_then(|i| i.number.clone()),
                invoice_total: total,
                amount: a.amount,
            });
        }
        let allocated: Decimal = allocations.iter().map(|a| a.amount).sum();

        Ok(PaymentView {
            id: row.id,
            number: row.number,
            customer_id: row.customer_id,
            customer_name: customer.map(|c| c.name).unwrap_or_default(),
            payment_date: row.payment_date,
            method: row.method,
            reference: row.reference,
            currency: row.currency,
            exchange_rate: row.exchange_rate,
            amount: row.amount,
            unallocated: round_money(row.amount - allocated),
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

/// Draft-time validation. Returns the resolved currency (from the request
/// or the customer). The row-locked re-check at post is authoritative.
async fn validate_payment<C: ConnectionTrait>(conn: &C, new: &NewPayment) -> Result<String> {
    let method = new.method.trim();
    if !METHODS.contains(&method) {
        return Err(Error::Validation(format!(
            "unknown payment method {method:?} (expected one of {})",
            METHODS.join(", ")
        )));
    }
    if new.amount <= Decimal::ZERO {
        return Err(Error::Validation("the payment amount must be positive".into()));
    }
    if new.exchange_rate.is_some_and(|r| r <= Decimal::ZERO) {
        return Err(Error::Validation("exchange rate must be positive".into()));
    }
    if new.allocations.is_empty() {
        return Err(Error::Validation(
            "a payment needs at least one allocation".into(),
        ));
    }
    let customer_row = customer::Entity::find_by_id(new.customer_id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("customer {}", new.customer_id)))?;
    let currency = match &new.currency {
        Some(c) if !c.trim().is_empty() => c.trim().to_uppercase(),
        _ => customer_row.currency.clone(),
    };

    let mut sum = Decimal::ZERO;
    let mut seen: HashSet<Uuid> = HashSet::new();
    let invoice_ids: Vec<Uuid> = new.allocations.iter().map(|a| a.invoice_id).collect();
    let already_settled = paid_amounts(conn, &invoice_ids).await?;
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
            .ok_or_else(|| Error::NotFound(format!("sales invoice {}", a.invoice_id)))?;
        if InvoiceStatus::parse(&inv.status)? != InvoiceStatus::Posted {
            return Err(Error::Validation(format!(
                "allocation {line_no}: invoice {} is not posted",
                inv.number.as_deref().unwrap_or("?")
            )));
        }
        if inv.customer_id != new.customer_id {
            return Err(Error::Validation(format!(
                "allocation {line_no}: invoice {} belongs to a different customer",
                inv.number.as_deref().unwrap_or("?")
            )));
        }
        if inv.currency != currency {
            return Err(Error::Validation(format!(
                "allocation {line_no}: invoice {} is in {} but the payment is in {}",
                inv.number.as_deref().unwrap_or("?"),
                inv.currency,
                currency
            )));
        }
        let total = invoice::invoice_total(conn, a.invoice_id).await?;
        let settled = already_settled.get(&a.invoice_id).copied().unwrap_or(Decimal::ZERO);
        if round_money(settled + a.amount) > total {
            return Err(Error::Validation(format!(
                "allocation {line_no}: invoice {} has only {} outstanding",
                inv.number.as_deref().unwrap_or("?"),
                total - settled
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
    Ok(currency)
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
            invoice_id: Set(Some(a.invoice_id)),
            credit_note_id: Set(None),
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
        .ok_or_else(|| Error::NotFound(format!("customer payment {id}")))
}

async fn load_payment_locked(txn: &DatabaseTransaction, id: Uuid) -> Result<payment::Model> {
    payment::Entity::find_by_id(id)
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("customer payment {id}")))
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
#[schema(as = SalesPaymentAllocationView)]
pub struct PaymentAllocationView {
    pub id: Uuid,
    pub invoice_id: Option<Uuid>,
    pub invoice_number: Option<String>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub invoice_total: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub amount: Decimal,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = SalesPaymentView)]
pub struct PaymentView {
    pub id: Uuid,
    pub number: Option<String>,
    pub customer_id: Uuid,
    pub customer_name: String,
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
    /// Amount received but not yet allocated — standing customer credit.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub unallocated: Decimal,
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

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = SalesPaymentHeader)]
pub struct PaymentHeader {
    pub id: Uuid,
    pub number: Option<String>,
    pub customer_id: Uuid,
    pub customer_name: String,
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
    pub customer_id: Option<Uuid>,
    pub status: Option<PaymentStatus>,
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = SalesPaymentAllocationRequest)]
pub struct PaymentAllocationRequest {
    pub invoice_id: Uuid,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub amount: Decimal,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = CreateSalesPaymentRequest)]
pub struct CreatePaymentRequest {
    pub customer_id: Uuid,
    #[schema(value_type = String, format = Date)]
    pub payment_date: chrono::NaiveDate,
    /// One of bank_transfer, cash, mobile_money, cheque, card.
    pub method: String,
    pub reference: Option<String>,
    /// Defaults to the customer's currency.
    pub currency: Option<String>,
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
#[schema(as = ReverseSalesPaymentRequest)]
pub struct ReversePaymentRequest {
    #[serde(default)]
    pub reason: String,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListPaymentsQuery {
    pub customer_id: Option<Uuid>,
    pub status: Option<PaymentStatus>,
    /// Payment date range, inclusive.
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

fn new_payment(req: CreatePaymentRequest, created_by: Option<Uuid>) -> NewPayment {
    NewPayment {
        customer_id: req.customer_id,
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
        .route("/sales/payments", get(list_payments).post(create_payment))
        .route(
            "/sales/payments/{id}",
            get(get_payment).put(update_payment).delete(delete_payment),
        )
        .route("/sales/payments/{id}/post", post(post_payment))
        .route("/sales/payments/{id}/reverse", post(reverse_payment))
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

#[utoipa::path(get, path = "/sales/payments", tag = "sales",
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
            customer_id: q.customer_id,
            status: q.status,
            from: q.from,
            to: q.to,
        })
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/sales/payments/{id}", tag = "sales",
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

#[utoipa::path(post, path = "/sales/payments", tag = "sales",
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
    audit.0.created("scm.sales_payment", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(put, path = "/sales/payments/{id}", tag = "sales",
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
    audit.0.updated("scm.sales_payment", id, &before, &after).await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/sales/payments/{id}", tag = "sales",
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
    audit.0.deleted("scm.sales_payment", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/sales/payments/{id}/post", tag = "sales",
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
            "posted customer payment {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/sales/payments/{id}/reverse", tag = "sales",
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
            "reversed customer payment {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}
