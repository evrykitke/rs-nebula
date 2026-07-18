//! POS orders: the lightweight sale document a till writes per ticket.
//!
//! Capture is one deliberately small transaction — validate, price,
//! number, insert — **no stock, no GL**; the session close consolidates
//! those (see [`super::session`]). `client_uuid` is the idempotency key
//! the offline queue replays on: a duplicate capture returns the order
//! already written, so a flaky network can never double-sell.
//!
//! Prices are tax-inclusive retail money (the shelf price is what the
//! customer pays); `tax_amount` is the VAT *inside* a line, extracted
//! from the item's sales tax code. The server reprices every line: an
//! online capture whose price disagrees is refused (the till's catalog is
//! stale), an offline-captured sale keeps the client's price — that
//! receipt already happened — and is flagged `price_drift` for the Z
//! report.
//!
//! Corrections are refunds referencing the original order line by line,
//! never edits: a captured order is immutable, and a void (PIN-gated,
//! only while its session is still open) marks, never deletes.

use crate::scm::inventory::item::{item, uom};
use crate::scm::inventory::stock::{self, round_money};
use crate::scm::pos::permissions::names;
use crate::scm::pos::register;
use crate::scm::pos::session::{SessionStatus, session as session_entity};
use crate::scm::sales::customer::customer;
use crate::scm::sales::invoice::tax_rates;
use crate::scm::sales::pricing::{PriceQuery, PriceSource, PricingService, price_list, price_list_item};
use axum::extract::{Path, Query};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use nebula::audit::Audit;
use nebula::auth::Authz;
use nebula::error::{Error, Result};
use nebula::{Numbering, TenantDb, sea_orm};
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DbBackend, QueryOrder, QuerySelect, Set, SqlErr,
    Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// What a POS order is: a sale, or a refund against one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = PosOrderKind)]
pub enum OrderKind {
    Sale,
    Refund,
}

impl OrderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OrderKind::Sale => "sale",
            OrderKind::Refund => "refund",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "sale" => Ok(OrderKind::Sale),
            "refund" => Ok(OrderKind::Refund),
            other => Err(Error::internal(format!("unknown pos order kind {other:?}"))),
        }
    }
}

/// A captured order is immutable; voiding marks, never deletes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = PosOrderStatus)]
pub enum OrderStatus {
    Captured,
    Voided,
}

impl OrderStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            OrderStatus::Captured => "captured",
            OrderStatus::Voided => "voided",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "captured" => Ok(OrderStatus::Captured),
            "voided" => Ok(OrderStatus::Voided),
            other => Err(Error::internal(format!(
                "unknown pos order status {other:?}"
            ))),
        }
    }
}

/// The tenders a till accepts in v1. Each maps to the seeded accounting
/// role its takings clear through at session close.
pub const TENDERS: &[&str] = &["cash", "mpesa", "card"];

/// The order header.
pub mod order {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "pos_orders")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub number: Option<String>,
        pub client_uuid: Uuid,
        pub session_id: Uuid,
        pub kind: String,
        pub customer_id: Uuid,
        pub sold_at: DateTimeUtc,
        pub currency: String,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))")]
        pub subtotal: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))")]
        pub discount_total: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))")]
        pub tax_total: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))")]
        pub total: Decimal,
        pub refund_of_id: Option<Uuid>,
        pub captured_offline: bool,
        pub price_drift: bool,
        pub status: String,
        pub voided_at: Option<DateTimeUtc>,
        pub voided_by: Option<Uuid>,
        pub void_reason: Option<String>,
        pub capture_seconds: Option<i32>,
        pub input_count: Option<i32>,
        pub created_at: DateTimeUtc,
        pub created_by: Option<Uuid>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// One sold (or refunded) line.
pub mod order_line {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "pos_order_lines")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub order_id: Uuid,
        pub line_no: i32,
        pub item_id: Uuid,
        pub description: String,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub qty: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub unit_price: Decimal,
        pub price_source: Option<String>,
        #[sea_orm(column_type = "Decimal(Some((7, 4)))", nullable)]
        pub discount_pct: Option<Decimal>,
        pub tax_code_id: Option<Uuid>,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))")]
        pub tax_amount: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))")]
        pub net: Decimal,
        pub batch_id: Option<Uuid>,
        pub refund_of_line_id: Option<Uuid>,
        pub created_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// One tender applied to an order (split payments are just several rows).
pub mod order_payment {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "pos_order_payments")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub order_id: Uuid,
        pub tender: String,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))")]
        pub amount: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))", nullable)]
        pub tendered: Option<Decimal>,
        pub reference: Option<String>,
        pub created_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

// ---------------------------------------------------------------------------
// The PIN gate
// ---------------------------------------------------------------------------

/// A supervisor's approval of a gated act: who, proven by their PIN.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct PinApproval {
    pub authorizer_id: Uuid,
    pub pin: String,
}

/// Verify a PIN approval: the authorizer must be an active user of the
/// same tenant, hold the override permission, have a PIN set, and the PIN
/// must verify against its hash. Every failure is the same [`Error::
/// Forbidden`] — nothing leaks about which part failed. Returns the
/// authorizer's id for the audit trail.
pub(crate) async fn verify_override_pin(
    authz: &Authz,
    db: &DatabaseConnection,
    approval: &PinApproval,
) -> Result<Uuid> {
    use nebula::auth::user;
    let authorizer = user::Entity::find_by_id(approval.authorizer_id)
        .one(db)
        .await?
        .filter(|u| u.is_active && u.deleted_at.is_none() && u.tenant_id == authz.user.tenant_id)
        .ok_or(Error::Forbidden)?;
    let Some(hash) = &authorizer.override_pin_hash else {
        return Err(Error::Forbidden);
    };
    if !nebula::auth::password::verify(&approval.pin, hash) {
        return Err(Error::Forbidden);
    }
    if !authz.roles().is_granted(authorizer.id, names::OVERRIDE).await? {
        return Err(Error::Forbidden);
    }
    Ok(authorizer.id)
}

/// Resolve whether a gated act may proceed: the caller's own override
/// permission suffices, otherwise a PIN approval must verify. Returns the
/// approving identity when a PIN was used.
async fn resolve_override(
    authz: &Authz,
    db: &DatabaseConnection,
    approval: Option<&PinApproval>,
) -> Result<(bool, Option<Uuid>)> {
    if authz.is_granted(names::OVERRIDE).await? {
        return Ok((true, None));
    }
    match approval {
        Some(a) => {
            let id = verify_override_pin(authz, db, a).await?;
            Ok((true, Some(id)))
        }
        None => Ok((false, None)),
    }
}

// ---------------------------------------------------------------------------
// Service inputs
// ---------------------------------------------------------------------------

pub struct SaleLineInput {
    pub item_id: Uuid,
    pub qty: Decimal,
    /// The tax-inclusive unit price the till showed and charged.
    pub unit_price: Decimal,
    /// The cashier keyed the price by hand (an override-gated act).
    pub manual_price: bool,
    pub discount_pct: Option<Decimal>,
    pub batch_id: Option<Uuid>,
}

pub struct TenderInput {
    pub tender: String,
    /// Amount applied to the sale.
    pub amount: Decimal,
    /// Cash physically handed over; change = tendered − amount.
    pub tendered: Option<Decimal>,
    /// M-Pesa confirmation code / card slip number.
    pub reference: Option<String>,
}

