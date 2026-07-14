//! Purchase orders: the hub of the purchase-to-pay cycle.
//!
//! Lifecycle: draft → submitted (numbered, terms snapshotted) → approved
//! (permission-gated; `on_order` bumped on the stock levels) →
//! partially_received → received → closed; cancellation only while nothing
//! posted references the order. Receipts and invoices maintain the
//! cumulative `received_qty`/`billed_qty` counters on the lines — those
//! two columns are what make partial deliveries, partial billing, the
//! over-receipt guard and the GRNI report all fall out naturally.
//!
//! Approval is permission-only: submitting needs `Orders.Submit`, approving
//! `Orders.Approve` — no amount thresholds yet. The approver supplies the
//! exchange rate when the order currency differs from base (no rate service
//! exists in this phase). Lines order in the item's stock UoM; ordering in
//! an alternate unit arrives with UoM conversions.

use crate::scm::inventory::item::item;
use crate::scm::inventory::{stock, warehouse};
use crate::scm::procurement::permissions::names;
use crate::scm::procurement::supplier::supplier;
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
    ConnectionTrait, DatabaseConnection, PaginatorTrait, QueryOrder, QuerySelect, Set,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Where a purchase order is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Draft,
    Submitted,
    Approved,
    PartiallyReceived,
    Received,
    Closed,
    Cancelled,
}

impl OrderStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            OrderStatus::Draft => "draft",
            OrderStatus::Submitted => "submitted",
            OrderStatus::Approved => "approved",
            OrderStatus::PartiallyReceived => "partially_received",
            OrderStatus::Received => "received",
            OrderStatus::Closed => "closed",
            OrderStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "draft" => Ok(OrderStatus::Draft),
            "submitted" => Ok(OrderStatus::Submitted),
            "approved" => Ok(OrderStatus::Approved),
            "partially_received" => Ok(OrderStatus::PartiallyReceived),
            "received" => Ok(OrderStatus::Received),
            "closed" => Ok(OrderStatus::Closed),
            "cancelled" => Ok(OrderStatus::Cancelled),
            other => Err(Error::internal(format!("unknown order status {other:?}"))),
        }
    }

    /// May goods still arrive against the order?
    pub fn receivable(self) -> bool {
        matches!(self, OrderStatus::Approved | OrderStatus::PartiallyReceived)
    }
}

/// The purchase order header.
pub mod order {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "procurement_orders")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub number: Option<String>,
        pub supplier_id: Uuid,
        pub order_date: Date,
        pub expected_date: Option<Date>,
        pub deliver_to_warehouse_id: Uuid,
        pub delivery_address: Option<String>,
        pub shipping_method: Option<String>,
        pub incoterms: Option<String>,
        pub supplier_contact: Option<String>,
        pub buyer_id: Option<Uuid>,
        pub currency: String,
        #[sea_orm(column_type = "Decimal(Some((20, 8)))")]
        pub exchange_rate: Decimal,
        pub payment_terms_days: i32,
        pub tax_inclusive: bool,
        #[sea_orm(column_type = "Decimal(Some((7, 4)))", nullable)]
        pub discount_pct: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))", nullable)]
        pub discount_amount: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))", nullable)]
        pub other_charges: Option<Decimal>,
        pub memo: Option<String>,
        pub reference: Option<String>,
        pub terms_and_conditions: Option<String>,
        pub status: String,
        pub submitted_at: Option<DateTimeUtc>,
        pub submitted_by: Option<Uuid>,
        pub approved_at: Option<DateTimeUtc>,
        pub approved_by: Option<Uuid>,
        pub cancelled_at: Option<DateTimeUtc>,
        pub cancelled_by: Option<Uuid>,
        pub cancel_reason: Option<String>,
        pub closed_at: Option<DateTimeUtc>,
        pub closed_by: Option<Uuid>,
        pub created_at: DateTimeUtc,
        pub created_by: Option<Uuid>,
        pub updated_at: DateTimeUtc,
        pub updated_by: Option<Uuid>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// One purchase order line. `received_qty`/`billed_qty` are cumulative,
/// maintained by receipt and invoice posting under the order row lock.
pub mod order_line {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "procurement_order_lines")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub order_id: Uuid,
        pub line_no: i32,
        pub item_id: Uuid,
        pub description: Option<String>,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub qty: Decimal,
        pub uom_id: Option<Uuid>,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub unit_price: Decimal,
        #[sea_orm(column_type = "Decimal(Some((7, 4)))", nullable)]
        pub discount_pct: Option<Decimal>,
        pub tax_code_id: Option<Uuid>,
        pub expected_date: Option<Date>,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub received_qty: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub billed_qty: Decimal,
        pub memo: Option<String>,
        pub created_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// A line's price after its own discount, at cost precision. Receipts cost
