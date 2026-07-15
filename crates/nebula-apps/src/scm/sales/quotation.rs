//! Quotations: an offer to a customer — no reservations, no credit
//! effects, no stock.
//!
//! Lifecycle: draft → sent (numbered from `sales.quotation`) → accepted |
//! declined | expired, and accepted → converted once a sales order is cut
//! from it. Conversion copies the lines *with their quoted prices and
//! provenance* — the customer accepted those numbers, so they do not
//! re-price — onto a draft sales order that then walks the normal
//! confirm path (credit check, reservation) like any other.

use crate::scm::inventory::item::item;
use crate::scm::inventory::stock;
use crate::scm::sales::customer::customer;
use crate::scm::sales::order::{self, PricedLine};
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
use sea_orm::{ConnectionTrait, DatabaseConnection, QueryOrder, QuerySelect, Set, TransactionTrait};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Where a quotation is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuotationStatus {
    Draft,
    Sent,
    Accepted,
    Declined,
    Expired,
    Converted,
}

impl QuotationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            QuotationStatus::Draft => "draft",
            QuotationStatus::Sent => "sent",
            QuotationStatus::Accepted => "accepted",
            QuotationStatus::Declined => "declined",
            QuotationStatus::Expired => "expired",
            QuotationStatus::Converted => "converted",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "draft" => Ok(QuotationStatus::Draft),
            "sent" => Ok(QuotationStatus::Sent),
            "accepted" => Ok(QuotationStatus::Accepted),
            "declined" => Ok(QuotationStatus::Declined),
            "expired" => Ok(QuotationStatus::Expired),
            "converted" => Ok(QuotationStatus::Converted),
            other => Err(Error::internal(format!(
                "unknown quotation status {other:?}"
            ))),
        }
    }
}

/// The quotation header.
pub mod quotation {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "sales_quotations")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub number: Option<String>,
        pub customer_id: Uuid,
        pub quote_date: Date,
        pub valid_until: Option<Date>,
        pub currency: String,
        pub price_list_id: Option<Uuid>,
        pub tax_inclusive: bool,
        #[sea_orm(column_type = "Decimal(Some((7, 4)))", nullable)]
        pub discount_pct: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))", nullable)]
        pub discount_amount: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))", nullable)]
        pub other_charges: Option<Decimal>,
        pub customer_contact: Option<String>,
        pub salesperson_id: Option<Uuid>,
        pub memo: Option<String>,
        pub reference: Option<String>,
        pub terms_and_conditions: Option<String>,
        pub status: String,
        pub sent_at: Option<DateTimeUtc>,
        pub sent_by: Option<Uuid>,
        pub resolved_at: Option<DateTimeUtc>,
        pub decline_reason: Option<String>,
        pub converted_to_id: Option<Uuid>,
        pub created_at: DateTimeUtc,
        pub created_by: Option<Uuid>,
        pub updated_at: DateTimeUtc,
        pub updated_by: Option<Uuid>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// One quotation line.
pub mod quotation_line {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "sales_quotation_lines")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub quotation_id: Uuid,
        pub line_no: i32,
        pub item_id: Uuid,
        pub description: Option<String>,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub qty: Decimal,
        pub uom_id: Option<Uuid>,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub unit_price: Decimal,
        pub price_source: Option<String>,
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

/// A quotation line as supplied by a caller. `unit_price = None` prices
/// through the chain; `Some` is a manual override.
pub struct QuotationLineInput {
    pub item_id: Uuid,
    pub description: Option<String>,
    pub qty: Decimal,
    pub unit_price: Option<Decimal>,
    pub discount_pct: Option<Decimal>,
    pub tax_code_id: Option<Uuid>,
    pub memo: Option<String>,
}