pub struct NewSale {
    pub client_uuid: Uuid,
    pub session_id: Uuid,
    /// `None` = the register's default customer (usually the walk-in).
    pub customer_id: Option<Uuid>,
    pub sold_at: chrono::DateTime<chrono::Utc>,
    pub captured_offline: bool,
    pub lines: Vec<SaleLineInput>,
    pub tenders: Vec<TenderInput>,
    /// Whether override-gated content (manual prices, discounts) is
    /// allowed — resolved by the handler from the caller's permission or
    /// a verified PIN approval.
    pub allow_override: bool,
    /// Till-measured: first line to payment, in seconds.
    pub capture_seconds: Option<i32>,
    /// Till-measured: taps/keys/scans the sale cost.
    pub input_count: Option<i32>,
    pub created_by: Option<Uuid>,
}

pub struct RefundLineInput {
    /// The original order line being refunded.
    pub line_id: Uuid,
    pub qty: Decimal,
}

pub struct NewRefund {
    pub client_uuid: Uuid,
    /// The open session the refund is captured in (money leaves this
    /// drawer) — not necessarily the session that sold.
    pub session_id: Uuid,
    pub original_id: Uuid,
    pub lines: Vec<RefundLineInput>,
    /// The tender the money goes back by (original tender by default —
    /// the client passes it explicitly).
    pub tender: String,
    pub reference: Option<String>,
    pub created_by: Option<Uuid>,
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

pub struct SaleService {
    db: DatabaseConnection,
}

impl SaleService {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Capture one sale: validate the session and items, reprice on the
    /// server, check the tenders sum, allocate the RCP number and write
    /// order + lines + payments in one small transaction. Idempotent on
    /// `client_uuid` — a replay returns the order already captured.
    pub async fn capture(&self, new: NewSale, numbering: &Numbering) -> Result<OrderView> {
        if let Some(existing) = self.find_by_client_uuid(new.client_uuid).await? {
            return self.view(existing.id).await;
        }

        let session_row = load_session(&self.db, new.session_id).await?;
        if SessionStatus::parse(&session_row.status)? != SessionStatus::Open {
            return Err(Error::Validation(
                "the session is not open; open a session before selling".into(),
            ));
        }
        let register_row = register::Entity::find_by_id(session_row.register_id)
            .one(&self.db)
            .await?
            .ok_or_else(|| Error::internal("session without a register"))?;
        let buyer = resolve_customer(
            &self.db,
            new.customer_id.or(register_row.default_customer_id),
        )
        .await?;

        if new.lines.is_empty() {
            return Err(Error::Validation("a sale needs at least one line".into()));
        }
        if new.tenders.is_empty() {
            return Err(Error::Validation("a sale needs at least one tender".into()));
        }
        if new.capture_seconds.is_some_and(|s| s < 0) || new.input_count.is_some_and(|n| n < 0) {
            return Err(Error::Validation(
                "instrumentation counters must not be negative".into(),
            ));
        }

        // Price and tax every line server-side.
        let pricing = PricingService::new(self.db.clone());
        let sold_on = new.sold_at.date_naive();
        let mut priced: Vec<PricedLine> = Vec::with_capacity(new.lines.len());
        for (i, line) in new.lines.iter().enumerate() {
            let line_no = (i + 1) as i32;
            let item_row = load_sellable_item(&self.db, line.item_id, line_no).await?;
            validate_qty(&self.db, &item_row, line.qty, line_no).await?;
            validate_batch(&self.db, &item_row, line.batch_id, line_no).await?;
            if line.discount_pct.is_some_and(|p| !p.is_zero()) && !new.allow_override {
                return Err(Error::Validation(format!(
                    "line {line_no}: discounts need the override permission or a supervisor PIN"
                )));
            }
            let pct_ok = line
                .discount_pct
                .is_none_or(|p| p >= Decimal::ZERO && p <= Decimal::ONE_HUNDRED);
            if !pct_ok {
                return Err(Error::Validation(format!(
                    "line {line_no}: discount must be between 0 and 100 percent"
                )));
            }

            let (unit_price, source, drift) = self
                .price_line(
                    &pricing,
                    &register_row,
                    &buyer,
                    &item_row,
                    line,
                    sold_on,
                    new.captured_offline,
                    new.allow_override,
                    line_no,
                )
                .await?;

            priced.push(PricedLine {
                item: item_row,
                qty: line.qty,
                unit_price,
                price_source: source,
                discount_pct: line.discount_pct.filter(|p| !p.is_zero()),
                batch_id: line.batch_id,
                drift,
                refund_of_line_id: None,
                tax_code_id: None,
                computed_gross: Decimal::ZERO,
                computed_tax: Decimal::ZERO,
            });
        }

        let totals = compute_totals(&self.db, &mut priced, buyer.tax_exempt).await?;
        let settings = super::settings::load(&self.db).await?;
        validate_tenders(&new.tenders, totals.total, settings.require_mpesa_reference)?;

        let price_drift = priced.iter().any(|l| l.drift);
        self.insert_order(InsertOrder {
            client_uuid: new.client_uuid,
            session_id: new.session_id,
            kind: OrderKind::Sale,
            customer_id: buyer.id,
            sold_at: new.sold_at,
            currency: buyer.currency.clone(),
            totals,
            refund_of_id: None,
            captured_offline: new.captured_offline,
            price_drift,
            lines: priced,
            tenders: new.tenders,
            capture_seconds: new.capture_seconds,
            input_count: new.input_count,
            created_by: new.created_by,
        }, numbering)
        .await
    }

