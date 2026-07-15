//! Price lists and the resolution chain: "what does this item cost this
//! customer today, and why?".
//!
//! A list is scoped to all customers (`default`), a customer group, or a
//! single customer, is date-bounded, and holds per-item lines with
//! optional quantity breaks (`min_qty`). A line prices with either a
//! fixed `unit_price` or a `discount_pct` off the item's default selling
//! price — exactly one of the two. Lifecycle: draft → active → archived;
//! only active lists price documents.
//!
//! [`PricingService::resolve`] walks: the customer's own list → the
//! customer group's list → active default lists (promotional first) →
//! `item.selling_price`, taking within each list the highest
//! `min_qty ≤ qty` line, and returns the price *with its provenance* so
//! document lines can record where their price came from. Manual
//! overrides never pass through here: document services gate them with
//! `Pricing.Override` and floor them at `item.min_selling_price`.

use crate::scm::inventory::item::item;
use crate::scm::sales::customer::{customer, group};
use crate::scm::sales::permissions::names;
use axum::extract::{Path, Query};
use axum::routing::{get, post};
use axum::{Json, Router};
use nebula::audit::Audit;
use nebula::auth::Authz;
use nebula::error::{Error, Result};
use nebula::{TenantDb, sea_orm};
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, QueryOrder, Set, TransactionTrait};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A price list header.
pub mod price_list {
    use nebula::sea_orm;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, utoipa::ToSchema)]
    #[schema(as = SalesPriceList)]
    #[sea_orm(table_name = "sales_price_lists")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        #[sea_orm(unique)]
        pub name: String,
        pub description: Option<String>,
        /// ISO 4217; only documents in this currency match the list.
        pub currency: String,
        /// default|group|customer.
        pub scope: String,
        pub tax_inclusive: bool,
        #[schema(value_type = Option<String>, format = Date)]
        pub valid_from: Option<Date>,
        #[schema(value_type = Option<String>, format = Date)]
        pub valid_to: Option<Date>,
        /// draft|active|archived.
        pub status: String,
        pub is_promotional: bool,
        #[schema(value_type = String, format = DateTime)]
        pub created_at: DateTimeUtc,
        pub created_by: Option<Uuid>,
        #[schema(value_type = String, format = DateTime)]
        pub updated_at: DateTimeUtc,
        pub updated_by: Option<Uuid>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// A price for an item on a list.
pub mod price_list_item {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, utoipa::ToSchema)]
    #[schema(as = SalesPriceListItem)]
    #[sea_orm(table_name = "sales_price_list_items")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub price_list_id: Uuid,
        pub item_id: Uuid,
        /// NULL = the item's stock UoM.
        pub uom_id: Option<Uuid>,
        /// Quantity break: the highest `min_qty <= ordered qty` wins.
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        #[serde(with = "rust_decimal::serde::str")]
        #[schema(value_type = String)]
        pub min_qty: Decimal,
        /// Exactly one of `unit_price` / `discount_pct` is set.
        #[sea_orm(column_type = "Decimal(Some((20, 6)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub unit_price: Option<Decimal>,
        /// Percentage off the item's default selling price.
        #[sea_orm(column_type = "Decimal(Some((7, 4)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub discount_pct: Option<Decimal>,
        #[schema(value_type = String, format = DateTime)]
        pub created_at: DateTimeUtc,
        #[schema(value_type = String, format = DateTime)]
        pub updated_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Who a price list applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ListScope {
    /// Every customer (the sticker price, or a promotion for everyone).
    Default,
    /// Customers of one group (trade, wholesale, retail).
    Group,
    /// One negotiated customer.
    Customer,
}

impl ListScope {
    pub fn as_str(self) -> &'static str {
        match self {
            ListScope::Default => "default",
            ListScope::Group => "group",
            ListScope::Customer => "customer",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "default" => Ok(ListScope::Default),
            "group" => Ok(ListScope::Group),
            "customer" => Ok(ListScope::Customer),
            other => Err(Error::internal(format!("unknown list scope {other:?}"))),
        }
    }
}

/// Where a list is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ListStatus {
    /// Being composed; never prices a document.
    Draft,
    /// Live: prices documents whose date its window covers.
    Active,
    /// Retired; read-only history.
    Archived,
}