/// A new draft quotation as supplied by a caller.
pub struct NewQuotation {
    pub customer_id: Uuid,
    pub quote_date: chrono::NaiveDate,
    pub valid_until: Option<chrono::NaiveDate>,
    pub currency: Option<String>,
    pub tax_inclusive: bool,
    pub discount_pct: Option<Decimal>,
    pub discount_amount: Option<Decimal>,
    pub other_charges: Option<Decimal>,
    pub customer_contact: Option<String>,
    pub memo: Option<String>,
    pub reference: Option<String>,
    pub terms_and_conditions: Option<String>,
    pub lines: Vec<QuotationLineInput>,
    pub created_by: Option<Uuid>,
    /// The actor holds `Pricing.Override` (checked by the handler).
    pub allow_price_override: bool,
}

/// The quotation service over one (tenant) connection.
pub struct QuotationService {
    db: DatabaseConnection,
}

impl QuotationService {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn create_draft(&self, new: NewQuotation) -> Result<QuotationView> {
        let buyer = order::load_customer_for_new_order(&self.db, new.customer_id).await?;
        let currency = match &new.currency {
            Some(c) => validate_currency(c)?,
            None => buyer.currency.clone(),
        };
        validate_quotation(&self.db, &new).await?;
        let priced = self.price_lines(&new, &buyer, &currency).await?;

        let quotation_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        let txn = self.db.begin().await?;
        quotation::ActiveModel {
            id: Set(quotation_id),
            number: Set(None),
            customer_id: Set(new.customer_id),
            quote_date: Set(new.quote_date),
            valid_until: Set(new.valid_until),
            currency: Set(currency),
            price_list_id: Set(buyer.price_list_id),
            tax_inclusive: Set(new.tax_inclusive),
            discount_pct: Set(new.discount_pct),
            discount_amount: Set(new.discount_amount),
            other_charges: Set(new.other_charges),
            customer_contact: Set(clean(new.customer_contact)),
            salesperson_id: Set(buyer.salesperson_id),
            memo: Set(clean(new.memo)),
            reference: Set(clean(new.reference)),
            terms_and_conditions: Set(clean(new.terms_and_conditions)),
            status: Set(QuotationStatus::Draft.as_str().to_string()),
            sent_at: Set(None),
            sent_by: Set(None),
            resolved_at: Set(None),
            decline_reason: Set(None),
            converted_to_id: Set(None),
            created_at: Set(now),
            created_by: Set(new.created_by),
            updated_at: Set(now),
            updated_by: Set(None),
        }
        .insert(&txn)
        .await?;
        insert_lines(&txn, quotation_id, &priced).await?;
        txn.commit().await?;
        self.view(quotation_id).await
    }