    /// Refund part or all of a captured order, at the original prices.
    /// Cannot exceed the un-refunded remainder per line; captured in the
    /// caller's open session, so the money leaves the right drawer.
    pub async fn refund(&self, new: NewRefund, numbering: &Numbering) -> Result<OrderView> {
        if let Some(existing) = self.find_by_client_uuid(new.client_uuid).await? {
            return self.view(existing.id).await;
        }

        let session_row = load_session(&self.db, new.session_id).await?;
        if SessionStatus::parse(&session_row.status)? != SessionStatus::Open {
            return Err(Error::Validation(
                "the session is not open; open a session before refunding".into(),
            ));
        }
        if !TENDERS.contains(&new.tender.as_str()) {
            return Err(Error::Validation(format!(
                "unknown tender {:?} (expected one of {})",
                new.tender,
                TENDERS.join(", ")
            )));
        }
        let original = load_order(&self.db, new.original_id).await?;
        if OrderKind::parse(&original.kind)? != OrderKind::Sale {
            return Err(Error::Validation("only a sale can be refunded".into()));
        }
        if OrderStatus::parse(&original.status)? != OrderStatus::Captured {
            return Err(Error::Validation("a voided sale cannot be refunded".into()));
        }
        if new.lines.is_empty() {
            return Err(Error::Validation("a refund needs at least one line".into()));
        }

        let original_lines: HashMap<Uuid, order_line::Model> =
            load_lines(&self.db, original.id)
                .await?
                .into_iter()
                .map(|l| (l.id, l))
                .collect();
        let already = refunded_quantities(&self.db, original.id).await?;

        // Per-line: positive, within the un-refunded remainder, priced at
        // the original line's effective money pro rata.
        let mut priced: Vec<PricedLine> = Vec::new();
        let mut refunding: HashMap<Uuid, Decimal> = HashMap::new();
        for (i, r) in new.lines.iter().enumerate() {
            let line_no = i + 1;
            let ol = original_lines.get(&r.line_id).ok_or_else(|| {
                Error::Validation(format!(
                    "refund line {line_no} does not belong to the original sale"
                ))
            })?;
            if r.qty <= Decimal::ZERO {
                return Err(Error::Validation(format!(
                    "refund line {line_no}: quantity must be positive"
                )));
            }
            let taken = refunding.entry(ol.id).or_default();
            *taken += r.qty;
            let prior = already.get(&ol.id).copied().unwrap_or(Decimal::ZERO);
            if prior + *taken > ol.qty {
                return Err(Error::Validation(format!(
                    "refund line {line_no}: only {} of {} left to refund",
                    (ol.qty - prior).normalize(),
                    ol.description
                )));
            }
            let item_row = item::Entity::find_by_id(ol.item_id)
                .one(&self.db)
                .await?
                .ok_or_else(|| Error::internal("order line without an item"))?;
            priced.push(PricedLine {
                item: item_row,
                qty: r.qty,
                unit_price: ol.unit_price,
                price_source: ol.price_source.clone(),
                discount_pct: ol.discount_pct,
                batch_id: ol.batch_id,
                drift: false,
                refund_of_line_id: Some(ol.id),
                tax_code_id: ol.tax_code_id,
                computed_gross: Decimal::ZERO,
                computed_tax: Decimal::ZERO,
            });
        }

        // Refund money is the original's pro rata share, not a reprice:
        // the receipt is the contract.
        let mut totals = Totals::default();
        for l in &mut priced {
            let ol = &original_lines[&l.refund_of_line_id.unwrap()];
            let share = |v: Decimal| {
                if ol.qty.is_zero() {
                    Decimal::ZERO
                } else {
                    round_money(v * l.qty / ol.qty)
                }
            };
            l.computed_gross = share(ol.net);
            l.computed_tax = share(ol.tax_amount);
            l.tax_code_id = ol.tax_code_id;
            totals.subtotal += round_money(l.qty * l.unit_price);
            totals.tax += l.computed_tax;
            totals.total += l.computed_gross;
        }
        totals.discount = totals.subtotal - totals.total;

        let tender = TenderInput {
            tender: new.tender,
            amount: totals.total,
            tendered: None,
            reference: new.reference,
        };
        self.insert_order(InsertOrder {
            client_uuid: new.client_uuid,
            session_id: new.session_id,
            kind: OrderKind::Refund,
            customer_id: original.customer_id,
            sold_at: chrono::Utc::now(),
            currency: original.currency.clone(),
            totals,
            refund_of_id: Some(original.id),
            captured_offline: false,
            price_drift: false,
            lines: priced,
            tenders: vec![tender],
            // Refunds are supervised, deliberate acts; speed is not a
            // metric anyone tunes on them.
            capture_seconds: None,
            input_count: None,
            created_by: new.created_by,
        }, numbering)
        .await
    }

    /// Void a captured order: only while its session is still open, and
    /// only when no refund references it. Marks, never deletes.
    pub async fn void(
        &self,
        id: Uuid,
        reason: &str,
        by: Option<Uuid>,
    ) -> Result<OrderView> {
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(Error::Validation("a void needs a reason".into()));
        }
        let txn = self.db.begin().await?;
        let existing = order::Entity::find_by_id(id)
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or_else(|| Error::NotFound(format!("pos order {id}")))?;
        if OrderStatus::parse(&existing.status)? != OrderStatus::Captured {
            return Err(Error::Validation("the order is already voided".into()));
        }
        let session_row = load_session(&txn, existing.session_id).await?;
        if SessionStatus::parse(&session_row.status)? != SessionStatus::Open {
            return Err(Error::Validation(
                "the session has closed; correct with a refund instead of a void".into(),
            ));
        }
        let refunds = order::Entity::find()
            .filter(order::Column::RefundOfId.eq(id))
            .filter(order::Column::Status.eq(OrderStatus::Captured.as_str()))
            .count(&txn)
            .await?;
        if refunds > 0 {
            return Err(Error::Validation(
                "the order has refunds against it and can no longer be voided".into(),
            ));
        }
        let now = chrono::Utc::now();
        let mut active: order::ActiveModel = existing.into();
        active.status = Set(OrderStatus::Voided.as_str().to_string());
        active.voided_at = Set(Some(now));
        active.voided_by = Set(by);
        active.void_reason = Set(Some(reason.to_string()));
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    pub async fn list(&self, filter: OrderFilter) -> Result<Vec<OrderHeader>> {
        let mut query = order::Entity::find();
        if let Some(session_id) = filter.session_id {
            query = query.filter(order::Column::SessionId.eq(session_id));
        }
        if let Some(kind) = filter.kind {
            query = query.filter(order::Column::Kind.eq(kind.as_str()));
        }
        let rows = query
            .order_by_desc(order::Column::SoldAt)
            .all(&self.db)
            .await?;
        rows.into_iter()
            .map(|r| {
                Ok(OrderHeader {
                    id: r.id,
                    number: r.number.clone(),
                    client_uuid: r.client_uuid,
                    session_id: r.session_id,
                    kind: OrderKind::parse(&r.kind)?,
                    status: OrderStatus::parse(&r.status)?,
                    sold_at: r.sold_at,
                    currency: r.currency.clone(),
                    total: r.total,
                    price_drift: r.price_drift,
                    captured_offline: r.captured_offline,
                })
            })
            .collect()
    }

    pub async fn view(&self, id: Uuid) -> Result<OrderView> {
        let row = load_order(&self.db, id).await?;
        let lines = load_lines(&self.db, id).await?;
        let payments = order_payment::Entity::find()
            .filter(order_payment::Column::OrderId.eq(id))
            .order_by_asc(order_payment::Column::CreatedAt)
            .all(&self.db)
            .await?;
        let buyer = customer::Entity::find_by_id(row.customer_id)
            .one(&self.db)
            .await?;
        let change = payments
            .iter()
            .filter_map(|p| p.tendered.map(|t| t - p.amount))
            .fold(Decimal::ZERO, |a, b| a + b);

        Ok(OrderView {
            id: row.id,
            number: row.number,
            client_uuid: row.client_uuid,
            session_id: row.session_id,
            kind: OrderKind::parse(&row.kind)?,
            status: OrderStatus::parse(&row.status)?,
            customer_id: row.customer_id,
            customer_name: buyer.map(|c| c.name).unwrap_or_default(),
            sold_at: row.sold_at,
            currency: row.currency,
            subtotal: row.subtotal,
            discount_total: row.discount_total,
            tax_total: row.tax_total,
            total: row.total,
            change: round_money(change),
            refund_of_id: row.refund_of_id,
            captured_offline: row.captured_offline,
            price_drift: row.price_drift,
            void_reason: row.void_reason,
            lines: lines
                .into_iter()
                .map(|l| OrderLineView {
                    id: l.id,
                    line_no: l.line_no,
                    item_id: l.item_id,
                    description: l.description,
                    qty: l.qty,
                    unit_price: l.unit_price,
                    price_source: l.price_source,
                    discount_pct: l.discount_pct,
                    tax_amount: l.tax_amount,
                    net: l.net,
                    batch_id: l.batch_id,
                    refund_of_line_id: l.refund_of_line_id,
                })
                .collect(),
            payments: payments
                .into_iter()
                .map(|p| OrderPaymentView {
                    tender: p.tender,
                    amount: p.amount,
                    tendered: p.tendered,
                    reference: p.reference,
                })
                .collect(),
        })
    }