impl ListStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ListStatus::Draft => "draft",
            ListStatus::Active => "active",
            ListStatus::Archived => "archived",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "draft" => Ok(ListStatus::Draft),
            "active" => Ok(ListStatus::Active),
            "archived" => Ok(ListStatus::Archived),
            other => Err(Error::internal(format!("unknown list status {other:?}"))),
        }
    }
}

/// Where a resolved price came from — recorded on document lines so
/// "why this price?" is always answerable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case", tag = "kind", content = "price_list_id")]
pub enum PriceSource {
    /// A price list line matched (the id names the list).
    List(Uuid),
    /// The item's own default selling price.
    ItemDefault,
    /// A permission-gated manual override on the document line.
    Manual,
}

impl PriceSource {
    /// The compact form document lines persist, e.g. `list:{uuid}`.
    pub fn as_string(&self) -> String {
        match self {
            PriceSource::List(id) => format!("list:{id}"),
            PriceSource::ItemDefault => "item_default".to_string(),
            PriceSource::Manual => "manual".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Bodies & views
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
pub struct PriceListBody {
    pub name: String,
    pub description: Option<String>,
    pub currency: String,
    pub scope: ListScope,
    #[serde(default)]
    pub tax_inclusive: bool,
    #[schema(value_type = Option<String>, format = Date)]
    pub valid_from: Option<chrono::NaiveDate>,
    #[schema(value_type = Option<String>, format = Date)]
    pub valid_to: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub is_promotional: bool,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct PriceLineBody {
    pub item_id: Uuid,
    pub uom_id: Option<Uuid>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub min_qty: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub unit_price: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub discount_pct: Option<Decimal>,
}

/// Replace a list's lines wholesale (the draft-editing model).
#[derive(Deserialize, utoipa::ToSchema)]
pub struct PriceLinesBody {
    pub lines: Vec<PriceLineBody>,
}

/// A resolved price with its provenance.
#[derive(Serialize, utoipa::ToSchema)]
pub struct ResolvedPrice {
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub unit_price: Decimal,
    pub source: PriceSource,
    /// The list's flag when a list priced it; documents honour it when
    /// computing tax.
    pub tax_inclusive: bool,
    pub currency: String,
}

/// Query for the `/sales/pricing/resolve` probe.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct ResolveQuery {
    pub customer_id: Uuid,
    pub item_id: Uuid,
    /// Defaults to 1.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub qty: Option<Decimal>,
    /// Defaults to the item's stock UoM.
    pub uom_id: Option<Uuid>,
    /// Defaults to the customer's currency.
    pub currency: Option<String>,
    /// Defaults to today.
    #[schema(value_type = Option<String>, format = Date)]
    pub date: Option<chrono::NaiveDate>,
}

// ---------------------------------------------------------------------------
// Store: list CRUD and lifecycle
// ---------------------------------------------------------------------------

pub struct Store {
    db: DatabaseConnection,
}

impl Store {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn find_all(&self) -> Result<Vec<price_list::Model>> {
        price_list::Entity::find()
            .order_by_asc(price_list::Column::Name)
            .all(&self.db)
            .await
            .map_err(Error::from)
    }

    pub async fn find_by_id(&self, id: Uuid) -> Result<price_list::Model> {
        price_list::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or_else(|| Error::NotFound(format!("price list {id}")))
    }

    pub async fn lines(&self, list_id: Uuid) -> Result<Vec<price_list_item::Model>> {
        self.find_by_id(list_id).await?;
        price_list_item::Entity::find()
            .filter(price_list_item::Column::PriceListId.eq(list_id))
            .order_by_asc(price_list_item::Column::CreatedAt)
            .all(&self.db)
            .await
            .map_err(Error::from)
    }

    pub async fn create(
        &self,
        body: PriceListBody,
        created_by: Option<Uuid>,
    ) -> Result<price_list::Model> {
        let (name, currency) = self.validate(&body, None).await?;
        let now = chrono::Utc::now();
        price_list::ActiveModel {
            id: Set(Uuid::new_v4()),
            name: Set(name),
            description: Set(body.description.filter(|s| !s.trim().is_empty())),
            currency: Set(currency),
            scope: Set(body.scope.as_str().to_string()),
            tax_inclusive: Set(body.tax_inclusive),
            valid_from: Set(body.valid_from),
            valid_to: Set(body.valid_to),
            status: Set(ListStatus::Draft.as_str().to_string()),
            is_promotional: Set(body.is_promotional),
            created_at: Set(now),
            created_by: Set(created_by),
            updated_at: Set(now),
            updated_by: Set(None),
        }
        .insert(&self.db)
        .await
        .map_err(Error::from)
    }

    /// Update a list's header. Scope and currency freeze once active —
    /// re-aiming a live list would silently reprice documents.
    pub async fn update(
        &self,
        id: Uuid,
        body: PriceListBody,
        updated_by: Option<Uuid>,
    ) -> Result<price_list::Model> {
        let existing = self.find_by_id(id).await?;
        let status = ListStatus::parse(&existing.status)?;
        if status == ListStatus::Archived {
            return Err(Error::Validation(
                "an archived price list is read-only".into(),
            ));
        }
        let (name, currency) = self.validate(&body, Some(&existing)).await?;
        if status == ListStatus::Active
            && (existing.scope != body.scope.as_str() || existing.currency != currency)
        {
            return Err(Error::Validation(
                "an active list's scope and currency cannot change; archive it and cut a new one"
                    .into(),
            ));
        }
        let mut active: price_list::ActiveModel = existing.into();
        active.name = Set(name);
        active.description = Set(body.description.filter(|s| !s.trim().is_empty()));
        active.currency = Set(currency);
        active.scope = Set(body.scope.as_str().to_string());
        active.tax_inclusive = Set(body.tax_inclusive);
        active.valid_from = Set(body.valid_from);
        active.valid_to = Set(body.valid_to);
        active.is_promotional = Set(body.is_promotional);
        active.updated_at = Set(chrono::Utc::now());
        active.updated_by = Set(updated_by);
        active.update(&self.db).await.map_err(Error::from)
    }

    pub async fn activate(&self, id: Uuid, updated_by: Option<Uuid>) -> Result<price_list::Model> {
        let existing = self.find_by_id(id).await?;
        if ListStatus::parse(&existing.status)? != ListStatus::Draft {
            return Err(Error::Validation(
                "only a draft price list can be activated".into(),
            ));
        }
        let mut active: price_list::ActiveModel = existing.into();
        active.status = Set(ListStatus::Active.as_str().to_string());
        active.updated_at = Set(chrono::Utc::now());
        active.updated_by = Set(updated_by);
        active.update(&self.db).await.map_err(Error::from)
    }

    pub async fn archive(&self, id: Uuid, updated_by: Option<Uuid>) -> Result<price_list::Model> {
        let existing = self.find_by_id(id).await?;
        if ListStatus::parse(&existing.status)? == ListStatus::Archived {
            return Err(Error::Validation("price list is already archived".into()));
        }
        let mut active: price_list::ActiveModel = existing.into();
        active.status = Set(ListStatus::Archived.as_str().to_string());
        active.updated_at = Set(chrono::Utc::now());
        active.updated_by = Set(updated_by);
        active.update(&self.db).await.map_err(Error::from)
    }

    /// Replace a list's lines wholesale (archived lists are read-only).
    pub async fn replace_lines(
        &self,
        list_id: Uuid,
        body: PriceLinesBody,
    ) -> Result<Vec<price_list_item::Model>> {
        let list = self.find_by_id(list_id).await?;
        if ListStatus::parse(&list.status)? == ListStatus::Archived {
            return Err(Error::Validation(
                "an archived price list is read-only".into(),
            ));
        }

        // Validate every line against the item master first.
        let item_ids: Vec<Uuid> = body.lines.iter().map(|l| l.item_id).collect();
        let items: std::collections::HashMap<Uuid, item::Model> = item::Entity::find()
            .filter(item::Column::Id.is_in(item_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|i| (i.id, i))
            .collect();
        let mut seen: std::collections::HashSet<(Uuid, Option<Uuid>, Decimal)> =
            std::collections::HashSet::new();
        for line in &body.lines {
            let Some(found) = items.get(&line.item_id) else {
                return Err(Error::NotFound(format!("item {}", line.item_id)));
            };
            if !found.is_sellable {
                return Err(Error::Validation(format!(
                    "item {} is not sellable",
                    found.sku
                )));
            }
            let min_qty = line.min_qty.unwrap_or(Decimal::ZERO);
            if min_qty < Decimal::ZERO {
                return Err(Error::Validation(format!(
                    "item {}: minimum quantity must not be negative",
                    found.sku
                )));
            }
            match (line.unit_price, line.discount_pct) {
                (Some(price), None) => {
                    if price < Decimal::ZERO {
                        return Err(Error::Validation(format!(
                            "item {}: price must not be negative",
                            found.sku
                        )));
                    }
                }
                (None, Some(pct)) => {
                    if pct < Decimal::ZERO || pct > Decimal::ONE_HUNDRED {
                        return Err(Error::Validation(format!(
                            "item {}: discount must be between 0 and 100 percent",
                            found.sku
                        )));
                    }
                    if found.selling_price.is_none() {
                        return Err(Error::Validation(format!(
                            "item {} has no default selling price for a discount line to work from",
                            found.sku
                        )));
                    }
                }
                _ => {
                    return Err(Error::Validation(format!(
                        "item {}: a line prices with exactly one of a unit price or a discount",
                        found.sku
                    )));
                }
            }
            if !seen.insert((line.item_id, line.uom_id, min_qty)) {
                return Err(Error::Validation(format!(
                    "item {}: duplicate line for the same UoM and minimum quantity",
                    found.sku
                )));
            }
        }

        let txn = self.db.begin().await?;
        price_list_item::Entity::delete_many()
            .filter(price_list_item::Column::PriceListId.eq(list_id))
            .exec(&txn)
            .await?;
        let now = chrono::Utc::now();
        for line in &body.lines {
            price_list_item::ActiveModel {
                id: Set(Uuid::new_v4()),
                price_list_id: Set(list_id),
                item_id: Set(line.item_id),
                uom_id: Set(line.uom_id),
                min_qty: Set(line.min_qty.unwrap_or(Decimal::ZERO)),
                unit_price: Set(line.unit_price),
                discount_pct: Set(line.discount_pct),
                created_at: Set(now),
                updated_at: Set(now),
            }
            .insert(&txn)
            .await?;
        }
        txn.commit().await?;
        self.lines(list_id).await
    }

    /// Delete a draft list; an active or archived one is history other
    /// rows may reference (customers, groups, document provenance) and
    /// archives instead.
    pub async fn delete(&self, id: Uuid, updated_by: Option<Uuid>) -> Result<price_list::Model> {
        let existing = self.find_by_id(id).await?;
        if ListStatus::parse(&existing.status)? != ListStatus::Draft {
            return self.archive(id, updated_by).await;
        }
        price_list::Entity::delete_by_id(id).exec(&self.db).await?;
        Ok(existing)
    }

    async fn validate(
        &self,
        body: &PriceListBody,
        existing: Option<&price_list::Model>,
    ) -> Result<(String, String)> {
        let name = body.name.trim().to_string();
        if name.is_empty() {
            return Err(Error::Validation("price list name must not be empty".into()));
        }
        let currency = body.currency.trim().to_uppercase();
        if currency.len() != 3 || !currency.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(Error::Validation(format!(
                "currency {:?} is not an ISO 4217 code",
                body.currency
            )));
        }
        if let (Some(from), Some(to)) = (body.valid_from, body.valid_to) {
            if to < from {
                return Err(Error::Validation(
                    "a price list cannot end before it starts".into(),
                ));
            }
        }
        let taken = price_list::Entity::find()
            .filter(price_list::Column::Name.eq(&name))
            .one(&self.db)
            .await?;
        if taken.is_some_and(|t| existing.is_none_or(|e| e.id != t.id)) {
            return Err(Error::Conflict(format!(
                "price list {name:?} already exists"
            )));
        }
        Ok((name, currency))
    }
}

// ---------------------------------------------------------------------------
// PricingService: the resolution chain
// ---------------------------------------------------------------------------

pub struct PricingService {
    db: DatabaseConnection,
}

/// A fully-specified price question.
pub struct PriceQuery {
    pub customer_id: Uuid,
    pub item_id: Uuid,
    pub qty: Decimal,
    /// `None` = the item's stock UoM.
    pub uom_id: Option<Uuid>,
    pub currency: String,
    pub date: chrono::NaiveDate,
}

impl PricingService {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Resolve the unit price for (customer, item, qty, uom, date):
    /// customer list → group list → active default lists (promotional
    /// first, then newest) → `item.selling_price`. Within a list the
    /// highest `min_qty ≤ qty` line wins; a line whose UoM matches the
    /// requested one beats a UoM-agnostic line. Lists in another
    /// currency, not active, or outside their validity window never
    /// match.
    pub async fn resolve(&self, q: PriceQuery) -> Result<ResolvedPrice> {
        if q.qty <= Decimal::ZERO {
            return Err(Error::Validation("quantity must be positive".into()));
        }
        let found = item::Entity::find_by_id(q.item_id)
            .one(&self.db)
            .await?
            .ok_or_else(|| Error::NotFound(format!("item {}", q.item_id)))?;
        if !found.is_sellable {
            return Err(Error::Validation(format!(
                "item {} is not sellable",
                found.sku
            )));
        }
        let buyer = customer::Entity::find_by_id(q.customer_id)
            .one(&self.db)
            .await?
            .ok_or_else(|| Error::NotFound(format!("customer {}", q.customer_id)))?;

        // The candidate lists, most specific first.
        let mut candidates: Vec<price_list::Model> = Vec::new();
        if let Some(list_id) = buyer.price_list_id {
            if let Some(list) = price_list::Entity::find_by_id(list_id).one(&self.db).await? {
                candidates.push(list);
            }
        }
        if let Some(group_id) = buyer.group_id {
            if let Some(g) = group::Entity::find_by_id(group_id).one(&self.db).await? {
                if let Some(list_id) = g.price_list_id {
                    if let Some(list) =
                        price_list::Entity::find_by_id(list_id).one(&self.db).await?
                    {
                        candidates.push(list);
                    }
                }
            }
        }
        let defaults = price_list::Entity::find()
            .filter(price_list::Column::Scope.eq(ListScope::Default.as_str()))
            .filter(price_list::Column::Status.eq(ListStatus::Active.as_str()))
            .order_by_desc(price_list::Column::IsPromotional)
            .order_by_desc(price_list::Column::CreatedAt)
            .all(&self.db)
            .await?;
        candidates.extend(defaults);

        for list in candidates {
            if !self.list_matches(&list, &q) {
                continue;
            }
            if let Some(priced) = self.best_line(&list, &found, &q).await? {
                return Ok(ResolvedPrice {
                    unit_price: priced,
                    source: PriceSource::List(list.id),
                    tax_inclusive: list.tax_inclusive,
                    currency: list.currency,
                });
            }
        }

        // Fall through to the item's own default selling price.
        match found.selling_price {
            Some(price) => Ok(ResolvedPrice {
                unit_price: price,
                source: PriceSource::ItemDefault,
                tax_inclusive: false,
                currency: q.currency,
            }),
            None => Err(Error::NotFound(format!(
                "no price for item {} — no list covers it and it has no default selling price",
                found.sku
            ))),
        }
    }

    /// Price one document line: a manual price when the caller supplied
    /// one (gated by `allow_override`, floored at the item's minimum
    /// selling price), the resolution chain otherwise. Returns the price
    /// with the provenance the line records.
    pub async fn price_line(
        &self,
        q: PriceQuery,
        manual: Option<Decimal>,
        allow_override: bool,
    ) -> Result<(Decimal, PriceSource)> {
        match manual {
            Some(price) => {
                if !allow_override {
                    return Err(Error::Validation(
                        "setting a line price by hand needs the price-override permission"
                            .into(),
                    ));
                }
                if price < Decimal::ZERO {
                    return Err(Error::Validation("unit price must not be negative".into()));
                }
                let found = item::Entity::find_by_id(q.item_id)
                    .one(&self.db)
                    .await?
                    .ok_or_else(|| Error::NotFound(format!("item {}", q.item_id)))?;
                if let Some(floor) = found.min_selling_price {
                    if price < floor {
                        return Err(Error::Validation(format!(
                            "price {price} for {} is below its minimum selling price {floor}",
                            found.sku
                        )));
                    }
                }
                Ok((price, PriceSource::Manual))
            }
            None => {
                let resolved = self.resolve(q).await?;
                Ok((resolved.unit_price, resolved.source))
            }
        }
    }

    /// Whether a list is live for this query at all.
    fn list_matches(&self, list: &price_list::Model, q: &PriceQuery) -> bool {
        list.status == ListStatus::Active.as_str()
            && list.currency == q.currency
            && list.valid_from.is_none_or(|from| from <= q.date)
            && list.valid_to.is_none_or(|to| q.date <= to)
    }

    /// The best line of a list for this item/qty/uom, priced out.
    async fn best_line(
        &self,
        list: &price_list::Model,
        found: &item::Model,
        q: &PriceQuery,
    ) -> Result<Option<Decimal>> {
        let lines = price_list_item::Entity::find()
            .filter(price_list_item::Column::PriceListId.eq(list.id))
            .filter(price_list_item::Column::ItemId.eq(q.item_id))
            .all(&self.db)
            .await?;
        let best = lines
            .into_iter()
            // A UoM-specific line only matches its own UoM; a
            // UoM-agnostic line matches anything.
            .filter(|l| l.uom_id.is_none() || l.uom_id == q.uom_id)
            .filter(|l| l.min_qty <= q.qty)
            // Highest break wins; an exact UoM match beats agnostic on
            // the same break.
            .max_by_key(|l| (l.min_qty, l.uom_id.is_some()));
        let Some(line) = best else {
            return Ok(None);
        };
        match (line.unit_price, line.discount_pct) {
            (Some(price), _) => Ok(Some(price)),
            (None, Some(pct)) => {
                let Some(base) = found.selling_price else {
                    // A discount line over an item that lost its default
                    // price prices nothing; fall through to other lists.
                    return Ok(None);
                };
                Ok(Some(
                    base - (base * pct / Decimal::ONE_HUNDRED).round_dp(6),
                ))
            }
            (None, None) => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

pub(crate) fn routes() -> Router {
    Router::new()
        .route("/sales/price-lists", get(list_lists).post(create_list))
        .route(
            "/sales/price-lists/{id}",
            get(get_list).put(update_list).delete(delete_list),
        )
        .route(
            "/sales/price-lists/{id}/items",
            get(get_lines).put(put_lines),
        )
        .route("/sales/price-lists/{id}/activate", post(activate_list))
        .route("/sales/price-lists/{id}/archive", post(archive_list))
        .route("/sales/pricing/resolve", get(resolve_price))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        list_lists,
        get_list,
        create_list,
        update_list,
        delete_list,
        get_lines,
        put_lines,
        activate_list,
        archive_list,
        resolve_price
    ),
    // Registered explicitly: enums referenced from bodies and the
    // resolve response must not dangle as $refs (the SerialStatus
    // lesson — NSwag chokes on unregistered schemas).
    components(schemas(ListScope, ListStatus, PriceSource))
)]
struct ApiDoc;

#[utoipa::path(get, path = "/sales/price-lists", tag = "sales",
    responses((status = 200, body = Vec<price_list::Model>)))]
async fn list_lists(
    authz: Authz,
    TenantDb(db): TenantDb,
) -> Result<Json<Vec<price_list::Model>>> {
    authz.require(names::PRICING_VIEW).await?;
    Store::new(db).find_all().await.map(Json)
}

#[utoipa::path(get, path = "/sales/price-lists/{id}", tag = "sales",
    params(("id" = Uuid, Path, description = "Price list id")),
    responses((status = 200, body = price_list::Model)))]