    pub async fn update_draft(
        &self,
        id: Uuid,
        new: NewQuotation,
        by: Option<Uuid>,
    ) -> Result<QuotationView> {
        let buyer = order::load_customer_for_new_order(&self.db, new.customer_id).await?;
        let currency = match &new.currency {
            Some(c) => validate_currency(c)?,
            None => buyer.currency.clone(),
        };
        validate_quotation(&self.db, &new).await?;
        let priced = self.price_lines(&new, &buyer, &currency).await?;

        let txn = self.db.begin().await?;
        let existing = load_quotation_locked(&txn, id).await?;
        if QuotationStatus::parse(&existing.status)? != QuotationStatus::Draft {
            return Err(Error::Validation(
                "only a draft quotation can be edited".into(),
            ));
        }
        quotation_line::Entity::delete_many()
            .filter(quotation_line::Column::QuotationId.eq(id))
            .exec(&txn)
            .await?;
        insert_lines(&txn, id, &priced).await?;

        let mut active: quotation::ActiveModel = existing.into();
        active.customer_id = Set(new.customer_id);
        active.quote_date = Set(new.quote_date);
        active.valid_until = Set(new.valid_until);
        active.currency = Set(currency);
        active.price_list_id = Set(buyer.price_list_id);
        active.tax_inclusive = Set(new.tax_inclusive);
        active.discount_pct = Set(new.discount_pct);
        active.discount_amount = Set(new.discount_amount);
        active.other_charges = Set(new.other_charges);
        active.customer_contact = Set(clean(new.customer_contact));
        active.salesperson_id = Set(buyer.salesperson_id);
        active.memo = Set(clean(new.memo));
        active.reference = Set(clean(new.reference));
        active.terms_and_conditions = Set(clean(new.terms_and_conditions));
        active.updated_at = Set(chrono::Utc::now());
        active.updated_by = Set(by);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    pub async fn delete_draft(&self, id: Uuid) -> Result<QuotationView> {
        let view = self.view(id).await?;
        let txn = self.db.begin().await?;
        let existing = load_quotation_locked(&txn, id).await?;
        if QuotationStatus::parse(&existing.status)? != QuotationStatus::Draft {
            return Err(Error::Validation(
                "only a draft quotation can be deleted".into(),
            ));
        }
        quotation::Entity::delete_by_id(id).exec(&txn).await?;
        txn.commit().await?;
        Ok(view)
    }

    /// Send a draft to the customer: allocate the QUO number and freeze
    /// the offer.
    pub async fn send(&self, id: Uuid, numbering: &Numbering, by: Option<Uuid>) -> Result<QuotationView> {
        let txn = self.db.begin().await?;
        let existing = load_quotation_locked(&txn, id).await?;
        if QuotationStatus::parse(&existing.status)? != QuotationStatus::Draft {
            return Err(Error::Validation(
                "only a draft quotation can be sent".into(),
            ));
        }
        let lines = load_lines(&txn, id).await?;
        if lines.is_empty() {
            return Err(Error::Validation(
                "a quotation needs at least one line".into(),
            ));
        }
        let number = numbering
            .next(&txn, crate::scm::SALES_QUOTATION_SERIES)
            .await?;
        let now = chrono::Utc::now();
        let mut active: quotation::ActiveModel = existing.into();
        active.number = Set(Some(number.formatted));
        active.status = Set(QuotationStatus::Sent.as_str().to_string());
        active.sent_at = Set(Some(now));
        active.sent_by = Set(by);
        active.updated_at = Set(now);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Record the customer's acceptance. A quotation past its validity
    /// moves to `expired` instead — the offer no longer stands.
    pub async fn accept(&self, id: Uuid) -> Result<QuotationView> {
        let txn = self.db.begin().await?;
        let existing = load_quotation_locked(&txn, id).await?;
        if QuotationStatus::parse(&existing.status)? != QuotationStatus::Sent {
            return Err(Error::Validation(
                "only a sent quotation can be accepted".into(),
            ));
        }
        let now = chrono::Utc::now();
        let today = now.date_naive();
        if existing.valid_until.is_some_and(|until| until < today) {
            let number = existing.number.clone().unwrap_or_default();
            let mut active: quotation::ActiveModel = existing.into();
            active.status = Set(QuotationStatus::Expired.as_str().to_string());
            active.resolved_at = Set(Some(now));
            active.updated_at = Set(now);
            active.update(&txn).await?;
            txn.commit().await?;
            return Err(Error::Validation(format!(
                "quotation {number} expired and can no longer be accepted; quote afresh"
            )));
        }
        let mut active: quotation::ActiveModel = existing.into();
        active.status = Set(QuotationStatus::Accepted.as_str().to_string());
        active.resolved_at = Set(Some(now));
        active.updated_at = Set(now);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Record the customer's decline.
    pub async fn decline(&self, id: Uuid, reason: &str) -> Result<QuotationView> {
        let txn = self.db.begin().await?;
        let existing = load_quotation_locked(&txn, id).await?;
        if QuotationStatus::parse(&existing.status)? != QuotationStatus::Sent {
            return Err(Error::Validation(
                "only a sent quotation can be declined".into(),
            ));
        }
        let now = chrono::Utc::now();
        let mut active: quotation::ActiveModel = existing.into();
        active.status = Set(QuotationStatus::Declined.as_str().to_string());
        active.resolved_at = Set(Some(now));
        active.decline_reason = Set(Some(reason.trim().to_string()).filter(|r| !r.is_empty()));
        active.updated_at = Set(now);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Cut a draft sales order from an accepted quotation. Lines copy
    /// with their quoted prices and provenance — the customer accepted
    /// those numbers — and the new order walks the normal confirm path
    /// afterwards. `warehouse_id` names the fulfilment source, which a
    /// quotation never carried.
    pub async fn convert(
        &self,
        id: Uuid,
        warehouse_id: Uuid,
        expected_date: Option<chrono::NaiveDate>,
        by: Option<Uuid>,
    ) -> Result<QuotationView> {
        let txn = self.db.begin().await?;
        let existing = load_quotation_locked(&txn, id).await?;
        if QuotationStatus::parse(&existing.status)? != QuotationStatus::Accepted {
            return Err(Error::Validation(
                "only an accepted quotation can become an order".into(),
            ));
        }
        let buyer = order::load_customer_for_new_order(&txn, existing.customer_id).await?;
        let wh = crate::scm::inventory::warehouse::Entity::find_by_id(warehouse_id)
            .one(&txn)
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
                    "warehouse {warehouse_id} does not exist"
                )));
            }
        }
        let lines = load_lines(&txn, id).await?;
        if lines.is_empty() {
            return Err(Error::Validation(
                "the quotation has no lines to convert".into(),
            ));
        }

