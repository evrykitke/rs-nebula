//! Sales orders: the hub of the order-to-cash cycle.
//!
//! Lifecycle: draft → confirmed (numbered, terms snapshotted, credit
//! checked, stock reserved) → partially_delivered → delivered → closed;
//! cancellation only while nothing posted references the order.
//! Deliveries and invoices maintain the cumulative `delivered_qty` /
//! `billed_qty` counters on the lines — the mirror of procurement's
//! `received_qty` / `billed_qty` — and `reserved_qty` tracks what the
//! order currently holds on the stock levels: confirmation reserves up
//! to free stock (partial reservation stays visible as a shortfall, not
//! silent), deliveries consume it, cancellation and close release it.
//!
//! Credit control happens here, at the commitment point: with a credit
//! limit set, `exposure + this order ≤ limit` or the confirm is blocked —
//! unless the actor holds `Credit.Override`, which is recorded on the
//! order. Exposure today is the remaining value of other open orders;
//! open invoice balances join the sum when invoicing lands.
//!
//! Line prices come from the pricing chain unless the caller supplies
//! one by hand (`Pricing.Override`, floored at the item's minimum
//! selling price); every line records its provenance.

use crate::scm::inventory::item::{ItemType, item};
use crate::scm::inventory::{stock, warehouse};
use crate::scm::sales::customer::customer;
use crate::scm::sales::permissions::names;
use crate::scm::sales::pricing::{PriceQuery, PricingService};
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
    ConnectionTrait, DatabaseConnection, QueryOrder, QuerySelect, Set, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

/// Where a sales order is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = SalesOrderStatus)]
pub enum OrderStatus {
    Draft,
    Confirmed,
    PartiallyDelivered,
    Delivered,
    Closed,
    Cancelled,
}

impl OrderStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            OrderStatus::Draft => "draft",
            OrderStatus::Confirmed => "confirmed",
            OrderStatus::PartiallyDelivered => "partially_delivered",
            OrderStatus::Delivered => "delivered",
            OrderStatus::Closed => "closed",
            OrderStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "draft" => Ok(OrderStatus::Draft),
            "confirmed" => Ok(OrderStatus::Confirmed),
            "partially_delivered" => Ok(OrderStatus::PartiallyDelivered),
            "delivered" => Ok(OrderStatus::Delivered),
            "closed" => Ok(OrderStatus::Closed),
            "cancelled" => Ok(OrderStatus::Cancelled),
            other => Err(Error::internal(format!(
                "unknown sales order status {other:?}"
            ))),
        }
    }

    /// May goods still leave against the order?
    pub fn deliverable(self) -> bool {
        matches!(
            self,
            OrderStatus::Confirmed | OrderStatus::PartiallyDelivered
        )
    }
}

/// The sales order header.
pub mod order {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "sales_orders")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub number: Option<String>,
        pub customer_id: Uuid,
        pub quotation_id: Option<Uuid>,
        pub order_date: Date,
        pub expected_date: Option<Date>,
        pub warehouse_id: Uuid,
        pub shipping_address: Option<String>,
        pub shipping_method: Option<String>,
        pub incoterms: Option<String>,
        pub customer_contact: Option<String>,
        pub customer_po_no: Option<String>,
        pub salesperson_id: Option<Uuid>,
        pub currency: String,
        #[sea_orm(column_type = "Decimal(Some((20, 8)))")]
        pub exchange_rate: Decimal,
        pub price_list_id: Option<Uuid>,
        pub payment_terms_days: i32,
        pub tax_inclusive: bool,
        #[sea_orm(column_type = "Decimal(Some((7, 4)))", nullable)]
        pub discount_pct: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))", nullable)]
        pub discount_amount: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))", nullable)]
        pub other_charges: Option<Decimal>,
        pub memo: Option<String>,
        pub terms_and_conditions: Option<String>,
        pub status: String,
        pub confirmed_at: Option<DateTimeUtc>,
        pub confirmed_by: Option<Uuid>,
        pub credit_override_by: Option<Uuid>,
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

/// One sales order line. `reserved_qty`, `delivered_qty` and `billed_qty`
/// are maintained under the order row lock.
pub mod order_line {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "sales_order_lines")]
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
        pub warehouse_id: Option<Uuid>,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub unit_price: Decimal,
        pub price_source: Option<String>,
        #[sea_orm(column_type = "Decimal(Some((7, 4)))", nullable)]
        pub discount_pct: Option<Decimal>,
        pub tax_code_id: Option<Uuid>,
        pub expected_date: Option<Date>,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub reserved_qty: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub delivered_qty: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub billed_qty: Decimal,
        pub memo: Option<String>,
        pub created_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// A line's price after its own discount, at cost precision — the sales