    async fn find_by_client_uuid(&self, client_uuid: Uuid) -> Result<Option<order::Model>> {
        order::Entity::find()
            .filter(order::Column::ClientUuid.eq(client_uuid))
            .one(&self.db)
            .await
            .map_err(Error::from)
    }

    /// The price of one sale line: the register's own price list first,
    /// then the customer resolution chain; manual prices go through the
    /// pricing service's floor check. Offline mismatches keep the client
    /// price and flag drift; online mismatches are refused.
    #[allow(clippy::too_many_arguments)]
    async fn price_line(
        &self,
        pricing: &PricingService,
        register_row: &register::Model,
        buyer: &customer::Model,
        item_row: &item::Model,
        line: &SaleLineInput,
        sold_on: chrono::NaiveDate,
        captured_offline: bool,
        allow_override: bool,
        line_no: i32,
    ) -> Result<(Decimal, Option<String>, bool)> {
        if line.manual_price {
            let (price, source) = pricing
                .price_line(
                    PriceQuery {
                        customer_id: buyer.id,
                        item_id: item_row.id,
                        qty: line.qty,
                        uom_id: None,
                        currency: buyer.currency.clone(),
                        date: sold_on,
                    },
                    Some(line.unit_price),
                    allow_override,
                )
                .await?;
            return Ok((price, Some(source.as_string()), false));
        }

        let (server_price, source) = match self
            .register_list_price(register_row, item_row, line.qty, &buyer.currency, sold_on)
            .await?
        {
            Some((price, list_id)) => (price, PriceSource::List(list_id)),
            None => {
                pricing
                    .price_line(
                        PriceQuery {
                            customer_id: buyer.id,
                            item_id: item_row.id,
                            qty: line.qty,
                            uom_id: None,
                            currency: buyer.currency.clone(),
                            date: sold_on,
                        },
                        None,
                        false,
                    )
                    .await?
            }
        };
        if server_price == line.unit_price {
            Ok((server_price, Some(source.as_string()), false))
        } else if captured_offline {
            // The receipt already happened at the till's cached price;
            // keep it and let the Z report show the drift.
            Ok((line.unit_price, Some(source.as_string()), true))
        } else {
            Err(Error::Validation(format!(
                "line {line_no}: the price of {} is now {server_price}; refresh the catalog",
                item_row.sku
            )))
        }
    }

    /// The register price-list override: an active list in the right
    /// currency whose window covers the date, quantity breaks honoured.
    async fn register_list_price(
        &self,
        register_row: &register::Model,
        item_row: &item::Model,
        qty: Decimal,
        currency: &str,
        date: chrono::NaiveDate,
    ) -> Result<Option<(Decimal, Uuid)>> {
        let Some(list_id) = register_row.price_list_id else {
            return Ok(None);
        };
        let Some(list) = price_list::Entity::find_by_id(list_id).one(&self.db).await? else {
            return Ok(None);
        };
        let live = list.status == "active"
            && list.currency == currency
            && list.valid_from.is_none_or(|f| f <= date)
            && list.valid_to.is_none_or(|t| t >= date);
        if !live {
            return Ok(None);
        }
        let lines = price_list_item::Entity::find()
            .filter(price_list_item::Column::PriceListId.eq(list_id))
            .filter(price_list_item::Column::ItemId.eq(item_row.id))
            .filter(price_list_item::Column::UomId.is_null())
            .filter(price_list_item::Column::MinQty.lte(qty))
            .order_by_desc(price_list_item::Column::MinQty)
            .all(&self.db)
            .await?;
        let Some(best) = lines.first() else {
            return Ok(None);
        };
        let price = match (best.unit_price, best.discount_pct) {
            (Some(p), _) => p,
            (None, Some(pct)) => {
                let base = item_row.selling_price.unwrap_or(Decimal::ZERO);
                round_money(base * (Decimal::ONE_HUNDRED - pct) / Decimal::ONE_HUNDRED)
            }
            (None, None) => return Ok(None),
        };
        Ok(Some((price, list_id)))
    }