        let order_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        order::order::ActiveModel {
            id: Set(order_id),
            number: Set(None),
            customer_id: Set(existing.customer_id),
            quotation_id: Set(Some(existing.id)),
            order_date: Set(now.date_naive()),
            expected_date: Set(expected_date),
            warehouse_id: Set(warehouse_id),
            shipping_address: Set(None),
            shipping_method: Set(None),
            incoterms: Set(buyer.incoterms.clone()),
            customer_contact: Set(existing.customer_contact.clone()),
            customer_po_no: Set(existing.reference.clone()),
            salesperson_id: Set(existing.salesperson_id),
            currency: Set(existing.currency.clone()),
            exchange_rate: Set(Decimal::ONE),
            price_list_id: Set(existing.price_list_id),
            payment_terms_days: Set(buyer.payment_terms_days),
            tax_inclusive: Set(existing.tax_inclusive),
            discount_pct: Set(existing.discount_pct),
            discount_amount: Set(existing.discount_amount),
            other_charges: Set(existing.other_charges),
            memo: Set(existing.memo.clone()),
            terms_and_conditions: Set(existing.terms_and_conditions.clone()),
            status: Set(order::OrderStatus::Draft.as_str().to_string()),
            confirmed_at: Set(None),
            confirmed_by: Set(None),
            credit_override_by: Set(None),
            cancelled_at: Set(None),
            cancelled_by: Set(None),
            cancel_reason: Set(None),
            closed_at: Set(None),
            closed_by: Set(None),
            created_at: Set(now),
            created_by: Set(by),
            updated_at: Set(now),
            updated_by: Set(None),
        }
        .insert(&txn)
        .await?;
        let priced: Vec<PricedLine> = lines
            .iter()
            .map(|l| PricedLine {
                item_id: l.item_id,
                description: l.description.clone(),
                qty: l.qty,
                warehouse_id: None,
                unit_price: l.unit_price,
                price_source: l.price_source.clone(),
                discount_pct: l.discount_pct,
                tax_code_id: l.tax_code_id,
                expected_date: None,
                memo: l.memo.clone(),
            })
            .collect();
        order::insert_lines(&txn, order_id, &priced).await?;