/// stock with it, the three-way match compares with it, GRNI values with
/// it — one definition.
pub(crate) fn effective_price(unit_price: Decimal, discount_pct: Option<Decimal>) -> Decimal {
    let pct = discount_pct.unwrap_or(Decimal::ZERO);
    stock::round_cost(unit_price * (Decimal::ONE_HUNDRED - pct) / Decimal::ONE_HUNDRED)
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// An order line as supplied by a caller.
pub struct OrderLineInput {
    pub item_id: Uuid,
    pub description: Option<String>,
    pub qty: Decimal,
    pub unit_price: Decimal,
    pub discount_pct: Option<Decimal>,
    pub tax_code_id: Option<Uuid>,
    pub expected_date: Option<chrono::NaiveDate>,
    pub memo: Option<String>,
}

/// A new draft purchase order as supplied by a caller.
pub struct NewOrder {
    pub supplier_id: Uuid,
    pub order_date: chrono::NaiveDate,
    pub expected_date: Option<chrono::NaiveDate>,
    pub deliver_to_warehouse_id: Uuid,
    pub delivery_address: Option<String>,
    pub shipping_method: Option<String>,
    pub incoterms: Option<String>,
    pub supplier_contact: Option<String>,
    pub currency: Option<String>,
    pub payment_terms_days: Option<i32>,
    pub tax_inclusive: bool,
    pub discount_pct: Option<Decimal>,
    pub discount_amount: Option<Decimal>,
    pub other_charges: Option<Decimal>,
    pub memo: Option<String>,
    pub reference: Option<String>,
    pub terms_and_conditions: Option<String>,
    pub lines: Vec<OrderLineInput>,
    pub created_by: Option<Uuid>,
}

/// The purchase order service over one (tenant) connection.
pub struct OrderService {
    db: DatabaseConnection,
}

impl OrderService {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Create a draft order. Currency and payment terms default from the
    /// supplier; the supplier must be active and not on hold — a hold
    /// blocks new commitments while in-flight documents finish.
    pub async fn create_draft(&self, new: NewOrder) -> Result<OrderView> {
        let supplier = load_supplier_for_new_order(&self.db, new.supplier_id).await?;
        validate_order(&self.db, &new).await?;
        let currency = match &new.currency {
            Some(c) => validate_currency(c)?,
            None => supplier.currency.clone(),
        };
        let txn = self.db.begin().await?;
        let order_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        order::ActiveModel {
            id: Set(order_id),
            number: Set(None),
            supplier_id: Set(new.supplier_id),
            order_date: Set(new.order_date),
            expected_date: Set(new.expected_date),
            deliver_to_warehouse_id: Set(new.deliver_to_warehouse_id),
            delivery_address: Set(clean(new.delivery_address)),
            shipping_method: Set(clean(new.shipping_method)),
            incoterms: Set(clean(new.incoterms)),
            supplier_contact: Set(clean(new.supplier_contact)),
            buyer_id: Set(new.created_by),
            currency: Set(currency),
            exchange_rate: Set(Decimal::ONE),
            payment_terms_days: Set(new.payment_terms_days.unwrap_or(supplier.payment_terms_days)),
            tax_inclusive: Set(new.tax_inclusive),
            discount_pct: Set(new.discount_pct),
            discount_amount: Set(new.discount_amount),
            other_charges: Set(new.other_charges),
            memo: Set(clean(new.memo)),
            reference: Set(clean(new.reference)),
            terms_and_conditions: Set(clean(new.terms_and_conditions)),
            status: Set(OrderStatus::Draft.as_str().to_string()),
            submitted_at: Set(None),
            submitted_by: Set(None),
            approved_at: Set(None),
            approved_by: Set(None),
            cancelled_at: Set(None),
            cancelled_by: Set(None),
            cancel_reason: Set(None),
            closed_at: Set(None),
            closed_by: Set(None),
            created_at: Set(now),
            created_by: Set(new.created_by),
            updated_at: Set(now),
            updated_by: Set(None),
        }
        .insert(&txn)
        .await?;
        insert_lines(&txn, order_id, &new.lines).await?;
        txn.commit().await?;
        self.view(order_id).await
    }

    /// Replace a draft's header and lines wholesale.
    pub async fn update_draft(&self, id: Uuid, new: NewOrder) -> Result<OrderView> {
        let supplier = load_supplier_for_new_order(&self.db, new.supplier_id).await?;
        validate_order(&self.db, &new).await?;
        let currency = match &new.currency {
            Some(c) => validate_currency(c)?,
            None => supplier.currency.clone(),
        };
        let txn = self.db.begin().await?;
        let existing = load_order_locked(&txn, id).await?;
        if OrderStatus::parse(&existing.status)? != OrderStatus::Draft {
            return Err(Error::Validation("only a draft order can be edited".into()));
        }
        order_line::Entity::delete_many()
            .filter(order_line::Column::OrderId.eq(id))
            .exec(&txn)
            .await?;
        insert_lines(&txn, id, &new.lines).await?;

        let mut active: order::ActiveModel = existing.into();
        active.supplier_id = Set(new.supplier_id);
        active.order_date = Set(new.order_date);
        active.expected_date = Set(new.expected_date);
        active.deliver_to_warehouse_id = Set(new.deliver_to_warehouse_id);
        active.delivery_address = Set(clean(new.delivery_address));
        active.shipping_method = Set(clean(new.shipping_method));
        active.incoterms = Set(clean(new.incoterms));
        active.supplier_contact = Set(clean(new.supplier_contact));
        active.currency = Set(currency);
        active.payment_terms_days =
            Set(new.payment_terms_days.unwrap_or(supplier.payment_terms_days));
        active.tax_inclusive = Set(new.tax_inclusive);
        active.discount_pct = Set(new.discount_pct);
        active.discount_amount = Set(new.discount_amount);
        active.other_charges = Set(new.other_charges);
        active.memo = Set(clean(new.memo));
        active.reference = Set(clean(new.reference));
        active.terms_and_conditions = Set(clean(new.terms_and_conditions));
        active.updated_at = Set(chrono::Utc::now());
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Delete a draft (lines cascade). Anything submitted is history.
    pub async fn delete_draft(&self, id: Uuid) -> Result<OrderView> {
        let view = self.view(id).await?;
        let txn = self.db.begin().await?;
        let existing = load_order_locked(&txn, id).await?;
        if OrderStatus::parse(&existing.status)? != OrderStatus::Draft {
            return Err(Error::Validation("only a draft order can be deleted".into()));
        }
        order::Entity::delete_by_id(id).exec(&txn).await?;
        txn.commit().await?;
        Ok(view)
    }

    /// Submit a draft: the moment it becomes a real commitment. Re-checks
    /// the supplier hold, snapshots terms the draft left unset, allocates
    /// the PO number — the document is frozen from here.
    pub async fn submit(&self, id: Uuid, numbering: &Numbering, by: Option<Uuid>) -> Result<OrderView> {
        let txn = self.db.begin().await?;
        let existing = load_order_locked(&txn, id).await?;
        if OrderStatus::parse(&existing.status)? != OrderStatus::Draft {
            return Err(Error::Validation("only a draft order can be submitted".into()));
        }
        let supplier = load_supplier_for_new_order(&txn, existing.supplier_id).await?;
        let lines = load_lines(&txn, id).await?;
        if lines.is_empty() {
            return Err(Error::Validation("an order needs at least one line".into()));
        }
        let number = numbering.next(&txn, crate::scm::ORDER_SERIES).await?;
        let now = chrono::Utc::now();
        let incoterms = existing.incoterms.clone().or(supplier.incoterms.clone());
        let mut active: order::ActiveModel = existing.into();
        active.number = Set(Some(number.formatted));
        active.incoterms = Set(incoterms);
        active.status = Set(OrderStatus::Submitted.as_str().to_string());
        active.submitted_at = Set(Some(now));
        active.submitted_by = Set(by);
        active.updated_at = Set(now);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Approve a submitted order and commit the demand: `on_order` rises
    /// on the delivery warehouse's levels, locked in ascending item order.
    /// The caller supplies the exchange rate when the order currency is
    /// not the base currency (there is no rate service yet).
    pub async fn approve(
        &self,
        id: Uuid,
        exchange_rate: Option<Decimal>,
        by: Option<Uuid>,
    ) -> Result<OrderView> {
        if exchange_rate.is_some_and(|r| r <= Decimal::ZERO) {
            return Err(Error::Validation("exchange rate must be positive".into()));
        }
        let txn = self.db.begin().await?;
        let existing = load_order_locked(&txn, id).await?;
        if OrderStatus::parse(&existing.status)? != OrderStatus::Submitted {
            return Err(Error::Validation(
                "only a submitted order can be approved".into(),
            ));
        }
        let lines = load_lines(&txn, id).await?;
        let mut by_item: HashMap<Uuid, Decimal> = HashMap::new();
        for l in &lines {
            *by_item.entry(l.item_id).or_default() += l.qty;
        }
        let mut demand: Vec<(Uuid, Decimal)> = by_item.into_iter().collect();
        demand.sort();
        for (item_id, qty) in &demand {
            stock::StockService::adjust_on_order(
                &txn,
                *item_id,
                existing.deliver_to_warehouse_id,
                *qty,
            )
            .await?;
        }
        let now = chrono::Utc::now();
        let mut active: order::ActiveModel = existing.into();
        if let Some(rate) = exchange_rate {
            active.exchange_rate = Set(rate);
        }
        active.status = Set(OrderStatus::Approved.as_str().to_string());
        active.approved_at = Set(Some(now));
        active.approved_by = Set(by);
        active.updated_at = Set(now);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Cancel an order that nothing posted references yet; releases the
    /// committed `on_order` demand if it was approved.
    pub async fn cancel(&self, id: Uuid, reason: &str, by: Option<Uuid>) -> Result<OrderView> {
        let txn = self.db.begin().await?;
        let existing = load_order_locked(&txn, id).await?;
        let status = OrderStatus::parse(&existing.status)?;
        if !matches!(
            status,
            OrderStatus::Draft | OrderStatus::Submitted | OrderStatus::Approved
        ) {
            return Err(Error::Validation(format!(
                "a {} order cannot be cancelled",
                status.as_str()
            )));
        }
        let receipts = super::receipt::receipt::Entity::find()
            .filter(super::receipt::receipt::Column::OrderId.eq(id))
            .filter(super::receipt::receipt::Column::Status.ne("draft"))
            .count(&txn)
            .await?;
        if receipts > 0 {
            return Err(Error::Validation(
                "the order has posted goods receipts and cannot be cancelled".into(),
            ));
        }
        let invoices = super::invoice::invoice::Entity::find()
            .filter(super::invoice::invoice::Column::OrderId.eq(id))
            .filter(super::invoice::invoice::Column::Status.eq("posted"))
            .count(&txn)
            .await?;
        if invoices > 0 {
            return Err(Error::Validation(
                "the order has posted invoices and cannot be cancelled".into(),
            ));
        }
        if status == OrderStatus::Approved {
            release_on_order(&txn, &existing).await?;
        }
        let now = chrono::Utc::now();
        let mut active: order::ActiveModel = existing.into();
        active.status = Set(OrderStatus::Cancelled.as_str().to_string());
        active.cancelled_at = Set(Some(now));
        active.cancelled_by = Set(by);
        active.cancel_reason = Set(Some(reason.trim().to_string()).filter(|r| !r.is_empty()));
        active.updated_at = Set(now);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Short-close: stop expecting the undelivered remainder and release
    /// its `on_order` demand. Billing what was received stays possible.
    pub async fn close(&self, id: Uuid, by: Option<Uuid>) -> Result<OrderView> {
        let txn = self.db.begin().await?;
        let existing = load_order_locked(&txn, id).await?;
        let status = OrderStatus::parse(&existing.status)?;
        if !matches!(
            status,
            OrderStatus::Approved | OrderStatus::PartiallyReceived | OrderStatus::Received
        ) {
            return Err(Error::Validation(format!(
                "a {} order cannot be closed",
                status.as_str()
            )));
        }
        release_on_order(&txn, &existing).await?;
        let now = chrono::Utc::now();
        let mut active: order::ActiveModel = existing.into();
        active.status = Set(OrderStatus::Closed.as_str().to_string());
        active.closed_at = Set(Some(now));
        active.closed_by = Set(by);
        active.updated_at = Set(now);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    pub async fn list(&self, filter: OrderFilter) -> Result<Vec<OrderHeader>> {
        let mut query = order::Entity::find();
        if let Some(s) = filter.status {
            query = query.filter(order::Column::Status.eq(s.as_str()));
        }
        if let Some(supplier_id) = filter.supplier_id {
            query = query.filter(order::Column::SupplierId.eq(supplier_id));
        }
        if let Some(from) = filter.from {
            query = query.filter(order::Column::OrderDate.gte(from));
        }
        if let Some(to) = filter.to {
            query = query.filter(order::Column::OrderDate.lte(to));
        }
        let rows = query
            .order_by_desc(order::Column::OrderDate)
            .order_by_desc(order::Column::CreatedAt)
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
                Ok(OrderHeader {
                    id: r.id,
                    number: r.number.clone(),
                    supplier_id: r.supplier_id,
                    supplier_name: suppliers
                        .get(&r.supplier_id)
                        .map(|s| s.name.clone())
                        .unwrap_or_default(),
                    order_date: r.order_date,
                    expected_date: r.expected_date,
                    currency: r.currency.clone(),
                    status: OrderStatus::parse(&r.status)?,
                })
            })
            .collect()
    }

    /// Load a full order with lines, labels and computed totals.
    pub async fn view(&self, id: Uuid) -> Result<OrderView> {
        let row = load_order(&self.db, id).await?;
        let lines = load_lines(&self.db, id).await?;
        let supplier = supplier::Entity::find_by_id(row.supplier_id)
            .one(&self.db)
            .await?;
        let wh = warehouse::Entity::find_by_id(row.deliver_to_warehouse_id)
            .one(&self.db)
            .await?;
        let item_ids: Vec<Uuid> = lines.iter().map(|l| l.item_id).collect();
        let items: HashMap<Uuid, item::Model> = item::Entity::find()
            .filter(item::Column::Id.is_in(item_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|i| (i.id, i))
            .collect();

        let mut subtotal = Decimal::ZERO;
        let line_views: Vec<OrderLineView> = lines
            .into_iter()
            .map(|l| {
                let item = items.get(&l.item_id);
                let price = effective_price(l.unit_price, l.discount_pct);
                let net = stock::round_money(l.qty * price);
                subtotal += net;
                OrderLineView {
                    id: l.id,
                    line_no: l.line_no,
                    item_id: l.item_id,
                    sku: item.map(|i| i.sku.clone()).unwrap_or_default(),
                    item_name: item.map(|i| i.name.clone()).unwrap_or_default(),
                    description: l.description,
                    qty: l.qty,
                    unit_price: l.unit_price,
                    discount_pct: l.discount_pct,
                    effective_price: price,
                    net,
                    received_qty: l.received_qty,
                    billed_qty: l.billed_qty,
                    expected_date: l.expected_date,
                    memo: l.memo,
                }
            })
            .collect();

        // Header discount (percent first, then absolute), plus charges.
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

        Ok(OrderView {
            id: row.id,
            number: row.number,
            supplier_id: row.supplier_id,
            supplier_name: supplier.map(|s| s.name).unwrap_or_default(),
            order_date: row.order_date,
            expected_date: row.expected_date,
            deliver_to_warehouse_id: row.deliver_to_warehouse_id,
            warehouse_code: wh.map(|w| w.code).unwrap_or_default(),
            delivery_address: row.delivery_address,
            shipping_method: row.shipping_method,
            incoterms: row.incoterms,
            supplier_contact: row.supplier_contact,
            buyer_id: row.buyer_id,
            currency: row.currency,
            exchange_rate: row.exchange_rate,
            payment_terms_days: row.payment_terms_days,
            tax_inclusive: row.tax_inclusive,
            discount_pct: row.discount_pct,
            discount_amount: row.discount_amount,
            other_charges: row.other_charges,
            memo: row.memo,
            reference: row.reference,
            terms_and_conditions: row.terms_and_conditions,
            status: OrderStatus::parse(&row.status)?,
            cancel_reason: row.cancel_reason,
            subtotal,
            total,
            created_at: row.created_at,
            lines: line_views,
        })
    }
}

/// Release whatever `on_order` demand the order still holds — the ordered
/// remainder of every line, floored at zero by the engine.
async fn release_on_order(
    txn: &sea_orm::DatabaseTransaction,
    row: &order::Model,
) -> Result<()> {
    let lines = load_lines(txn, row.id).await?;
    let mut by_item: HashMap<Uuid, Decimal> = HashMap::new();
    for l in &lines {
        let remaining = l.qty - l.received_qty;
        if remaining > Decimal::ZERO {
            *by_item.entry(l.item_id).or_default() += remaining;
        }
    }
    let mut demand: Vec<(Uuid, Decimal)> = by_item.into_iter().collect();
    demand.sort();
    for (item_id, qty) in &demand {
        stock::StockService::adjust_on_order(txn, *item_id, row.deliver_to_warehouse_id, -*qty)
            .await?;
    }
    Ok(())
}

fn clean(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

fn validate_currency(c: &str) -> Result<String> {
    let currency = c.trim().to_uppercase();
    if currency.len() != 3 || !currency.chars().all(|ch| ch.is_ascii_alphabetic()) {
        return Err(Error::Validation(format!(
            "currency {c:?} is not an ISO 4217 code"
        )));
    }
    Ok(currency)
}

/// The supplier a new order commits to: must exist, be active, and not be
/// on hold.
async fn load_supplier_for_new_order<C: ConnectionTrait>(
    conn: &C,
    supplier_id: Uuid,
) -> Result<supplier::Model> {
    let found = supplier::Entity::find_by_id(supplier_id).one(conn).await?;
    let Some(found) = found else {
        return Err(Error::NotFound(format!("supplier {supplier_id}")));
    };
    if !found.is_active {
        return Err(Error::Validation(format!(
            "supplier {} is inactive",
            found.code
        )));
    }
    if found.on_hold {
        return Err(Error::Validation(format!(
            "supplier {} is on hold{}",
            found.code,
            found
                .hold_reason
                .as_deref()
                .map(|r| format!(": {r}"))
                .unwrap_or_default()
        )));
    }
    Ok(found)
}

/// Draft-time validation of everything but the supplier (checked where the
/// order is written, so the hold message can name the code).
async fn validate_order<C: ConnectionTrait>(conn: &C, new: &NewOrder) -> Result<()> {
    if new.lines.is_empty() {
        return Err(Error::Validation("an order needs at least one line".into()));
    }
    let wh = warehouse::Entity::find_by_id(new.deliver_to_warehouse_id)
        .one(conn)
        .await?;
    match wh {
        Some(w) if w.is_active => {}
        Some(w) => {
            return Err(Error::Validation(format!(
                "warehouse {} is inactive",
                w.code
            )));
        }
        None => {
            return Err(Error::Validation(format!(
                "warehouse {} does not exist",
                new.deliver_to_warehouse_id
            )));
        }
    }
    if let Some(pct) = new.discount_pct {
        if pct < Decimal::ZERO || pct > Decimal::ONE_HUNDRED {
            return Err(Error::Validation(
                "discount must be between 0 and 100 percent".into(),
            ));
        }
    }
    if new.payment_terms_days.is_some_and(|d| d < 0) {
        return Err(Error::Validation("payment terms must not be negative".into()));
    }

    let item_ids: Vec<Uuid> = new.lines.iter().map(|l| l.item_id).collect();
    let items: HashMap<Uuid, item::Model> = item::Entity::find()
        .filter(item::Column::Id.is_in(item_ids))
        .all(conn)
        .await?
        .into_iter()
        .map(|i| (i.id, i))
        .collect();
    for (i, l) in new.lines.iter().enumerate() {
        let line_no = i + 1;
        let Some(item) = items.get(&l.item_id) else {
            return Err(Error::NotFound(format!("item {}", l.item_id)));
        };
        if !item.is_active {
            return Err(Error::Validation(format!(
                "line {line_no}: item {} is inactive",
                item.sku
            )));
        }
        if !item.is_purchasable {
            return Err(Error::Validation(format!(
                "line {line_no}: item {} is not purchasable",
                item.sku
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
    Ok(())
}

/// Insert a set of lines for an order, numbered from one. Lines are in
/// the item's stock UoM (`uom_id` stays NULL until UoM conversions land).
async fn insert_lines<C: ConnectionTrait>(
    conn: &C,
    order_id: Uuid,
    lines: &[OrderLineInput],
) -> Result<()> {
    let now = chrono::Utc::now();
    for (i, l) in lines.iter().enumerate() {
        order_line::ActiveModel {
            id: Set(Uuid::new_v4()),
            order_id: Set(order_id),
            line_no: Set((i + 1) as i32),
            item_id: Set(l.item_id),
            description: Set(l.description.clone().filter(|d| !d.trim().is_empty())),
            qty: Set(l.qty),
            uom_id: Set(None),
            unit_price: Set(l.unit_price),
            discount_pct: Set(l.discount_pct),
            tax_code_id: Set(l.tax_code_id),
            expected_date: Set(l.expected_date),
            received_qty: Set(Decimal::ZERO),
            billed_qty: Set(Decimal::ZERO),
            memo: Set(l.memo.clone().filter(|m| !m.trim().is_empty())),
            created_at: Set(now),
        }
        .insert(conn)
        .await?;
    }
    Ok(())
}

pub(crate) async fn load_order<C: ConnectionTrait>(conn: &C, id: Uuid) -> Result<order::Model> {
    order::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("purchase order {id}")))
}

/// Load an order holding its row lock — the serialization point for
/// everything that mutates its counters (receipts, invoices, transitions).
pub(crate) async fn load_order_locked(
    txn: &sea_orm::DatabaseTransaction,
    id: Uuid,
) -> Result<order::Model> {
    order::Entity::find_by_id(id)
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("purchase order {id}")))
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

/// Recompute the order's fulfilment status from its line counters; only
/// moves between approved / partially_received / received.
pub(crate) async fn recompute_status(
    txn: &sea_orm::DatabaseTransaction,
    row: order::Model,
) -> Result<()> {
    let status = OrderStatus::parse(&row.status)?;
    if !matches!(
        status,
        OrderStatus::Approved | OrderStatus::PartiallyReceived | OrderStatus::Received
    ) {
        return Ok(());
    }
    let lines = load_lines(txn, row.id).await?;
    let any_received = lines.iter().any(|l| l.received_qty > Decimal::ZERO);
    let all_received = lines.iter().all(|l| l.received_qty >= l.qty);
    let next = if all_received {
        OrderStatus::Received
    } else if any_received {
        OrderStatus::PartiallyReceived
    } else {
        OrderStatus::Approved
    };
    if next != status {
        let mut active: order::ActiveModel = row.into();
        active.status = Set(next.as_str().to_string());
        active.updated_at = Set(chrono::Utc::now());
        active.update(txn).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Views (API DTOs)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct OrderLineView {
    pub id: Uuid,
    pub line_no: i32,
    pub item_id: Uuid,
    pub sku: String,
    pub item_name: String,
    pub description: Option<String>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub unit_price: Decimal,
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub discount_pct: Option<Decimal>,
    /// Unit price after the line discount — what receipts cost stock at.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub effective_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub net: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub received_qty: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub billed_qty: Decimal,
    #[schema(value_type = Option<String>, format = Date)]
    pub expected_date: Option<chrono::NaiveDate>,
    pub memo: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct OrderView {
    pub id: Uuid,
    pub number: Option<String>,
    pub supplier_id: Uuid,
    pub supplier_name: String,
    #[schema(value_type = String, format = Date)]
    pub order_date: chrono::NaiveDate,
    #[schema(value_type = Option<String>, format = Date)]
    pub expected_date: Option<chrono::NaiveDate>,
    pub deliver_to_warehouse_id: Uuid,
    pub warehouse_code: String,
    pub delivery_address: Option<String>,
    pub shipping_method: Option<String>,
    pub incoterms: Option<String>,
    pub supplier_contact: Option<String>,
    pub buyer_id: Option<Uuid>,
    pub currency: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub exchange_rate: Decimal,
    pub payment_terms_days: i32,
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
    pub memo: Option<String>,
    pub reference: Option<String>,
    pub terms_and_conditions: Option<String>,
    pub status: OrderStatus,
    pub cancel_reason: Option<String>,
    /// Sum of line nets, order currency.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub subtotal: Decimal,
    /// After header discounts and other charges, before tax.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total: Decimal,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub lines: Vec<OrderLineView>,
}

/// A row of the order register.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct OrderHeader {
    pub id: Uuid,
    pub number: Option<String>,
    pub supplier_id: Uuid,
    pub supplier_name: String,
    #[schema(value_type = String, format = Date)]
    pub order_date: chrono::NaiveDate,
    #[schema(value_type = Option<String>, format = Date)]
    pub expected_date: Option<chrono::NaiveDate>,
    pub currency: String,
    pub status: OrderStatus,
}

pub struct OrderFilter {
    pub status: Option<OrderStatus>,
    pub supplier_id: Option<Uuid>,
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
pub struct OrderLineRequest {
    pub item_id: Uuid,
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
    #[schema(value_type = Option<String>, format = Date)]
    pub expected_date: Option<chrono::NaiveDate>,
    pub memo: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateOrderRequest {
    pub supplier_id: Uuid,
    #[schema(value_type = String, format = Date)]
    pub order_date: chrono::NaiveDate,
    #[schema(value_type = Option<String>, format = Date)]
    pub expected_date: Option<chrono::NaiveDate>,
    pub deliver_to_warehouse_id: Uuid,
    pub delivery_address: Option<String>,
    pub shipping_method: Option<String>,
    pub incoterms: Option<String>,
    pub supplier_contact: Option<String>,
    /// Defaults to the supplier's currency.
    pub currency: Option<String>,
    /// Defaults to the supplier's terms.
    pub payment_terms_days: Option<i32>,
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
    pub memo: Option<String>,
    pub reference: Option<String>,
    pub terms_and_conditions: Option<String>,
    pub lines: Vec<OrderLineRequest>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ApproveOrderRequest {
    /// Required when the order currency differs from the base currency.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub exchange_rate: Option<Decimal>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CancelOrderRequest {
    #[serde(default)]
    pub reason: String,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct ListOrdersQuery {
    pub status: Option<OrderStatus>,
    pub supplier_id: Option<Uuid>,
    /// Order date range, inclusive.
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

fn new_order(req: CreateOrderRequest, created_by: Option<Uuid>) -> NewOrder {
    NewOrder {
        supplier_id: req.supplier_id,
        order_date: req.order_date,
        expected_date: req.expected_date,
        deliver_to_warehouse_id: req.deliver_to_warehouse_id,
        delivery_address: req.delivery_address,
        shipping_method: req.shipping_method,
        incoterms: req.incoterms,
        supplier_contact: req.supplier_contact,
        currency: req.currency,
        payment_terms_days: req.payment_terms_days,
        tax_inclusive: req.tax_inclusive,
        discount_pct: req.discount_pct,
        discount_amount: req.discount_amount,
        other_charges: req.other_charges,
        memo: req.memo,
        reference: req.reference,
        terms_and_conditions: req.terms_and_conditions,
        lines: req
            .lines
            .into_iter()
            .map(|l| OrderLineInput {
                item_id: l.item_id,
                description: l.description,
                qty: l.qty,
                unit_price: l.unit_price,
                discount_pct: l.discount_pct,
                tax_code_id: l.tax_code_id,
                expected_date: l.expected_date,
                memo: l.memo,
            })
            .collect(),
        created_by,
    }
}

pub(crate) fn routes() -> Router {
    Router::new()
        .route("/procurement/orders", get(list_orders).post(create_order))
        .route(
            "/procurement/orders/{id}",
            get(get_order).put(update_order).delete(delete_order),
        )
        .route("/procurement/orders/{id}/submit", post(submit_order))
        .route("/procurement/orders/{id}/approve", post(approve_order))
        .route("/procurement/orders/{id}/cancel", post(cancel_order))
        .route("/procurement/orders/{id}/close", post(close_order))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    list_orders,
    get_order,
    create_order,
    update_order,
    delete_order,
    submit_order,
    approve_order,
    cancel_order,
    close_order
))]
struct ApiDoc;

#[utoipa::path(get, path = "/procurement/orders", tag = "procurement",
    params(ListOrdersQuery),
    responses((status = 200, body = Vec<OrderHeader>)))]
async fn list_orders(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<ListOrdersQuery>,
) -> Result<Json<Vec<OrderHeader>>> {
    authz.require(names::ORDERS_VIEW).await?;
    OrderService::new(db)
        .list(OrderFilter {
            status: q.status,
            supplier_id: q.supplier_id,
            from: q.from,
            to: q.to,
        })
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/procurement/orders/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "Order id")),
    responses((status = 200, body = OrderView)))]
async fn get_order(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<OrderView>> {
    authz.require(names::ORDERS_VIEW).await?;
    OrderService::new(db).view(id).await.map(Json)
}

#[utoipa::path(post, path = "/procurement/orders", tag = "procurement",
    request_body = CreateOrderRequest,
    responses((status = 200, body = OrderView)))]
async fn create_order(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(req): Json<CreateOrderRequest>,
) -> Result<Json<OrderView>> {
    authz.require(names::ORDERS_CREATE).await?;
    let view = OrderService::new(db)
        .create_draft(new_order(req, Some(authz.user.id)))
        .await?;
    audit.0.created("scm.order", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(put, path = "/procurement/orders/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "Order id")),
    request_body = CreateOrderRequest,
    responses((status = 200, body = OrderView)))]
async fn update_order(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<CreateOrderRequest>,
) -> Result<Json<OrderView>> {
    authz.require(names::ORDERS_CREATE).await?;
    let service = OrderService::new(db);
    let before = service.view(id).await?;
    let after = service.update_draft(id, new_order(req, None)).await?;
    audit.0.updated("scm.order", id, &before, &after).await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/procurement/orders/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "Order id")),
    responses((status = 200, body = OrderView)))]
async fn delete_order(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<OrderView>> {
    authz.require(names::ORDERS_CREATE).await?;
    let view = OrderService::new(db).delete_draft(id).await?;
    audit.0.deleted("scm.order", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/orders/{id}/submit", tag = "procurement",
    params(("id" = Uuid, Path, description = "Order id")),
    responses((status = 200, body = OrderView)))]
async fn submit_order(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Extension(numbering): Extension<Numbering>,
    Path(id): Path<Uuid>,
) -> Result<Json<OrderView>> {
    authz.require(names::ORDERS_SUBMIT).await?;
    let view = OrderService::new(db)
        .submit(id, &numbering, Some(authz.user.id))
        .await?;
    audit
        .0
        .event(format!(
            "submitted purchase order {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/orders/{id}/approve", tag = "procurement",
    params(("id" = Uuid, Path, description = "Order id")),
    request_body = ApproveOrderRequest,
    responses((status = 200, body = OrderView)))]
async fn approve_order(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<ApproveOrderRequest>,
) -> Result<Json<OrderView>> {
    authz.require(names::ORDERS_APPROVE).await?;
    let view = OrderService::new(db)
        .approve(id, req.exchange_rate, Some(authz.user.id))
        .await?;
    audit
        .0
        .event(format!(
            "approved purchase order {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/orders/{id}/cancel", tag = "procurement",
    params(("id" = Uuid, Path, description = "Order id")),
    request_body = CancelOrderRequest,
    responses((status = 200, body = OrderView)))]
async fn cancel_order(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<CancelOrderRequest>,
) -> Result<Json<OrderView>> {
    authz.require(names::ORDERS_CANCEL).await?;
    let view = OrderService::new(db)
        .cancel(id, &req.reason, Some(authz.user.id))
        .await?;
    audit
        .0
        .event(format!(
            "cancelled purchase order {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/orders/{id}/close", tag = "procurement",
    params(("id" = Uuid, Path, description = "Order id")),
    responses((status = 200, body = OrderView)))]
async fn close_order(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<OrderView>> {
    authz.require(names::ORDERS_CANCEL).await?;
    let view = OrderService::new(db).close(id, Some(authz.user.id)).await?;
    audit
        .0
        .event(format!(
            "closed purchase order {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}