/// twin of procurement's. Deliveries value nothing with it (COGS is the
/// engine's moving average); billing consistency and the credit exposure
/// compare with it.
pub(crate) fn effective_price(unit_price: Decimal, discount_pct: Option<Decimal>) -> Decimal {
    let pct = discount_pct.unwrap_or(Decimal::ZERO);
    stock::round_cost(unit_price * (Decimal::ONE_HUNDRED - pct) / Decimal::ONE_HUNDRED)
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// An order line as supplied by a caller. `unit_price = None` prices the
/// line through the resolution chain; `Some` is a manual override.
pub struct OrderLineInput {
    pub item_id: Uuid,
    pub description: Option<String>,
    pub qty: Decimal,
    pub warehouse_id: Option<Uuid>,
    pub unit_price: Option<Decimal>,
    pub discount_pct: Option<Decimal>,
    pub tax_code_id: Option<Uuid>,
    pub expected_date: Option<chrono::NaiveDate>,
    pub memo: Option<String>,
}

/// A priced line ready to insert.
pub(crate) struct PricedLine {
    pub item_id: Uuid,
    pub description: Option<String>,
    pub qty: Decimal,
    pub warehouse_id: Option<Uuid>,
    pub unit_price: Decimal,
    pub price_source: Option<String>,
    pub discount_pct: Option<Decimal>,
    pub tax_code_id: Option<Uuid>,
    pub expected_date: Option<chrono::NaiveDate>,
    pub memo: Option<String>,
}

/// A new draft sales order as supplied by a caller.
pub struct NewOrder {
    pub customer_id: Uuid,
    pub order_date: chrono::NaiveDate,
    pub expected_date: Option<chrono::NaiveDate>,
    pub warehouse_id: Uuid,
    pub shipping_address: Option<String>,
    pub shipping_method: Option<String>,
    pub incoterms: Option<String>,
    pub customer_contact: Option<String>,
    pub customer_po_no: Option<String>,
    pub currency: Option<String>,
    pub payment_terms_days: Option<i32>,
    pub tax_inclusive: bool,
    pub discount_pct: Option<Decimal>,
    pub discount_amount: Option<Decimal>,
    pub other_charges: Option<Decimal>,
    pub memo: Option<String>,
    pub terms_and_conditions: Option<String>,
    pub lines: Vec<OrderLineInput>,
    pub created_by: Option<Uuid>,
    /// The actor holds `Pricing.Override` (checked by the handler).
    pub allow_price_override: bool,
}

/// The sales order service over one (tenant) connection.
pub struct OrderService {
    db: DatabaseConnection,
}

impl OrderService {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Create a draft order. Currency and payment terms default from the
    /// customer; the customer must be active and not on hold; lines are
    /// priced through the chain unless overridden by hand.
    pub async fn create_draft(&self, new: NewOrder) -> Result<OrderView> {
        let buyer = load_customer_for_new_order(&self.db, new.customer_id).await?;
        let currency = match &new.currency {
            Some(c) => validate_currency(c)?,
            None => buyer.currency.clone(),
        };
        validate_order(&self.db, &new).await?;
        let priced = self.price_lines(&new, &buyer, &currency).await?;

        let order_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        let txn = self.db.begin().await?;
        order::ActiveModel {
            id: Set(order_id),
            number: Set(None),
            customer_id: Set(new.customer_id),
            quotation_id: Set(None),
            order_date: Set(new.order_date),
            expected_date: Set(new.expected_date),
            warehouse_id: Set(new.warehouse_id),
            shipping_address: Set(clean(new.shipping_address)),
            shipping_method: Set(clean(new.shipping_method)),
            incoterms: Set(clean(new.incoterms).or(buyer.incoterms.clone())),
            customer_contact: Set(clean(new.customer_contact)),
            customer_po_no: Set(clean(new.customer_po_no)),
            salesperson_id: Set(buyer.salesperson_id),
            currency: Set(currency),
            exchange_rate: Set(Decimal::ONE),
            price_list_id: Set(buyer.price_list_id),
            payment_terms_days: Set(new.payment_terms_days.unwrap_or(buyer.payment_terms_days)),
            tax_inclusive: Set(new.tax_inclusive),
            discount_pct: Set(new.discount_pct),
            discount_amount: Set(new.discount_amount),
            other_charges: Set(new.other_charges),
            memo: Set(clean(new.memo)),
            terms_and_conditions: Set(clean(new.terms_and_conditions)),
            status: Set(OrderStatus::Draft.as_str().to_string()),
            confirmed_at: Set(None),
            confirmed_by: Set(None),
            credit_override_by: Set(None),
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
        insert_lines(&txn, order_id, &priced).await?;
        txn.commit().await?;
        self.view(order_id).await
    }

    /// Replace a draft's header and lines wholesale (lines re-price).
    pub async fn update_draft(
        &self,
        id: Uuid,
        new: NewOrder,
        by: Option<Uuid>,
    ) -> Result<OrderView> {
        let buyer = load_customer_for_new_order(&self.db, new.customer_id).await?;
        let currency = match &new.currency {
            Some(c) => validate_currency(c)?,
            None => buyer.currency.clone(),
        };
        validate_order(&self.db, &new).await?;
        let priced = self.price_lines(&new, &buyer, &currency).await?;

        let txn = self.db.begin().await?;
        let existing = load_order_locked(&txn, id).await?;
        if OrderStatus::parse(&existing.status)? != OrderStatus::Draft {
            return Err(Error::Validation("only a draft order can be edited".into()));
        }
        order_line::Entity::delete_many()
            .filter(order_line::Column::OrderId.eq(id))
            .exec(&txn)
            .await?;
        insert_lines(&txn, id, &priced).await?;

        let mut active: order::ActiveModel = existing.into();
        active.customer_id = Set(new.customer_id);
        active.order_date = Set(new.order_date);
        active.expected_date = Set(new.expected_date);
        active.warehouse_id = Set(new.warehouse_id);
        active.shipping_address = Set(clean(new.shipping_address));
        active.shipping_method = Set(clean(new.shipping_method));
        active.incoterms = Set(clean(new.incoterms).or(buyer.incoterms.clone()));
        active.customer_contact = Set(clean(new.customer_contact));
        active.customer_po_no = Set(clean(new.customer_po_no));
        active.salesperson_id = Set(buyer.salesperson_id);
        active.currency = Set(currency);
        active.price_list_id = Set(buyer.price_list_id);
        active.payment_terms_days = Set(new.payment_terms_days.unwrap_or(buyer.payment_terms_days));
        active.tax_inclusive = Set(new.tax_inclusive);
        active.discount_pct = Set(new.discount_pct);
        active.discount_amount = Set(new.discount_amount);
        active.other_charges = Set(new.other_charges);
        active.memo = Set(clean(new.memo));
        active.terms_and_conditions = Set(clean(new.terms_and_conditions));
        active.updated_at = Set(chrono::Utc::now());
        active.updated_by = Set(by);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Delete a draft (lines cascade). Anything confirmed is history.
    pub async fn delete_draft(&self, id: Uuid) -> Result<OrderView> {
        let view = self.view(id).await?;
        let txn = self.db.begin().await?;
        let existing = load_order_locked(&txn, id).await?;
        if OrderStatus::parse(&existing.status)? != OrderStatus::Draft {
            return Err(Error::Validation(
                "only a draft order can be deleted".into(),
            ));
        }
        order::Entity::delete_by_id(id).exec(&txn).await?;
        txn.commit().await?;
        Ok(view)
    }

    /// Confirm a draft: the commitment point. Re-checks the customer
    /// hold, runs the credit check, captures the exchange rate, allocates
    /// the SO number and reserves stock up to what is free — partial
    /// reservation is recorded per line, never silent.
    pub async fn confirm(
        &self,
        id: Uuid,
        exchange_rate: Option<Decimal>,
        by: Option<Uuid>,
        credit_override: bool,
        numbering: &Numbering,
    ) -> Result<OrderView> {
        if exchange_rate.is_some_and(|r| r <= Decimal::ZERO) {
            return Err(Error::Validation("exchange rate must be positive".into()));
        }
        let txn = self.db.begin().await?;
        let existing = load_order_locked(&txn, id).await?;
        if OrderStatus::parse(&existing.status)? != OrderStatus::Draft {
            return Err(Error::Validation(
                "only a draft order can be confirmed".into(),
            ));
        }
        let buyer = load_customer_for_new_order(&txn, existing.customer_id).await?;
        let lines = load_lines(&txn, id).await?;
        if lines.is_empty() {
            return Err(Error::Validation("an order needs at least one line".into()));
        }
        let rate = exchange_rate.unwrap_or(existing.exchange_rate);

        // Credit check at the commitment point.
        let overridden = match buyer.credit_limit {
            None => false,
            Some(limit) => {
                let this_order = stock::round_money(order_value(&existing, &lines) * rate);
                let exposure = customer_exposure(&txn, buyer.id, Some(id)).await?;
                if exposure + this_order > limit {
                    if !credit_override {
                        return Err(Error::Validation(format!(
                            "confirming would put {} at {} against a credit limit of {} \
                             (current exposure {}); an override needs the credit-override \
                             permission",
                            buyer.code,
                            exposure + this_order,
                            limit,
                            exposure
                        )));
                    }
                    true
                } else {
                    false
                }
            }
        };

        // Reserve stock for the stockable lines, aggregated per
        // item × warehouse and locked in ascending order so concurrent
        // confirms serialize instead of deadlocking.
        let item_ids: Vec<Uuid> = lines.iter().map(|l| l.item_id).collect();
        let items: HashMap<Uuid, item::Model> = item::Entity::find()
            .filter(item::Column::Id.is_in(item_ids))
            .all(&txn)
            .await?
            .into_iter()
            .map(|i| (i.id, i))
            .collect();
        let mut wants: BTreeMap<(Uuid, Uuid), Decimal> = BTreeMap::new();
        for l in &lines {
            let Some(it) = items.get(&l.item_id) else {
                return Err(Error::NotFound(format!("item {}", l.item_id)));
            };
            if ItemType::parse(&it.item_type)? != ItemType::Stockable {
                continue;
            }
            let wh = l.warehouse_id.unwrap_or(existing.warehouse_id);
            *wants.entry((l.item_id, wh)).or_default() += l.qty;
        }
        let mut granted: HashMap<(Uuid, Uuid), Decimal> = HashMap::new();
        for ((item_id, wh), want) in &wants {
            let got = stock::StockService::reserve_up_to(&txn, *item_id, *wh, *want).await?;
            granted.insert((*item_id, *wh), got);
        }
        // Distribute each grant across its lines in line order.
        for l in &lines {
            let Some(it) = items.get(&l.item_id) else {
                continue;
            };
            if ItemType::parse(&it.item_type)? != ItemType::Stockable {
                continue;
            }
            let wh = l.warehouse_id.unwrap_or(existing.warehouse_id);
            let pool = granted.entry((l.item_id, wh)).or_default();
            let take = (*pool).min(l.qty);
            if take > Decimal::ZERO {
                *pool -= take;
                let mut active: order_line::ActiveModel = l.clone().into();
                active.reserved_qty = Set(take);
                active.update(&txn).await?;
            }
        }

        let number = numbering.next(&txn, crate::scm::SALES_ORDER_SERIES).await?;
        let now = chrono::Utc::now();
        let mut active: order::ActiveModel = existing.into();
        active.number = Set(Some(number.formatted));
        active.exchange_rate = Set(rate);
        active.status = Set(OrderStatus::Confirmed.as_str().to_string());
        active.confirmed_at = Set(Some(now));
        active.confirmed_by = Set(by);
        if overridden {
            active.credit_override_by = Set(by);
        }
        active.updated_at = Set(now);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Retry reservation for the shortfall on an open order — called
    /// after stock arrives (manually now; a receipt-driven job later).
    pub async fn reserve_more(&self, id: Uuid) -> Result<OrderView> {
        let txn = self.db.begin().await?;
        let existing = load_order_locked(&txn, id).await?;
        if !OrderStatus::parse(&existing.status)?.deliverable() {
            return Err(Error::Validation(
                "only an open confirmed order can reserve more stock".into(),
            ));
        }
        let lines = load_lines(&txn, id).await?;
        let item_ids: Vec<Uuid> = lines.iter().map(|l| l.item_id).collect();
        let items: HashMap<Uuid, item::Model> = item::Entity::find()
            .filter(item::Column::Id.is_in(item_ids))
            .all(&txn)
            .await?
            .into_iter()
            .map(|i| (i.id, i))
            .collect();
        let mut wants: BTreeMap<(Uuid, Uuid), Decimal> = BTreeMap::new();
        for l in &lines {
            let Some(it) = items.get(&l.item_id) else {
                continue;
            };
            if ItemType::parse(&it.item_type)? != ItemType::Stockable {
                continue;
            }
            let short = l.qty - l.delivered_qty - l.reserved_qty;
            if short > Decimal::ZERO {
                let wh = l.warehouse_id.unwrap_or(existing.warehouse_id);
                *wants.entry((l.item_id, wh)).or_default() += short;
            }
        }
        let mut granted: HashMap<(Uuid, Uuid), Decimal> = HashMap::new();
        for ((item_id, wh), want) in &wants {
            let got = stock::StockService::reserve_up_to(&txn, *item_id, *wh, *want).await?;
            granted.insert((*item_id, *wh), got);
        }
        for l in &lines {
            let Some(it) = items.get(&l.item_id) else {
                continue;
            };
            if ItemType::parse(&it.item_type)? != ItemType::Stockable {
                continue;
            }
            let short = l.qty - l.delivered_qty - l.reserved_qty;
            if short <= Decimal::ZERO {
                continue;
            }
            let wh = l.warehouse_id.unwrap_or(existing.warehouse_id);
            let pool = granted.entry((l.item_id, wh)).or_default();
            let take = (*pool).min(short);
            if take > Decimal::ZERO {
                *pool -= take;
                let mut active: order_line::ActiveModel = l.clone().into();
                active.reserved_qty = Set(l.reserved_qty + take);
                active.update(&txn).await?;
            }
        }
        let mut active: order::ActiveModel = existing.into();
        active.updated_at = Set(chrono::Utc::now());
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Cancel an order nothing posted references yet; releases whatever
    /// it holds reserved. (Posted deliveries move the status past
    /// `confirmed`, so the status gate already excludes them; the
    /// explicit document checks join with the delivery and invoice
    /// phases.)
    pub async fn cancel(&self, id: Uuid, reason: &str, by: Option<Uuid>) -> Result<OrderView> {
        let txn = self.db.begin().await?;
        let existing = load_order_locked(&txn, id).await?;
        let status = OrderStatus::parse(&existing.status)?;
        if !matches!(status, OrderStatus::Draft | OrderStatus::Confirmed) {
            return Err(Error::Validation(format!(
                "a {} order cannot be cancelled",
                status.as_str()
            )));
        }
        release_reservations(&txn, &existing).await?;
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
    /// the reservation it still holds. Billing what was delivered stays
    /// possible.
    pub async fn close(&self, id: Uuid, by: Option<Uuid>) -> Result<OrderView> {
        let txn = self.db.begin().await?;
        let existing = load_order_locked(&txn, id).await?;
        let status = OrderStatus::parse(&existing.status)?;
        if !matches!(
            status,
            OrderStatus::Confirmed | OrderStatus::PartiallyDelivered | OrderStatus::Delivered
        ) {
            return Err(Error::Validation(format!(
                "a {} order cannot be closed",
                status.as_str()
            )));
        }
        release_reservations(&txn, &existing).await?;
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
        if let Some(customer_id) = filter.customer_id {
            query = query.filter(order::Column::CustomerId.eq(customer_id));
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
                Ok(OrderHeader {
                    id: r.id,
                    number: r.number.clone(),
                    customer_id: r.customer_id,
                    customer_name: customers
                        .get(&r.customer_id)
                        .map(|c| c.name.clone())
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
        let buyer = customer::Entity::find_by_id(row.customer_id)
            .one(&self.db)
            .await?;
        let wh = warehouse::Entity::find_by_id(row.warehouse_id)
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
                let it = items.get(&l.item_id);
                let price = effective_price(l.unit_price, l.discount_pct);
                let net = stock::round_money(l.qty * price);
                subtotal += net;
                OrderLineView {
                    id: l.id,
                    line_no: l.line_no,
                    item_id: l.item_id,
                    sku: it.map(|i| i.sku.clone()).unwrap_or_default(),
                    item_name: it.map(|i| i.name.clone()).unwrap_or_default(),
                    description: l.description,
                    qty: l.qty,
                    warehouse_id: l.warehouse_id,
                    unit_price: l.unit_price,
                    price_source: l.price_source,
                    discount_pct: l.discount_pct,
                    effective_price: price,
                    net,
                    reserved_qty: l.reserved_qty,
                    delivered_qty: l.delivered_qty,
                    billed_qty: l.billed_qty,
                    expected_date: l.expected_date,
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

        Ok(OrderView {
            id: row.id,
            number: row.number,
            customer_id: row.customer_id,
            customer_name: buyer.map(|c| c.name).unwrap_or_default(),
            quotation_id: row.quotation_id,
            order_date: row.order_date,
            expected_date: row.expected_date,
            warehouse_id: row.warehouse_id,
            warehouse_code: wh.map(|w| w.code).unwrap_or_default(),
            shipping_address: row.shipping_address,
            shipping_method: row.shipping_method,
            incoterms: row.incoterms,
            customer_contact: row.customer_contact,
            customer_po_no: row.customer_po_no,
            salesperson_id: row.salesperson_id,
            currency: row.currency,
            exchange_rate: row.exchange_rate,
            payment_terms_days: row.payment_terms_days,
            tax_inclusive: row.tax_inclusive,
            discount_pct: row.discount_pct,
            discount_amount: row.discount_amount,
            other_charges: row.other_charges,
            memo: row.memo,
            terms_and_conditions: row.terms_and_conditions,
            status: OrderStatus::parse(&row.status)?,
            credit_override_by: row.credit_override_by,
            cancel_reason: row.cancel_reason,
            subtotal,
            total,
            created_at: row.created_at,
            lines: line_views,
        })
    }

    /// Price every line of a new order, resolving where no manual price
    /// was given.
    async fn price_lines(
        &self,
        new: &NewOrder,
        buyer: &customer::Model,
        currency: &str,
    ) -> Result<Vec<PricedLine>> {
        let pricing = PricingService::new(self.db.clone());
        let mut priced = Vec::with_capacity(new.lines.len());
        for l in &new.lines {
            let (unit_price, source) = pricing
                .price_line(
                    PriceQuery {
                        customer_id: buyer.id,
                        item_id: l.item_id,
                        qty: l.qty,
                        uom_id: None,
                        currency: currency.to_string(),
                        date: new.order_date,
                    },
                    l.unit_price,
                    new.allow_price_override,
                )
                .await?;
            priced.push(PricedLine {
                item_id: l.item_id,
                description: l.description.clone(),
                qty: l.qty,
                warehouse_id: l.warehouse_id,
                unit_price,
                price_source: Some(source.as_string()),
                discount_pct: l.discount_pct,
                tax_code_id: l.tax_code_id,
                expected_date: l.expected_date,
                memo: l.memo.clone(),
            });
        }
        Ok(priced)
    }
}

/// The order's own value (line nets, header discounts, charges) in the
/// order currency — the amount the credit check commits.
pub(crate) fn order_value(row: &order::Model, lines: &[order_line::Model]) -> Decimal {
    let mut subtotal = Decimal::ZERO;
    for l in lines {
        subtotal += stock::round_money(l.qty * effective_price(l.unit_price, l.discount_pct));
    }
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

/// The customer's current credit exposure in base currency: the
/// undelivered remainder of every other open confirmed order, valued at
/// its own rate. Open invoice balances and delivered-not-billed value
/// join this sum when the invoicing phase lands.
pub(crate) async fn customer_exposure(
    txn: &sea_orm::DatabaseTransaction,
    customer_id: Uuid,
    excluding_order: Option<Uuid>,
) -> Result<Decimal> {
    let mut query = order::Entity::find()
        .filter(order::Column::CustomerId.eq(customer_id))
        .filter(order::Column::Status.is_in([
            OrderStatus::Confirmed.as_str(),
            OrderStatus::PartiallyDelivered.as_str(),
        ]));
    if let Some(id) = excluding_order {
        query = query.filter(order::Column::Id.ne(id));
    }
    let orders = query.all(txn).await?;
    let mut exposure = Decimal::ZERO;
    for o in orders {
        let lines = load_lines(txn, o.id).await?;
        let mut remaining = Decimal::ZERO;
        for l in &lines {
            let undelivered = (l.qty - l.delivered_qty).max(Decimal::ZERO);
            remaining +=
                stock::round_money(undelivered * effective_price(l.unit_price, l.discount_pct));
        }
        exposure += stock::round_money(remaining * o.exchange_rate);
    }
    Ok(exposure)
}

/// Release every reservation the order still holds, aggregated per
/// item × warehouse in ascending lock order, and zero the line state.
async fn release_reservations(
    txn: &sea_orm::DatabaseTransaction,
    row: &order::Model,
) -> Result<()> {
    let lines = load_lines(txn, row.id).await?;
    let mut held: BTreeMap<(Uuid, Uuid), Decimal> = BTreeMap::new();
    for l in &lines {
        if l.reserved_qty > Decimal::ZERO {
            let wh = l.warehouse_id.unwrap_or(row.warehouse_id);
            *held.entry((l.item_id, wh)).or_default() += l.reserved_qty;
        }
    }
    for ((item_id, wh), qty) in &held {
        stock::StockService::release_reserved(txn, *item_id, *wh, *qty).await?;
    }
    for l in lines {
        if l.reserved_qty > Decimal::ZERO {
            let mut active: order_line::ActiveModel = l.into();
            active.reserved_qty = Set(Decimal::ZERO);
            active.update(txn).await?;
        }
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

/// The customer a new order commits to: must exist, be active, and not
/// be on hold.
pub(crate) async fn load_customer_for_new_order<C: ConnectionTrait>(
    conn: &C,
    customer_id: Uuid,
) -> Result<customer::Model> {
    let found = customer::Entity::find_by_id(customer_id).one(conn).await?;
    let Some(found) = found else {
        return Err(Error::NotFound(format!("customer {customer_id}")));
    };
    if !found.is_active {
        return Err(Error::Validation(format!(
            "customer {} is inactive",
            found.code
        )));
    }
    if found.on_hold {
        return Err(Error::Validation(format!(
            "customer {} is on hold{}",
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

/// Draft-time validation of everything but the customer (checked where
/// the order is written, so the hold message can name the code).
async fn validate_order<C: ConnectionTrait>(conn: &C, new: &NewOrder) -> Result<()> {
    if new.lines.is_empty() {
        return Err(Error::Validation("an order needs at least one line".into()));
    }
    let mut warehouse_ids: Vec<Uuid> = vec![new.warehouse_id];
    warehouse_ids.extend(new.lines.iter().filter_map(|l| l.warehouse_id));
    warehouse_ids.sort();
    warehouse_ids.dedup();
    let warehouses: HashMap<Uuid, warehouse::Model> = warehouse::Entity::find()
        .filter(warehouse::Column::Id.is_in(warehouse_ids.clone()))
        .all(conn)
        .await?
        .into_iter()
        .map(|w| (w.id, w))
        .collect();
    for wh_id in &warehouse_ids {
        match warehouses.get(wh_id) {
            Some(w) if w.is_active => {}
            Some(w) => {
                return Err(Error::Validation(format!(
                    "warehouse {} is inactive",
                    w.code
                )));
            }
            None => {
                return Err(Error::Validation(format!(
                    "warehouse {wh_id} does not exist"
                )));
            }
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
        return Err(Error::Validation(
            "payment terms must not be negative".into(),
        ));
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
        let Some(it) = items.get(&l.item_id) else {
            return Err(Error::NotFound(format!("item {}", l.item_id)));
        };
        if !it.is_active {
            return Err(Error::Validation(format!(
                "line {line_no}: item {} is inactive",
                it.sku
            )));
        }
        if !it.is_sellable {
            return Err(Error::Validation(format!(
                "line {line_no}: item {} is not sellable",
                it.sku
            )));
        }
        if l.qty <= Decimal::ZERO {
            return Err(Error::Validation(format!(
                "line {line_no}: quantity must be positive"
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

/// Insert priced lines, numbered from one. Lines are in the item's stock
/// UoM (`uom_id` stays NULL until UoM conversions land).
pub(crate) async fn insert_lines<C: ConnectionTrait>(
    conn: &C,
    order_id: Uuid,
    lines: &[PricedLine],
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
            warehouse_id: Set(l.warehouse_id),
            unit_price: Set(l.unit_price),
            price_source: Set(l.price_source.clone()),
            discount_pct: Set(l.discount_pct),
            tax_code_id: Set(l.tax_code_id),
            expected_date: Set(l.expected_date),
            reserved_qty: Set(Decimal::ZERO),
            delivered_qty: Set(Decimal::ZERO),
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
        .ok_or_else(|| Error::NotFound(format!("sales order {id}")))
}

/// Load an order holding its row lock — the serialization point for
/// everything that mutates its counters (deliveries, invoices,
/// transitions).
pub(crate) async fn load_order_locked(
    txn: &sea_orm::DatabaseTransaction,
    id: Uuid,
) -> Result<order::Model> {
    order::Entity::find_by_id(id)
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("sales order {id}")))
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
/// moves between confirmed / partially_delivered / delivered.
pub(crate) async fn recompute_status(
    txn: &sea_orm::DatabaseTransaction,
    row: order::Model,
) -> Result<()> {
    let status = OrderStatus::parse(&row.status)?;
    if !matches!(
        status,
        OrderStatus::Confirmed | OrderStatus::PartiallyDelivered | OrderStatus::Delivered
    ) {
        return Ok(());
    }
    let lines = load_lines(txn, row.id).await?;
    let any_delivered = lines.iter().any(|l| l.delivered_qty > Decimal::ZERO);
    let all_delivered = lines.iter().all(|l| l.delivered_qty >= l.qty);
    let next = if all_delivered {
        OrderStatus::Delivered
    } else if any_delivered {
        OrderStatus::PartiallyDelivered
    } else {
        OrderStatus::Confirmed
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

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = SalesOrderLineView)]
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
    /// NULL = the header's warehouse.
    pub warehouse_id: Option<Uuid>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub unit_price: Decimal,
    /// Where the price came from: `list:{uuid}` | `item_default` | `manual`.
    pub price_source: Option<String>,
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub discount_pct: Option<Decimal>,
    /// Unit price after the line discount.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub effective_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub net: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub reserved_qty: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub delivered_qty: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub billed_qty: Decimal,
    #[schema(value_type = Option<String>, format = Date)]
    pub expected_date: Option<chrono::NaiveDate>,
    pub memo: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = SalesOrderView)]
pub struct OrderView {
    pub id: Uuid,
    pub number: Option<String>,
    pub customer_id: Uuid,
    pub customer_name: String,
    pub quotation_id: Option<Uuid>,
    #[schema(value_type = String, format = Date)]
    pub order_date: chrono::NaiveDate,
    #[schema(value_type = Option<String>, format = Date)]
    pub expected_date: Option<chrono::NaiveDate>,
    pub warehouse_id: Uuid,
    pub warehouse_code: String,
    pub shipping_address: Option<String>,
    pub shipping_method: Option<String>,
    pub incoterms: Option<String>,
    pub customer_contact: Option<String>,
    pub customer_po_no: Option<String>,
    pub salesperson_id: Option<Uuid>,
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
    pub terms_and_conditions: Option<String>,
    pub status: OrderStatus,
    pub credit_override_by: Option<Uuid>,
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
#[schema(as = SalesOrderHeader)]
pub struct OrderHeader {
    pub id: Uuid,
    pub number: Option<String>,
    pub customer_id: Uuid,
    pub customer_name: String,
    #[schema(value_type = String, format = Date)]
    pub order_date: chrono::NaiveDate,
    #[schema(value_type = Option<String>, format = Date)]
    pub expected_date: Option<chrono::NaiveDate>,
    pub currency: String,
    pub status: OrderStatus,
}

pub struct OrderFilter {
    pub status: Option<OrderStatus>,
    pub customer_id: Option<Uuid>,
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = SalesOrderLineRequest)]
pub struct OrderLineRequest {
    pub item_id: Uuid,
    pub description: Option<String>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    /// Fulfil this line from another warehouse than the header's.
    pub warehouse_id: Option<Uuid>,
    /// Omit to price through the chain; supplying one is a manual
    /// override and needs `Pricing.Override`.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub unit_price: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub discount_pct: Option<Decimal>,
    pub tax_code_id: Option<Uuid>,
    #[schema(value_type = Option<String>, format = Date)]
    pub expected_date: Option<chrono::NaiveDate>,
    pub memo: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = CreateSalesOrderRequest)]
pub struct CreateOrderRequest {
    pub customer_id: Uuid,
    #[schema(value_type = String, format = Date)]
    pub order_date: chrono::NaiveDate,
    #[schema(value_type = Option<String>, format = Date)]
    pub expected_date: Option<chrono::NaiveDate>,
    pub warehouse_id: Uuid,
    pub shipping_address: Option<String>,
    pub shipping_method: Option<String>,
    pub incoterms: Option<String>,
    pub customer_contact: Option<String>,
    pub customer_po_no: Option<String>,
    /// Defaults to the customer's currency.
    pub currency: Option<String>,
    /// Defaults to the customer's terms.
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
    pub terms_and_conditions: Option<String>,
    pub lines: Vec<OrderLineRequest>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ConfirmOrderRequest {
    /// Required when the order currency differs from the base currency.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub exchange_rate: Option<Decimal>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = CancelSalesOrderRequest)]
pub struct CancelOrderRequest {
    #[serde(default)]
    pub reason: String,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListOrdersQuery {
    pub status: Option<OrderStatus>,
    pub customer_id: Option<Uuid>,
    /// Order date range, inclusive.
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

fn new_order(req: CreateOrderRequest, created_by: Option<Uuid>, allow_override: bool) -> NewOrder {
    NewOrder {
        customer_id: req.customer_id,
        order_date: req.order_date,
        expected_date: req.expected_date,
        warehouse_id: req.warehouse_id,
        shipping_address: req.shipping_address,
        shipping_method: req.shipping_method,
        incoterms: req.incoterms,
        customer_contact: req.customer_contact,
        customer_po_no: req.customer_po_no,
        currency: req.currency,
        payment_terms_days: req.payment_terms_days,
        tax_inclusive: req.tax_inclusive,
        discount_pct: req.discount_pct,
        discount_amount: req.discount_amount,
        other_charges: req.other_charges,
        memo: req.memo,
        terms_and_conditions: req.terms_and_conditions,
        lines: req
            .lines
            .into_iter()
            .map(|l| OrderLineInput {
                item_id: l.item_id,
                description: l.description,
                qty: l.qty,
                warehouse_id: l.warehouse_id,
                unit_price: l.unit_price,
                discount_pct: l.discount_pct,
                tax_code_id: l.tax_code_id,
                expected_date: l.expected_date,
                memo: l.memo,
            })
            .collect(),
        created_by,
        allow_price_override: allow_override,
    }
}

pub(crate) fn routes() -> Router {
    Router::new()
        .route("/sales/orders", get(list_orders).post(create_order))
        .route(
            "/sales/orders/{id}",
            get(get_order).put(update_order).delete(delete_order),
        )
        .route("/sales/orders/{id}/confirm", post(confirm_order))
        .route("/sales/orders/{id}/cancel", post(cancel_order))
        .route("/sales/orders/{id}/close", post(close_order))
        .route("/sales/orders/{id}/reserve", post(reserve_order))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        list_orders,
        get_order,
        create_order,
        update_order,
        delete_order,
        confirm_order,
        cancel_order,
        close_order,
        reserve_order
    ),
    components(schemas(OrderStatus))
)]
struct ApiDoc;

#[utoipa::path(get, path = "/sales/orders", tag = "sales",
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
            customer_id: q.customer_id,
            from: q.from,
            to: q.to,
        })
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/sales/orders/{id}", tag = "sales",
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

#[utoipa::path(post, path = "/sales/orders", tag = "sales",
    request_body = CreateOrderRequest,
    responses((status = 200, body = OrderView)))]
async fn create_order(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(req): Json<CreateOrderRequest>,
) -> Result<Json<OrderView>> {
    authz.require(names::ORDERS_CREATE).await?;
    let allow_override = authz.require(names::PRICING_OVERRIDE).await.is_ok();
    let view = OrderService::new(db)
        .create_draft(new_order(req, Some(authz.user.id), allow_override))
        .await?;
    audit.0.created("scm.sales_order", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(put, path = "/sales/orders/{id}", tag = "sales",
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
    let allow_override = authz.require(names::PRICING_OVERRIDE).await.is_ok();
    let service = OrderService::new(db);
    let before = service.view(id).await?;
    let after = service
        .update_draft(
            id,
            new_order(req, None, allow_override),
            Some(authz.user.id),
        )
        .await?;
    audit
        .0
        .updated("scm.sales_order", id, &before, &after)
        .await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/sales/orders/{id}", tag = "sales",
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
    audit.0.deleted("scm.sales_order", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/sales/orders/{id}/confirm", tag = "sales",
    params(("id" = Uuid, Path, description = "Order id")),
    request_body = ConfirmOrderRequest,
    responses((status = 200, body = OrderView)))]
async fn confirm_order(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Extension(numbering): Extension<Numbering>,
    Path(id): Path<Uuid>,
    Json(req): Json<ConfirmOrderRequest>,
) -> Result<Json<OrderView>> {
    authz.require(names::ORDERS_CONFIRM).await?;
    let credit_override = authz.require(names::CREDIT_OVERRIDE).await.is_ok();
    let view = OrderService::new(db)
        .confirm(
            id,
            req.exchange_rate,
            Some(authz.user.id),
            credit_override,
            &numbering,
        )
        .await?;
    audit
        .0
        .event(format!(
            "confirmed sales order {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/sales/orders/{id}/cancel", tag = "sales",
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
            "cancelled sales order {}",
            view.number.as_deref().unwrap_or(&view.id.to_string())
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/sales/orders/{id}/close", tag = "sales",
    params(("id" = Uuid, Path, description = "Order id")),
    responses((status = 200, body = OrderView)))]
async fn close_order(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<OrderView>> {
    authz.require(names::ORDERS_CLOSE).await?;
    let view = OrderService::new(db).close(id, Some(authz.user.id)).await?;
    audit
        .0
        .event(format!(
            "closed sales order {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/sales/orders/{id}/reserve", tag = "sales",
    params(("id" = Uuid, Path, description = "Order id")),
    responses((status = 200, body = OrderView)))]
async fn reserve_order(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<OrderView>> {
    authz.require(names::ORDERS_CONFIRM).await?;
    let view = OrderService::new(db).reserve_more(id).await?;
    audit
        .0
        .event(format!(
            "retried reservation on sales order {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}