        let mut active: quotation::ActiveModel = existing.into();
        active.status = Set(QuotationStatus::Converted.as_str().to_string());
        active.converted_to_id = Set(Some(order_id));
        active.updated_at = Set(now);
        active.updated_by = Set(by);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    pub async fn list(&self, filter: QuotationFilter) -> Result<Vec<QuotationHeader>> {
        let mut query = quotation::Entity::find();
        if let Some(s) = filter.status {
            query = query.filter(quotation::Column::Status.eq(s.as_str()));
        }
        if let Some(customer_id) = filter.customer_id {
            query = query.filter(quotation::Column::CustomerId.eq(customer_id));
        }
        if let Some(from) = filter.from {
            query = query.filter(quotation::Column::QuoteDate.gte(from));
        }
        if let Some(to) = filter.to {
            query = query.filter(quotation::Column::QuoteDate.lte(to));
        }
        let rows = query
            .order_by_desc(quotation::Column::QuoteDate)
            .order_by_desc(quotation::Column::CreatedAt)
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
                Ok(QuotationHeader {
                    id: r.id,
                    number: r.number.clone(),
                    customer_id: r.customer_id,
                    customer_name: customers
                        .get(&r.customer_id)
                        .map(|c| c.name.clone())
                        .unwrap_or_default(),
                    quote_date: r.quote_date,
                    valid_until: r.valid_until,
                    currency: r.currency.clone(),
                    status: QuotationStatus::parse(&r.status)?,
                })
            })
            .collect()
    }

    /// Load a full quotation with lines, labels and computed totals.
    pub async fn view(&self, id: Uuid) -> Result<QuotationView> {
        let row = load_quotation(&self.db, id).await?;
        let lines = load_lines(&self.db, id).await?;
        let buyer = customer::Entity::find_by_id(row.customer_id)
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
        let line_views: Vec<QuotationLineView> = lines
            .into_iter()
            .map(|l| {
                let it = items.get(&l.item_id);
                let price = order::effective_price(l.unit_price, l.discount_pct);
                let net = stock::round_money(l.qty * price);
                subtotal += net;
                QuotationLineView {
                    id: l.id,
                    line_no: l.line_no,
                    item_id: l.item_id,
                    sku: it.map(|i| i.sku.clone()).unwrap_or_default(),
                    item_name: it.map(|i| i.name.clone()).unwrap_or_default(),
                    description: l.description,
                    qty: l.qty,
                    unit_price: l.unit_price,
                    price_source: l.price_source,
                    discount_pct: l.discount_pct,
                    effective_price: price,
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

        Ok(QuotationView {
            id: row.id,
            number: row.number,
            customer_id: row.customer_id,
            customer_name: buyer.map(|c| c.name).unwrap_or_default(),
            quote_date: row.quote_date,
            valid_until: row.valid_until,
            currency: row.currency,
            tax_inclusive: row.tax_inclusive,
            discount_pct: row.discount_pct,
            discount_amount: row.discount_amount,
            other_charges: row.other_charges,
            customer_contact: row.customer_contact,
            salesperson_id: row.salesperson_id,
            memo: row.memo,
            reference: row.reference,
            terms_and_conditions: row.terms_and_conditions,
            status: QuotationStatus::parse(&row.status)?,
            decline_reason: row.decline_reason,
            converted_to_id: row.converted_to_id,
            subtotal,
            total,
            created_at: row.created_at,
            lines: line_views,
        })
    }

    /// Price every line, resolving where no manual price was given.
    async fn price_lines(
        &self,
        new: &NewQuotation,
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
                        date: new.quote_date,
                    },
                    l.unit_price,
                    new.allow_price_override,
                )
                .await?;
            priced.push(PricedLine {
                item_id: l.item_id,
                description: l.description.clone(),
                qty: l.qty,
                warehouse_id: None,
                unit_price,
                price_source: Some(source.as_string()),
                discount_pct: l.discount_pct,
                tax_code_id: l.tax_code_id,
                expected_date: None,
                memo: l.memo.clone(),
            });
        }
        Ok(priced)
    }
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