    /// Write order + lines + payments and allocate the RCP number, one
    /// transaction. A `client_uuid` race (two replays of the same sale
    /// landing together) resolves to the row that won.
    async fn insert_order(
        &self,
        ins: InsertOrder,
        numbering: &Numbering,
    ) -> Result<OrderView> {
        let txn = self.db.begin().await?;
        let order_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        let number = numbering.next(&txn, crate::scm::POS_RECEIPT_SERIES).await?;
        let inserted = order::ActiveModel {
            id: Set(order_id),
            number: Set(Some(number.formatted)),
            client_uuid: Set(ins.client_uuid),
            session_id: Set(ins.session_id),
            kind: Set(ins.kind.as_str().to_string()),
            customer_id: Set(ins.customer_id),
            sold_at: Set(ins.sold_at),
            currency: Set(ins.currency),
            subtotal: Set(round_money(ins.totals.subtotal)),
            discount_total: Set(round_money(ins.totals.discount)),
            tax_total: Set(round_money(ins.totals.tax)),
            total: Set(round_money(ins.totals.total)),
            refund_of_id: Set(ins.refund_of_id),
            captured_offline: Set(ins.captured_offline),
            price_drift: Set(ins.price_drift),
            status: Set(OrderStatus::Captured.as_str().to_string()),
            voided_at: Set(None),
            voided_by: Set(None),
            void_reason: Set(None),
            capture_seconds: Set(ins.capture_seconds),
            input_count: Set(ins.input_count),
            created_at: Set(now),
            created_by: Set(ins.created_by),
        }
        .insert(&txn)
        .await;
        let _inserted = match inserted {
            Ok(row) => row,
            Err(e) => {
                if matches!(e.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) {
                    // Another replay of this very sale won the race.
                    drop(txn);
                    if let Some(existing) = self.find_by_client_uuid(ins.client_uuid).await? {
                        return self.view(existing.id).await;
                    }
                }
                return Err(Error::from(e));
            }
        };
        for (i, l) in ins.lines.iter().enumerate() {
            order_line::ActiveModel {
                id: Set(Uuid::new_v4()),
                order_id: Set(order_id),
                line_no: Set((i + 1) as i32),
                item_id: Set(l.item.id),
                description: Set(l.item.name.clone()),
                qty: Set(l.qty),
                unit_price: Set(l.unit_price),
                price_source: Set(l.price_source.clone()),
                discount_pct: Set(l.discount_pct),
                tax_code_id: Set(l.tax_code_id),
                tax_amount: Set(l.computed_tax),
                net: Set(l.computed_gross),
                batch_id: Set(l.batch_id),
                refund_of_line_id: Set(l.refund_of_line_id),
                created_at: Set(now),
            }
            .insert(&txn)
            .await?;
        }
        for t in &ins.tenders {
            order_payment::ActiveModel {
                id: Set(Uuid::new_v4()),
                order_id: Set(order_id),
                tender: Set(t.tender.clone()),
                amount: Set(round_money(t.amount)),
                tendered: Set(t.tendered.map(round_money)),
                reference: Set(t.reference.clone().filter(|r| !r.trim().is_empty())),
                created_at: Set(now),
            }
            .insert(&txn)
            .await?;
        }
        txn.commit().await?;
        self.view(order_id).await
    }
}

/// A line after server pricing, ready to persist.
struct PricedLine {
    item: item::Model,
    qty: Decimal,
    unit_price: Decimal,
    price_source: Option<String>,
    discount_pct: Option<Decimal>,
    batch_id: Option<Uuid>,
    drift: bool,
    refund_of_line_id: Option<Uuid>,
    // Filled by the totals computation:
    tax_code_id: Option<Uuid>,
    computed_gross: Decimal,
    computed_tax: Decimal,
}

impl PricedLine {
    fn effective_gross(&self) -> Decimal {
        let pct = self.discount_pct.unwrap_or(Decimal::ZERO);
        round_money(self.qty * self.unit_price * (Decimal::ONE_HUNDRED - pct) / Decimal::ONE_HUNDRED)
    }
}

#[derive(Default, Clone, Copy)]
struct Totals {
    subtotal: Decimal,
    discount: Decimal,
    tax: Decimal,
    total: Decimal,
}

struct InsertOrder {
    client_uuid: Uuid,
    session_id: Uuid,
    kind: OrderKind,
    customer_id: Uuid,
    sold_at: chrono::DateTime<chrono::Utc>,
    currency: String,
    totals: Totals,
    refund_of_id: Option<Uuid>,
    captured_offline: bool,
    price_drift: bool,
    lines: Vec<PricedLine>,
    tenders: Vec<TenderInput>,
    capture_seconds: Option<i32>,
    input_count: Option<i32>,
    created_by: Option<Uuid>,
}

/// Fill each line's gross and the VAT inside it, and the order totals.
/// Prices are tax-inclusive, so the tax is extracted, never added:
/// `tax = gross × rate ⁄ (100 + rate)`.
async fn compute_totals(
    db: &DatabaseConnection,
    lines: &mut [PricedLine],
    tax_exempt: bool,
) -> Result<Totals> {
    let code_ids: Vec<Uuid> = lines
        .iter()
        .filter_map(|l| l.item.sales_tax_code_id)
        .collect();
    let rates = tax_rates(db, &code_ids).await?;
    let mut totals = Totals::default();
    for l in lines.iter_mut() {
        let gross = l.effective_gross();
        let rate = if tax_exempt {
            Decimal::ZERO
        } else {
            l.item
                .sales_tax_code_id
                .and_then(|id| rates.get(&id).copied())
                .unwrap_or(Decimal::ZERO)
        };
        l.tax_code_id = l.item.sales_tax_code_id.filter(|_| !tax_exempt);
        l.computed_gross = gross;
        l.computed_tax = round_money(gross * rate / (Decimal::ONE_HUNDRED + rate));
        totals.subtotal += round_money(l.qty * l.unit_price);
        totals.tax += l.computed_tax;
        totals.total += gross;
    }
    totals.discount = totals.subtotal - totals.total;
    Ok(totals)
}

/// Tenders: known kinds, positive amounts summing exactly to the total;
/// cash change never exceeds what was handed over, and M-Pesa needs its
/// confirmation code when the tenant's settings say so (the manual-confirm
/// path of v1 — a tenant may trade the code for queue speed).
fn validate_tenders(tenders: &[TenderInput], total: Decimal, mpesa_needs_code: bool) -> Result<()> {
    let mut sum = Decimal::ZERO;
    for (i, t) in tenders.iter().enumerate() {
        let n = i + 1;
        if !TENDERS.contains(&t.tender.as_str()) {
            return Err(Error::Validation(format!(
                "tender {n}: unknown tender {:?} (expected one of {})",
                t.tender,
                TENDERS.join(", ")
            )));
        }
        if t.amount <= Decimal::ZERO {
            return Err(Error::Validation(format!(
                "tender {n}: amount must be positive"
            )));
        }
        match t.tender.as_str() {
            "cash" => {
                if t.tendered.is_some_and(|given| given < t.amount) {
                    return Err(Error::Validation(format!(
                        "tender {n}: cash handed over is less than the amount applied"
                    )));
                }
            }
            other => {
                if t.tendered.is_some() {
                    return Err(Error::Validation(format!(
                        "tender {n}: change only exists for cash"
                    )));
                }
                if other == "mpesa"
                    && mpesa_needs_code
                    && t.reference.as_deref().is_none_or(|r| r.trim().is_empty())
                {
                    return Err(Error::Validation(format!(
                        "tender {n}: an M-Pesa payment needs its confirmation code"
                    )));
                }
            }
        }
        sum += t.amount;
    }
    if round_money(sum) != round_money(total) {
        return Err(Error::Validation(format!(
            "tenders sum to {} but the sale totals {}",
            round_money(sum),
            round_money(total)
        )));
    }
    Ok(())
}

/// Quantities already refunded per original line (captured refunds only).
pub(crate) async fn refunded_quantities<C: ConnectionTrait>(
    conn: &C,
    original_id: Uuid,
) -> Result<HashMap<Uuid, Decimal>> {
    let refund_ids: Vec<Uuid> = order::Entity::find()
        .filter(order::Column::RefundOfId.eq(original_id))
        .filter(order::Column::Status.eq(OrderStatus::Captured.as_str()))
        .all(conn)
        .await?
        .into_iter()
        .map(|o| o.id)
        .collect();
    let mut map: HashMap<Uuid, Decimal> = HashMap::new();
    if refund_ids.is_empty() {
        return Ok(map);
    }
    let lines = order_line::Entity::find()
        .filter(order_line::Column::OrderId.is_in(refund_ids))
        .all(conn)
        .await?;
    for l in lines {
        if let Some(orig) = l.refund_of_line_id {
            *map.entry(orig).or_default() += l.qty;
        }
    }
    Ok(map)
}

async fn resolve_customer(
    db: &DatabaseConnection,
    customer_id: Option<Uuid>,
) -> Result<customer::Model> {
    match customer_id {
        Some(id) => customer::Entity::find_by_id(id)
            .one(db)
            .await?
            .filter(|c| c.is_active)
            .ok_or_else(|| Error::NotFound(format!("customer {id}"))),
        None => customer::Entity::find()
            .filter(customer::Column::Code.eq(crate::scm::seed::WALK_IN_CODE))
            .one(db)
            .await?
            .ok_or_else(|| {
                Error::internal("the walk-in customer is missing; reseed the tenant")
            }),
    }
}

async fn load_session<C: ConnectionTrait>(
    conn: &C,
    id: Uuid,
) -> Result<session_entity::Model> {
    session_entity::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("pos session {id}")))
}

async fn load_order<C: ConnectionTrait>(conn: &C, id: Uuid) -> Result<order::Model> {
    order::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("pos order {id}")))
}

pub(crate) async fn load_lines<C: ConnectionTrait>(
    conn: &C,
    order_id: Uuid,
) -> Result<Vec<order_line::Model>> {
    order_line::Entity::find()
        .filter(order_line::Column::OrderId.eq(order_id))
        .order_by_asc(order_line::Column::LineNo)
        .all(conn)
        .await
        .map_err(Error::from)
}

async fn load_sellable_item(
    db: &DatabaseConnection,
    item_id: Uuid,
    line_no: i32,
) -> Result<item::Model> {
    let item_row = item::Entity::find_by_id(item_id)
        .one(db)
        .await?
        .ok_or_else(|| Error::NotFound(format!("item {item_id}")))?;
    if !item_row.is_active || !item_row.is_sellable {
        return Err(Error::Validation(format!(
            "line {line_no}: item {} is not sellable",
            item_row.sku
        )));
    }
    if item_row.track_serials {
        return Err(Error::Validation(format!(
            "line {line_no}: item {} tracks serial numbers and cannot be sold at the till",
            item_row.sku
        )));
    }
    Ok(item_row)
}