async fn get_list(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<price_list::Model>> {
    authz.require(names::PRICING_VIEW).await?;
    Store::new(db).find_by_id(id).await.map(Json)
}

#[utoipa::path(post, path = "/sales/price-lists", tag = "sales",
    request_body = PriceListBody,
    responses((status = 200, body = price_list::Model)))]
async fn create_list(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(body): Json<PriceListBody>,
) -> Result<Json<price_list::Model>> {
    authz.require(names::PRICING_MANAGE).await?;
    let row = Store::new(db).create(body, Some(authz.user.id)).await?;
    audit.0.created("scm.price_list", row.id, &row).await;
    Ok(Json(row))
}

#[utoipa::path(put, path = "/sales/price-lists/{id}", tag = "sales",
    params(("id" = Uuid, Path, description = "Price list id")),
    request_body = PriceListBody,
    responses((status = 200, body = price_list::Model)))]
async fn update_list(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(body): Json<PriceListBody>,
) -> Result<Json<price_list::Model>> {
    authz.require(names::PRICING_MANAGE).await?;
    let store = Store::new(db);
    let before = store.find_by_id(id).await?;
    let after = store.update(id, body, Some(authz.user.id)).await?;
    audit.0.updated("scm.price_list", id, &before, &after).await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/sales/price-lists/{id}", tag = "sales",
    params(("id" = Uuid, Path, description = "Price list id")),
    responses((status = 200, body = price_list::Model)))]