async fn validate_quotation<C: ConnectionTrait>(conn: &C, new: &NewQuotation) -> Result<()> {
    if new.lines.is_empty() {
        return Err(Error::Validation(
            "a quotation needs at least one line".into(),
        ));
    }
    if let Some(until) = new.valid_until {
        if until < new.quote_date {
            return Err(Error::Validation(
                "a quotation cannot expire before its own date".into(),
            ));
        }
    }
    if let Some(pct) = new.discount_pct {
        if pct < Decimal::ZERO || pct > Decimal::ONE_HUNDRED {
            return Err(Error::Validation(
                "discount must be between 0 and 100 percent".into(),
            ));
        }
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

async fn insert_lines<C: ConnectionTrait>(
    conn: &C,
    quotation_id: Uuid,
    lines: &[PricedLine],
) -> Result<()> {
    let now = chrono::Utc::now();
    for (i, l) in lines.iter().enumerate() {
        quotation_line::ActiveModel {
            id: Set(Uuid::new_v4()),
            quotation_id: Set(quotation_id),
            line_no: Set((i + 1) as i32),
            item_id: Set(l.item_id),
            description: Set(l.description.clone().filter(|d| !d.trim().is_empty())),
            qty: Set(l.qty),
            uom_id: Set(None),
            unit_price: Set(l.unit_price),
            price_source: Set(l.price_source.clone()),
            discount_pct: Set(l.discount_pct),
            tax_code_id: Set(l.tax_code_id),
            memo: Set(l.memo.clone().filter(|m| !m.trim().is_empty())),
            created_at: Set(now),
        }
        .insert(conn)
        .await?;
    }
    Ok(())
}

async fn load_quotation<C: ConnectionTrait>(conn: &C, id: Uuid) -> Result<quotation::Model> {
    quotation::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("quotation {id}")))
}

async fn load_quotation_locked(
    txn: &sea_orm::DatabaseTransaction,
    id: Uuid,
) -> Result<quotation::Model> {
    quotation::Entity::find_by_id(id)
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("quotation {id}")))
}

async fn load_lines<C: ConnectionTrait>(
    conn: &C,
    quotation_id: Uuid,
) -> Result<Vec<quotation_line::Model>> {
    quotation_line::Entity::find()
        .filter(quotation_line::Column::QuotationId.eq(quotation_id))
        .order_by_asc(quotation_line::Column::LineNo)
        .all(conn)
        .await
        .map_err(Error::from)
}

// ---------------------------------------------------------------------------
// Views (API DTOs)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct QuotationLineView {
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
    pub price_source: Option<String>,
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub discount_pct: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub effective_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub net: Decimal,
    pub memo: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct QuotationView {
    pub id: Uuid,
    pub number: Option<String>,
    pub customer_id: Uuid,
    pub customer_name: String,
    #[schema(value_type = String, format = Date)]
    pub quote_date: chrono::NaiveDate,
    #[schema(value_type = Option<String>, format = Date)]
    pub valid_until: Option<chrono::NaiveDate>,
    pub currency: String,
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
    pub customer_contact: Option<String>,
    pub salesperson_id: Option<Uuid>,
    pub memo: Option<String>,
    pub reference: Option<String>,
    pub terms_and_conditions: Option<String>,
    pub status: QuotationStatus,
    pub decline_reason: Option<String>,
    /// The sales order cut from this quotation, when converted.
    pub converted_to_id: Option<Uuid>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub subtotal: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total: Decimal,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub lines: Vec<QuotationLineView>,
}

/// A row of the quotation register.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct QuotationHeader {
    pub id: Uuid,
    pub number: Option<String>,
    pub customer_id: Uuid,
    pub customer_name: String,
    #[schema(value_type = String, format = Date)]
    pub quote_date: chrono::NaiveDate,
    #[schema(value_type = Option<String>, format = Date)]
    pub valid_until: Option<chrono::NaiveDate>,
    pub currency: String,
    pub status: QuotationStatus,
}