async fn validate_qty(
    db: &DatabaseConnection,
    item_row: &item::Model,
    qty: Decimal,
    line_no: i32,
) -> Result<()> {
    if qty <= Decimal::ZERO {
        return Err(Error::Validation(format!(
            "line {line_no}: quantity must be positive"
        )));
    }
    let stock_uom = uom::Entity::find_by_id(item_row.uom_id)
        .one(db)
        .await?
        .ok_or_else(|| Error::internal(format!("stock uom missing for {}", item_row.sku)))?;
    if !stock_uom.fractional && qty.normalize().scale() > 0 {
        return Err(Error::Validation(format!(
            "line {line_no}: {} sells in whole {}",
            item_row.sku, stock_uom.code
        )));
    }
    Ok(())
}

async fn validate_batch(
    db: &DatabaseConnection,
    item_row: &item::Model,
    batch_id: Option<Uuid>,
    line_no: i32,
) -> Result<()> {
    use crate::scm::inventory::batch::batch;
    match (batch_id, item_row.track_batches) {
        (Some(id), true) => {
            let b = batch::Entity::find_by_id(id)
                .one(db)
                .await?
                .filter(|b| b.item_id == item_row.id)
                .ok_or_else(|| {
                    Error::Validation(format!(
                        "line {line_no}: the batch does not belong to {}",
                        item_row.sku
                    ))
                })?;
            if !b.is_active {
                return Err(Error::Validation(format!(
                    "line {line_no}: batch {} of {} is not active",
                    b.batch_no, item_row.sku
                )));
            }
            Ok(())
        }
        (None, true) => Err(Error::Validation(format!(
            "line {line_no}: item {} tracks batches; pick the lot being sold",
            item_row.sku
        ))),
        (Some(_), false) => Err(Error::Validation(format!(
            "line {line_no}: item {} does not track batches",
            item_row.sku
        ))),
        (None, false) => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Catalog feed
// ---------------------------------------------------------------------------

/// One sellable item as the till caches it: identity, the resolved price
/// for the register's default buyer, the VAT inside it, and the stock
/// dimensions the till needs (batches for FEFO suggestion, on-hand for
/// the tile badge).
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CatalogItem {
    pub id: Uuid,
    pub sku: String,
    pub name: String,
    pub barcode: Option<String>,
    pub category_id: Option<Uuid>,
    pub image_file_id: Option<Uuid>,
    pub uom_code: String,
    /// Whole units only when false.
    pub uom_fractional: bool,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub price: Decimal,
    pub price_source: Option<String>,
    pub tax_code_id: Option<Uuid>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub tax_rate: Decimal,
    pub track_batches: bool,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub on_hand: Decimal,
    pub batches: Vec<CatalogBatch>,
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// A lot with stock in the register's warehouse, expiry-sorted so the
/// till suggests first-expired-first-out.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CatalogBatch {
    pub id: Uuid,
    pub batch_no: String,
    #[schema(value_type = Option<String>, format = Date)]
    pub expires_on: Option<chrono::NaiveDate>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub on_hand: Decimal,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CatalogView {
    /// Pass back as `since` on the next delta fetch.
    #[schema(value_type = String, format = DateTime)]
    pub generated_at: chrono::DateTime<chrono::Utc>,
    pub currency: String,
    /// Full when `since` was absent, else only items updated since.
    pub items: Vec<CatalogItem>,
}

/// Build the catalog for one register: sellable active items (serial-
/// tracked ones excluded — the till cannot sell them), priced for the
/// register's default buyer, with per-batch and total on-hand in the
/// register's warehouse. `since` narrows to items updated after it.
pub async fn catalog(
    db: &DatabaseConnection,
    register_id: Uuid,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<CatalogView> {
    let register_row = register::Entity::find_by_id(register_id)
        .one(db)
        .await?
        .ok_or_else(|| Error::NotFound(format!("register {register_id}")))?;
    let buyer = resolve_customer(db, register_row.default_customer_id).await?;
    let generated_at = chrono::Utc::now();
    let today = generated_at.date_naive();

    let mut query = item::Entity::find()
        .filter(item::Column::IsActive.eq(true))
        .filter(item::Column::IsSellable.eq(true))
        .filter(item::Column::TrackSerials.eq(false));
    if let Some(since) = since {
        query = query.filter(item::Column::UpdatedAt.gt(since));
    }
    let items = query.order_by_asc(item::Column::Sku).all(db).await?;
    if items.is_empty() {
        return Ok(CatalogView {
            generated_at,
            currency: buyer.currency,
            items: Vec::new(),
        });
    }

    let uoms: HashMap<Uuid, uom::Model> = uom::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|u| (u.id, u))
        .collect();
    let code_ids: Vec<Uuid> = items.iter().filter_map(|i| i.sales_tax_code_id).collect();
    let rates = tax_rates(db, &code_ids).await?;

    // On-hand per item in the register's warehouse, one query.
    let item_ids: Vec<Uuid> = items.iter().map(|i| i.id).collect();
    let levels: HashMap<Uuid, Decimal> = stock::level::Entity::find()
        .filter(stock::level::Column::WarehouseId.eq(register_row.warehouse_id))
        .filter(stock::level::Column::ItemId.is_in(item_ids.clone()))
        .all(db)
        .await?
        .into_iter()
        .map(|l| (l.item_id, l.on_hand))
        .collect();

    // Per-batch on-hand for tracked items, one grouped ledger scan.
    let mut batch_stock: HashMap<Uuid, Vec<CatalogBatch>> = HashMap::new();
    if items.iter().any(|i| i.track_batches) {
        let rows = db
            .query_all(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT l.item_id, l.batch_id, b.batch_no, b.expires_on,
                        SUM(l.qty_delta)::numeric AS on_hand
                 FROM inventory_stock_ledger l
                 JOIN inventory_batches b ON b.id = l.batch_id
                 WHERE l.warehouse_id = $1 AND l.batch_id IS NOT NULL
                 GROUP BY l.item_id, l.batch_id, b.batch_no, b.expires_on
                 HAVING SUM(l.qty_delta) > 0
                 ORDER BY b.expires_on NULLS LAST, b.batch_no",
                [register_row.warehouse_id.into()],
            ))
            .await?;
        for r in rows {
            let item_id: Uuid = r
                .try_get("", "item_id")
                .map_err(|e| Error::internal(format!("batch stock item: {e}")))?;
            let batch_id: Uuid = r
                .try_get("", "batch_id")
                .map_err(|e| Error::internal(format!("batch stock batch: {e}")))?;
            let batch_no: String = r.try_get("", "batch_no").unwrap_or_default();
            let expires_on: Option<chrono::NaiveDate> = r.try_get("", "expires_on").ok();
            let on_hand: Decimal = r.try_get("", "on_hand").unwrap_or(Decimal::ZERO);
            batch_stock.entry(item_id).or_default().push(CatalogBatch {
                id: batch_id,
                batch_no,
                expires_on,
                on_hand,
            });
        }
    }

    let service = SaleService::new(db.clone());
    let pricing = PricingService::new(db.clone());
    let mut out = Vec::with_capacity(items.len());
    for item_row in items {
        let (price, source) = match service
            .register_list_price(&register_row, &item_row, Decimal::ONE, &buyer.currency, today)
            .await?
        {
            Some((price, list_id)) => (price, PriceSource::List(list_id).as_string()),
            None => match pricing
                .price_line(
                    PriceQuery {
                        customer_id: buyer.id,
                        item_id: item_row.id,
                        qty: Decimal::ONE,
                        uom_id: None,
                        currency: buyer.currency.clone(),
                        date: today,
                    },
                    None,
                    false,
                )
                .await
            {
                Ok((price, source)) => (price, source.as_string()),
                // An item with no price anywhere still belongs in the
                // catalog — the till shows it unpriced and refuses to
                // sell it until a price exists.
                Err(_) => (Decimal::ZERO, PriceSource::ItemDefault.as_string()),
            },
        };
        let rate = if buyer.tax_exempt {
            Decimal::ZERO
        } else {
            item_row
                .sales_tax_code_id
                .and_then(|id| rates.get(&id).copied())
                .unwrap_or(Decimal::ZERO)
        };
        let stock_uom = uoms.get(&item_row.uom_id);
        out.push(CatalogItem {
            id: item_row.id,
            sku: item_row.sku,
            name: item_row.name,
            barcode: item_row.barcode,
            category_id: item_row.category_id,
            image_file_id: item_row.image_file_id,
            uom_code: stock_uom.map(|u| u.code.clone()).unwrap_or_default(),
            uom_fractional: stock_uom.map(|u| u.fractional).unwrap_or(false),
            price,
            price_source: Some(source),
            tax_code_id: item_row.sales_tax_code_id,
            tax_rate: rate,
            track_batches: item_row.track_batches,
            on_hand: levels.get(&item_row.id).copied().unwrap_or(Decimal::ZERO),
            batches: batch_stock.remove(&item_row.id).unwrap_or_default(),
            updated_at: item_row.updated_at,
        });
    }

    Ok(CatalogView {
        generated_at,
        currency: buyer.currency,
        items: out,
    })
}

// ---------------------------------------------------------------------------
// Views (API DTOs)
// ---------------------------------------------------------------------------

// The `as` aliases below keep the OpenAPI document honest: procurement also
// publishes an OrderView/OrderHeader/OrderLineView, and two schemas under one
// name silently become one schema — whichever registered last.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = PosOrderLineView)]
pub struct OrderLineView {
    pub id: Uuid,
    pub line_no: i32,
    pub item_id: Uuid,
    pub description: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub unit_price: Decimal,
    pub price_source: Option<String>,
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub discount_pct: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub tax_amount: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub net: Decimal,
    pub batch_id: Option<Uuid>,
    pub refund_of_line_id: Option<Uuid>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = PosOrderPaymentView)]
pub struct OrderPaymentView {
    pub tender: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub amount: Decimal,
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub tendered: Option<Decimal>,
    pub reference: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = PosOrderView)]