async fn delete_list(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<price_list::Model>> {
    authz.require(names::PRICING_MANAGE).await?;
    let row = Store::new(db).delete(id, Some(authz.user.id)).await?;
    audit.0.deleted("scm.price_list", id, &row).await;
    Ok(Json(row))
}

#[utoipa::path(get, path = "/sales/price-lists/{id}/items", tag = "sales",
    params(("id" = Uuid, Path, description = "Price list id")),
    responses((status = 200, body = Vec<price_list_item::Model>)))]
async fn get_lines(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<price_list_item::Model>>> {
    authz.require(names::PRICING_VIEW).await?;
    Store::new(db).lines(id).await.map(Json)
}

#[utoipa::path(put, path = "/sales/price-lists/{id}/items", tag = "sales",
    params(("id" = Uuid, Path, description = "Price list id")),
    request_body = PriceLinesBody,
    responses((status = 200, body = Vec<price_list_item::Model>)))]
async fn put_lines(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(body): Json<PriceLinesBody>,
) -> Result<Json<Vec<price_list_item::Model>>> {
    authz.require(names::PRICING_MANAGE).await?;
    let rows = Store::new(db).replace_lines(id, body).await?;
    audit
        .0
        .event(format!("replaced the lines of price list {id}"))
        .await;
    Ok(Json(rows))
}