pub struct QuotationFilter {
    pub status: Option<QuotationStatus>,
    pub customer_id: Option<Uuid>,
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
pub struct QuotationLineRequest {
    pub item_id: Uuid,
    pub description: Option<String>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    /// Omit to price through the chain; supplying one is a manual
    /// override and needs `Pricing.Override`.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub unit_price: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub discount_pct: Option<Decimal>,
    pub tax_code_id: Option<Uuid>,
    pub memo: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateQuotationRequest {
    pub customer_id: Uuid,
    #[schema(value_type = String, format = Date)]
    pub quote_date: chrono::NaiveDate,
    #[schema(value_type = Option<String>, format = Date)]
    pub valid_until: Option<chrono::NaiveDate>,
    /// Defaults to the customer's currency.
    pub currency: Option<String>,
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
    pub customer_contact: Option<String>,
    pub memo: Option<String>,
    pub reference: Option<String>,
    pub terms_and_conditions: Option<String>,
    pub lines: Vec<QuotationLineRequest>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct DeclineQuotationRequest {
    #[serde(default)]
    pub reason: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ConvertQuotationRequest {
    /// The warehouse the resulting order fulfils from.
    pub warehouse_id: Uuid,
    #[schema(value_type = Option<String>, format = Date)]
    pub expected_date: Option<chrono::NaiveDate>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuotationsQuery {
    pub status: Option<QuotationStatus>,
    pub customer_id: Option<Uuid>,
    /// Quote date range, inclusive.
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

fn new_quotation(
    req: CreateQuotationRequest,
    created_by: Option<Uuid>,
    allow_override: bool,
) -> NewQuotation {
    NewQuotation {
        customer_id: req.customer_id,
        quote_date: req.quote_date,
        valid_until: req.valid_until,
        currency: req.currency,
        tax_inclusive: req.tax_inclusive,
        discount_pct: req.discount_pct,
        discount_amount: req.discount_amount,
        other_charges: req.other_charges,
        customer_contact: req.customer_contact,
        memo: req.memo,
        reference: req.reference,
        terms_and_conditions: req.terms_and_conditions,
        lines: req
            .lines
            .into_iter()
            .map(|l| QuotationLineInput {
                item_id: l.item_id,
                description: l.description,
                qty: l.qty,
                unit_price: l.unit_price,
                discount_pct: l.discount_pct,
                tax_code_id: l.tax_code_id,
                memo: l.memo,
            })
            .collect(),
        created_by,
        allow_price_override: allow_override,
    }
}

pub(crate) fn routes() -> Router {
    Router::new()
        .route(
            "/sales/quotations",
            get(list_quotations).post(create_quotation),
        )
        .route(
            "/sales/quotations/{id}",
            get(get_quotation)
                .put(update_quotation)
                .delete(delete_quotation),
        )
        .route("/sales/quotations/{id}/send", post(send_quotation))
        .route("/sales/quotations/{id}/accept", post(accept_quotation))
        .route("/sales/quotations/{id}/decline", post(decline_quotation))
        .route("/sales/quotations/{id}/convert", post(convert_quotation))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        list_quotations,
        get_quotation,
        create_quotation,
        update_quotation,
        delete_quotation,
        send_quotation,
        accept_quotation,
        decline_quotation,
        convert_quotation
    ),
    components(schemas(QuotationStatus))
)]
struct ApiDoc;

#[utoipa::path(get, path = "/sales/quotations", tag = "sales",
    params(ListQuotationsQuery),
    responses((status = 200, body = Vec<QuotationHeader>)))]
async fn list_quotations(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<ListQuotationsQuery>,
) -> Result<Json<Vec<QuotationHeader>>> {
    authz.require(names::QUOTATIONS_VIEW).await?;
    QuotationService::new(db)
        .list(QuotationFilter {
            status: q.status,
            customer_id: q.customer_id,
            from: q.from,
            to: q.to,
        })
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/sales/quotations/{id}", tag = "sales",
    params(("id" = Uuid, Path, description = "Quotation id")),
    responses((status = 200, body = QuotationView)))]
async fn get_quotation(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<QuotationView>> {
    authz.require(names::QUOTATIONS_VIEW).await?;
    QuotationService::new(db).view(id).await.map(Json)
}

#[utoipa::path(post, path = "/sales/quotations", tag = "sales",
    request_body = CreateQuotationRequest,
    responses((status = 200, body = QuotationView)))]