pub struct OrderView {
    pub id: Uuid,
    /// The RCP receipt number (always present once captured server-side).
    pub number: Option<String>,
    pub client_uuid: Uuid,
    pub session_id: Uuid,
    pub kind: OrderKind,
    pub status: OrderStatus,
    pub customer_id: Uuid,
    pub customer_name: String,
    #[schema(value_type = String, format = DateTime)]
    pub sold_at: chrono::DateTime<chrono::Utc>,
    pub currency: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub subtotal: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub discount_total: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub tax_total: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total: Decimal,
    /// Cash change due, summed over cash tenders.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub change: Decimal,
    pub refund_of_id: Option<Uuid>,
    pub captured_offline: bool,
    pub price_drift: bool,
    pub void_reason: Option<String>,
    pub lines: Vec<OrderLineView>,
    pub payments: Vec<OrderPaymentView>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = PosOrderHeader)]
pub struct OrderHeader {
    pub id: Uuid,
    pub number: Option<String>,
    pub client_uuid: Uuid,
    pub session_id: Uuid,
    pub kind: OrderKind,
    pub status: OrderStatus,
    #[schema(value_type = String, format = DateTime)]
    pub sold_at: chrono::DateTime<chrono::Utc>,
    pub currency: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total: Decimal,
    pub price_drift: bool,
    pub captured_offline: bool,
}