#[utoipa::path(post, path = "/sales/price-lists/{id}/activate", tag = "sales",
    params(("id" = Uuid, Path, description = "Price list id")),
    responses((status = 200, body = price_list::Model)))]
async fn activate_list(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<price_list::Model>> {
    authz.require(names::PRICING_MANAGE).await?;
    let row = Store::new(db).activate(id, Some(authz.user.id)).await?;
    audit
        .0
        .event(format!("activated price list {}", row.name))
        .await;
    Ok(Json(row))
}

#[utoipa::path(post, path = "/sales/price-lists/{id}/archive", tag = "sales",
    params(("id" = Uuid, Path, description = "Price list id")),
    responses((status = 200, body = price_list::Model)))]
async fn archive_list(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<price_list::Model>> {
    authz.require(names::PRICING_MANAGE).await?;
    let row = Store::new(db).archive(id, Some(authz.user.id)).await?;
    audit
        .0
        .event(format!("archived price list {}", row.name))
        .await;
    Ok(Json(row))
}

#[utoipa::path(get, path = "/sales/pricing/resolve", tag = "sales",
    params(
        ("customer_id" = Uuid, Query, description = "Customer id"),
        ("item_id" = Uuid, Query, description = "Item id"),
        ("qty" = Option<String>, Query, description = "Quantity (default 1)"),
        ("uom_id" = Option<Uuid>, Query, description = "UoM (default: the item's stock UoM)"),
        ("currency" = Option<String>, Query, description = "Currency (default: the customer's)"),
        ("date" = Option<String>, Query, description = "Pricing date (default: today)")
    ),
    responses((status = 200, body = ResolvedPrice)))]
async fn resolve_price(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<ResolveQuery>,
) -> Result<Json<ResolvedPrice>> {
    authz.require(names::PRICING_VIEW).await?;
    let currency = match q.currency {
        Some(c) => c.trim().to_uppercase(),
        None => {
            customer::Entity::find_by_id(q.customer_id)
                .one(&db)
                .await?
                .ok_or_else(|| Error::NotFound(format!("customer {}", q.customer_id)))?
                .currency
        }
    };
    PricingService::new(db)
        .resolve(PriceQuery {
            customer_id: q.customer_id,
            item_id: q.item_id,
            qty: q.qty.unwrap_or(Decimal::ONE),
            uom_id: q.uom_id,
            currency,
            date: q.date.unwrap_or_else(|| chrono::Utc::now().date_naive()),
        })
        .await
        .map(Json)
}