async fn create_quotation(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(req): Json<CreateQuotationRequest>,
) -> Result<Json<QuotationView>> {
    authz.require(names::QUOTATIONS_CREATE).await?;
    let allow_override = authz.require(names::PRICING_OVERRIDE).await.is_ok();
    let view = QuotationService::new(db)
        .create_draft(new_quotation(req, Some(authz.user.id), allow_override))
        .await?;
    audit.0.created("scm.quotation", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(put, path = "/sales/quotations/{id}", tag = "sales",
    params(("id" = Uuid, Path, description = "Quotation id")),
    request_body = CreateQuotationRequest,
    responses((status = 200, body = QuotationView)))]
async fn update_quotation(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<CreateQuotationRequest>,
) -> Result<Json<QuotationView>> {
    authz.require(names::QUOTATIONS_CREATE).await?;
    let allow_override = authz.require(names::PRICING_OVERRIDE).await.is_ok();
    let service = QuotationService::new(db);
    let before = service.view(id).await?;
    let after = service
        .update_draft(id, new_quotation(req, None, allow_override), Some(authz.user.id))
        .await?;
    audit.0.updated("scm.quotation", id, &before, &after).await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/sales/quotations/{id}", tag = "sales",
    params(("id" = Uuid, Path, description = "Quotation id")),
    responses((status = 200, body = QuotationView)))]
async fn delete_quotation(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<QuotationView>> {
    authz.require(names::QUOTATIONS_CREATE).await?;
    let view = QuotationService::new(db).delete_draft(id).await?;
    audit.0.deleted("scm.quotation", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/sales/quotations/{id}/send", tag = "sales",
    params(("id" = Uuid, Path, description = "Quotation id")),
    responses((status = 200, body = QuotationView)))]
async fn send_quotation(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Extension(numbering): Extension<Numbering>,
    Path(id): Path<Uuid>,
) -> Result<Json<QuotationView>> {
    authz.require(names::QUOTATIONS_SEND).await?;
    let view = QuotationService::new(db)
        .send(id, &numbering, Some(authz.user.id))
        .await?;
    audit
        .0
        .event(format!(
            "sent quotation {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/sales/quotations/{id}/accept", tag = "sales",
    params(("id" = Uuid, Path, description = "Quotation id")),
    responses((status = 200, body = QuotationView)))]
async fn accept_quotation(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<QuotationView>> {
    authz.require(names::QUOTATIONS_SEND).await?;
    let view = QuotationService::new(db).accept(id).await?;
    audit
        .0
        .event(format!(
            "recorded acceptance of quotation {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/sales/quotations/{id}/decline", tag = "sales",
    params(("id" = Uuid, Path, description = "Quotation id")),
    request_body = DeclineQuotationRequest,
    responses((status = 200, body = QuotationView)))]
async fn decline_quotation(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<DeclineQuotationRequest>,
) -> Result<Json<QuotationView>> {
    authz.require(names::QUOTATIONS_SEND).await?;
    let view = QuotationService::new(db).decline(id, &req.reason).await?;
    audit
        .0
        .event(format!(
            "recorded decline of quotation {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/sales/quotations/{id}/convert", tag = "sales",
    params(("id" = Uuid, Path, description = "Quotation id")),
    request_body = ConvertQuotationRequest,
    responses((status = 200, body = QuotationView)))]
async fn convert_quotation(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<ConvertQuotationRequest>,
) -> Result<Json<QuotationView>> {
    authz.require(names::QUOTATIONS_CONVERT).await?;
    let view = QuotationService::new(db)
        .convert(id, req.warehouse_id, req.expected_date, Some(authz.user.id))
        .await?;
    audit
        .0
        .event(format!(
            "converted quotation {} to a sales order",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}