pub struct OrderFilter {
    pub session_id: Option<Uuid>,
    pub kind: Option<OrderKind>,
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SaleLineRequest {
    pub item_id: Uuid,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    /// The tax-inclusive unit price the till charged.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub unit_price: Decimal,
    /// The cashier keyed the price by hand (override-gated).
    #[serde(default)]
    pub manual_price: bool,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub discount_pct: Option<Decimal>,
    /// Required when the item tracks batches.
    pub batch_id: Option<Uuid>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SaleTenderRequest {
    /// cash | mpesa | card.
    pub tender: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub amount: Decimal,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub tendered: Option<Decimal>,
    pub reference: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateSaleRequest {
    /// The till-generated idempotency key.
    pub client_uuid: Uuid,
    pub session_id: Uuid,
    /// Attach a real customer; absent = the register's default buyer.
    pub customer_id: Option<Uuid>,
    /// The client-captured moment of sale.
    #[schema(value_type = String, format = DateTime)]
    pub sold_at: chrono::DateTime<chrono::Utc>,
    /// True when this sale was captured with the network down and is
    /// arriving through the queue replay.
    #[serde(default)]
    pub captured_offline: bool,
    pub lines: Vec<SaleLineRequest>,
    pub tenders: Vec<SaleTenderRequest>,
    /// Till-measured seconds from first line to payment (instrumentation).
    pub capture_seconds: Option<i32>,
    /// Till-measured inputs (taps/keys/scans) the sale cost.
    pub input_count: Option<i32>,
    /// Supervisor approval for override-gated content, when the cashier
    /// lacks the permission themselves.
    pub approval: Option<PinApproval>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SyncSalesRequest {
    pub sales: Vec<CreateSaleRequest>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct SyncSaleResult {
    pub client_uuid: Uuid,
    /// The captured (or already-existing) order; absent on error.
    pub order: Option<OrderView>,
    pub error: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct VoidSaleRequest {
    pub reason: String,
    /// Supervisor approval when the cashier lacks the override permission.
    pub approval: Option<PinApproval>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct RefundLineRequest {
    pub line_id: Uuid,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct RefundSaleRequest {
    /// The till-generated idempotency key of the refund document.
    pub client_uuid: Uuid,
    /// The open session the refund is paid out of.
    pub session_id: Uuid,
    pub lines: Vec<RefundLineRequest>,
    /// cash | mpesa | card — original tender by default, the till passes
    /// it explicitly.
    pub tender: String,
    pub reference: Option<String>,
    /// Supervisor approval when the cashier lacks the override permission.
    pub approval: Option<PinApproval>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListSalesQuery {
    pub session_id: Option<Uuid>,
    pub kind: Option<OrderKind>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CatalogQuery {
    pub register_id: Uuid,
    /// Only items updated after this moment (the previous response's
    /// `generated_at`); absent = the full catalog.
    pub since: Option<chrono::DateTime<chrono::Utc>>,
}

fn new_sale(req: CreateSaleRequest, allow_override: bool, created_by: Option<Uuid>) -> NewSale {
    NewSale {
        client_uuid: req.client_uuid,
        session_id: req.session_id,
        customer_id: req.customer_id,
        sold_at: req.sold_at,
        captured_offline: req.captured_offline,
        lines: req
            .lines
            .into_iter()
            .map(|l| SaleLineInput {
                item_id: l.item_id,
                qty: l.qty,
                unit_price: l.unit_price,
                manual_price: l.manual_price,
                discount_pct: l.discount_pct,
                batch_id: l.batch_id,
            })
            .collect(),
        tenders: req
            .tenders
            .into_iter()
            .map(|t| TenderInput {
                tender: t.tender,
                amount: t.amount,
                tendered: t.tendered,
                reference: t.reference,
            })
            .collect(),
        allow_override,
        capture_seconds: req.capture_seconds,
        input_count: req.input_count,
        created_by,
    }
}

/// Whether a sale request carries override-gated content at all — when it
/// does not, no override needs resolving.
fn needs_override(req: &CreateSaleRequest) -> bool {
    req.lines
        .iter()
        .any(|l| l.manual_price || l.discount_pct.is_some_and(|p| !p.is_zero()))
}

pub(crate) fn routes() -> Router {
    Router::new()
        .route("/pos/sales", get(list_sales).post(capture_sale))
        .route("/pos/sales/sync", post(sync_sales))
        .route("/pos/sales/{id}", get(get_sale))
        .route("/pos/sales/{id}/void", post(void_sale))
        .route("/pos/sales/{id}/refund", post(refund_sale))
        .route("/pos/catalog", get(get_catalog))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    list_sales,
    get_sale,
    capture_sale,
    sync_sales,
    void_sale,
    refund_sale,
    get_catalog
))]
struct ApiDoc;

#[utoipa::path(get, path = "/pos/sales", tag = "pos",
    params(ListSalesQuery),
    responses((status = 200, body = Vec<OrderHeader>)))]
async fn list_sales(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<ListSalesQuery>,
) -> Result<Json<Vec<OrderHeader>>> {
    authz.require(names::SELL).await?;
    SaleService::new(db)
        .list(OrderFilter {
            session_id: q.session_id,
            kind: q.kind,
        })
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/pos/sales/{id}", tag = "pos",
    params(("id" = Uuid, Path, description = "Order id")),
    responses((status = 200, body = OrderView)))]
async fn get_sale(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<OrderView>> {
    authz.require(names::SELL).await?;
    SaleService::new(db).view(id).await.map(Json)
}

#[utoipa::path(post, path = "/pos/sales", tag = "pos",
    request_body = CreateSaleRequest,
    responses((status = 200, body = OrderView)))]
async fn capture_sale(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Extension(numbering): Extension<Numbering>,
    Json(req): Json<CreateSaleRequest>,
) -> Result<Json<OrderView>> {
    authz.require(names::SELL).await?;
    let (allow_override, approved_by) = if needs_override(&req) {
        resolve_override(&authz, &db, req.approval.as_ref()).await?
    } else {
        (false, None)
    };
    let view = SaleService::new(db)
        .capture(new_sale(req, allow_override, Some(authz.user.id)), &numbering)
        .await?;
    if let Some(approver) = approved_by {
        audit
            .0
            .event(format!(
                "sale {} carried overrides approved by user {approver}",
                view.number.as_deref().unwrap_or("")
            ))
            .await;
    }
    Ok(Json(view))
}

/// Replay the offline queue: each sale captures independently, so one
/// bad ticket never blocks the rest; already-captured entries come back
/// as their existing orders. Override-gated content in an offline sale
/// rides on the syncing cashier's own permission — a PIN cannot be
/// verified after the fact.
#[utoipa::path(post, path = "/pos/sales/sync", tag = "pos",
    request_body = SyncSalesRequest,
    responses((status = 200, body = Vec<SyncSaleResult>)))]
async fn sync_sales(
    authz: Authz,
    TenantDb(db): TenantDb,
    Extension(numbering): Extension<Numbering>,
    Json(req): Json<SyncSalesRequest>,
) -> Result<Json<Vec<SyncSaleResult>>> {
    authz.require(names::SELL).await?;
    let allow_override = authz.is_granted(names::OVERRIDE).await?;
    let service = SaleService::new(db);
    let mut results = Vec::with_capacity(req.sales.len());
    for mut sale in req.sales {
        // Everything replayed from the queue was captured offline,
        // whatever the client says.
        sale.captured_offline = true;
        let client_uuid = sale.client_uuid;
        match service
            .capture(
                new_sale(sale, allow_override, Some(authz.user.id)),
                &numbering,
            )
            .await
        {
            Ok(order) => results.push(SyncSaleResult {
                client_uuid,
                order: Some(order),
                error: None,
            }),
            Err(e) => results.push(SyncSaleResult {
                client_uuid,
                order: None,
                error: Some(e.to_string()),
            }),
        }
    }
    Ok(Json(results))
}

#[utoipa::path(post, path = "/pos/sales/{id}/void", tag = "pos",
    params(("id" = Uuid, Path, description = "Order id")),
    request_body = VoidSaleRequest,
    responses((status = 200, body = OrderView)))]
async fn void_sale(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<VoidSaleRequest>,
) -> Result<Json<OrderView>> {
    authz.require(names::SELL).await?;
    let (allowed, approved_by) = resolve_override(&authz, &db, req.approval.as_ref()).await?;
    if !allowed {
        return Err(Error::Validation(
            "voiding needs the override permission or a supervisor PIN".into(),
        ));
    }
    let view = SaleService::new(db)
        .void(id, &req.reason, Some(authz.user.id))
        .await?;
    audit
        .0
        .event(format!(
            "voided sale {} ({}){}",
            view.number.as_deref().unwrap_or(""),
            req.reason.trim(),
            approved_by
                .map(|a| format!(", approved by user {a}"))
                .unwrap_or_default()
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/pos/sales/{id}/refund", tag = "pos",
    params(("id" = Uuid, Path, description = "Original order id")),
    request_body = RefundSaleRequest,
    responses((status = 200, body = OrderView)))]
async fn refund_sale(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Extension(numbering): Extension<Numbering>,
    Path(id): Path<Uuid>,
    Json(req): Json<RefundSaleRequest>,
) -> Result<Json<OrderView>> {
    authz.require(names::REFUND).await?;
    let (allowed, approved_by) = resolve_override(&authz, &db, req.approval.as_ref()).await?;
    if !allowed {
        return Err(Error::Validation(
            "refunding needs the override permission or a supervisor PIN".into(),
        ));
    }
    let view = SaleService::new(db)
        .refund(
            NewRefund {
                client_uuid: req.client_uuid,
                session_id: req.session_id,
                original_id: id,
                lines: req
                    .lines
                    .into_iter()
                    .map(|l| RefundLineInput {
                        line_id: l.line_id,
                        qty: l.qty,
                    })
                    .collect(),
                tender: req.tender,
                reference: req.reference,
                created_by: Some(authz.user.id),
            },
            &numbering,
        )
        .await?;
    audit
        .0
        .event(format!(
            "refunded {} against sale {}{}",
            view.total,
            id,
            approved_by
                .map(|a| format!(", approved by user {a}"))
                .unwrap_or_default()
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(get, path = "/pos/catalog", tag = "pos",
    params(CatalogQuery),
    responses((status = 200, body = CatalogView)))]
async fn get_catalog(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<CatalogQuery>,
) -> Result<Json<CatalogView>> {
    authz.require(names::SELL).await?;
    catalog(&db, q.register_id, q.since).await.map(Json)
}
