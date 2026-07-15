//! The item master: everything the business stocks, consumes, buys or
//! sells, plus its taxonomy (categories) and units of measure.
//!
//! Masters are wide from day one — every column a mature ERP keeps is
//! stored and exposed through the DTOs immediately, nullable where the
//! owning feature has not shipped. Storage is not enforcement: an item can
//! carry `track_batches` today even though posting only starts demanding
//! batch numbers when that phase lands. Soft references (tax codes,
//! preferred supplier, default warehouse) carry no FK and are validated
//! here in the service layer.

use crate::scm::inventory::permissions::names;
use axum::extract::{Path, Query};
use axum::routing::get;
use axum::{Json, Router};
use nebula::audit::Audit;
use nebula::auth::Authz;
use nebula::error::{Error, Result};
use nebula::{TenantDb, sea_orm};
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, QueryOrder, Set};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// What kind of thing an item is, which decides whether the stock ledger
/// tracks it: stockable moves through the ledger, a consumable is expensed
/// on receipt, a service never has stock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemType {
    Stockable,
    Consumable,
    Service,
}

impl ItemType {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemType::Stockable => "stockable",
            ItemType::Consumable => "consumable",
            ItemType::Service => "service",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "stockable" => Ok(ItemType::Stockable),
            "consumable" => Ok(ItemType::Consumable),
            "service" => Ok(ItemType::Service),
            other => Err(Error::Validation(format!(
                "unknown item type {other:?} (expected stockable, consumable or service)"
            ))),
        }
    }
}

/// How issues out of stock are costed. The schema knows all three; only
/// moving average is implemented, so the others are rejected at the door
/// until their phases land.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CostingMethod {
    MovingAverage,
    Fifo,
    Standard,
}

impl CostingMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            CostingMethod::MovingAverage => "moving_average",
            CostingMethod::Fifo => "fifo",
            CostingMethod::Standard => "standard",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "moving_average" => Ok(CostingMethod::MovingAverage),
            "fifo" => Ok(CostingMethod::Fifo),
            "standard" => Ok(CostingMethod::Standard),
            other => Err(Error::Validation(format!(
                "unknown costing method {other:?} (expected moving_average, fifo or standard)"
            ))),
        }
    }
}

/// Item category: a hierarchical grouping carrying the defaults an item
/// inherits when its own columns are NULL.
pub mod category {
    use nebula::sea_orm;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, utoipa::ToSchema)]
    #[schema(as = InventoryCategory)]
    #[sea_orm(table_name = "inventory_categories")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub code: Option<String>,
        pub name: String,
        pub description: Option<String>,
        pub parent_id: Option<Uuid>,
        pub default_costing_method: Option<String>,
        pub default_uom_id: Option<Uuid>,
        pub inventory_account_role: Option<String>,
        pub cogs_account_role: Option<String>,
        pub adjustment_account_role: Option<String>,
        pub is_active: bool,
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

/// Unit of measure: the granularity quantities are entered and stocked in.
pub mod uom {
    use nebula::sea_orm;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, utoipa::ToSchema)]
    #[schema(as = InventoryUom)]
    #[sea_orm(table_name = "inventory_uoms")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        #[sea_orm(unique)]
        pub code: String,
        pub name: String,
        pub symbol: Option<String>,
        /// Whether quantities in this UoM may carry decimals (kg yes, unit no).
        pub fractional: bool,
        pub is_active: bool,
        #[schema(value_type = String, format = DateTime)]
        pub created_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// Generic conversion between two UoMs (1 box = 12 unit). Stored from day
/// one; consumed when purchase-UoM conversions ship.
pub mod uom_conversion {
    use nebula::sea_orm;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "inventory_uom_conversions")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub from_uom_id: Uuid,
        pub to_uom_id: Uuid,
        #[sea_orm(column_type = "Decimal(Some((20, 8)))")]
        pub factor: Decimal,
        pub created_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// The item itself. Wide by design; see the module doc.
pub mod item {
    use nebula::sea_orm;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, utoipa::ToSchema)]
    #[schema(as = InventoryItem)]
    #[sea_orm(table_name = "inventory_items")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,

        // Identity
        #[sea_orm(unique)]
        pub sku: String,
        pub name: String,
        pub description: Option<String>,
        pub category_id: Option<Uuid>,
        pub brand: Option<String>,
        pub manufacturer: Option<String>,
        pub manufacturer_part_no: Option<String>,
        pub model: Option<String>,
        pub barcode: Option<String>,
        pub image_file_id: Option<Uuid>,
        pub country_of_origin: Option<String>,
        pub hs_code: Option<String>,
        pub notes: Option<String>,

        // Classification & roles
        pub item_type: String,
        pub is_purchasable: bool,
        pub is_sellable: bool,
        pub is_active: bool,

        // Units
        pub uom_id: Uuid,
        pub purchase_uom_id: Option<Uuid>,
        pub sales_uom_id: Option<Uuid>,
        #[sea_orm(column_type = "Decimal(Some((20, 8)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub purchase_uom_factor: Option<Decimal>,

        // Costing & pricing
        pub costing_method: String,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub standard_cost: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub purchase_price: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub last_purchase_price: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub selling_price: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub min_selling_price: Option<Decimal>,
        pub purchase_tax_code_id: Option<Uuid>,
        pub sales_tax_code_id: Option<Uuid>,

        // Procurement planning
        pub preferred_supplier_id: Option<Uuid>,
        pub lead_time_days: Option<i32>,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub min_order_qty: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub order_multiple: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub reorder_level: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub reorder_qty: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub max_level: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub safety_stock: Option<Decimal>,

        // Stock control (stored now, enforced by their feature phases)
        pub default_warehouse_id: Option<Uuid>,
        pub track_batches: bool,
        pub track_serials: bool,
        pub shelf_life_days: Option<i32>,
        pub warranty_days: Option<i32>,
        pub allow_negative: bool,

        // Physical attributes
        #[sea_orm(column_type = "Decimal(Some((20, 6)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub weight: Option<Decimal>,
        pub weight_uom_id: Option<Uuid>,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub volume: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 2)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub length_mm: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 2)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub width_mm: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 2)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub height_mm: Option<Decimal>,

        // GL role overrides (NULL = inherit category, then tenant default)
        pub inventory_account_role: Option<String>,
        pub cogs_account_role: Option<String>,
        pub adjustment_account_role: Option<String>,
        pub expense_account_role: Option<String>,

        // Audit
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

/// Extra scan codes for an item (case barcodes, legacy codes).
pub mod item_barcode {
    use nebula::sea_orm;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "inventory_item_barcodes")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub item_id: Uuid,
        pub barcode: String,
        pub uom_id: Option<Uuid>,
        pub created_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

// ---------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------

/// All writable item fields, shared by create and update (PUT semantics:
/// the body is the item's full intended state).
#[derive(Deserialize, utoipa::ToSchema)]
pub struct ItemBody {
    pub sku: String,
    pub name: String,
    pub description: Option<String>,
    pub category_id: Option<Uuid>,
    pub brand: Option<String>,
    pub manufacturer: Option<String>,
    pub manufacturer_part_no: Option<String>,
    pub model: Option<String>,
    pub barcode: Option<String>,
    pub image_file_id: Option<Uuid>,
    pub country_of_origin: Option<String>,
    pub hs_code: Option<String>,
    pub notes: Option<String>,
    #[serde(default = "default_item_type")]
    pub item_type: ItemType,
    #[serde(default = "yes")]
    pub is_purchasable: bool,
    #[serde(default = "yes")]
    pub is_sellable: bool,
    #[serde(default = "yes")]
    pub is_active: bool,
    pub uom_id: Uuid,
    pub purchase_uom_id: Option<Uuid>,
    pub sales_uom_id: Option<Uuid>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub purchase_uom_factor: Option<Decimal>,
    #[serde(default = "default_costing_method")]
    pub costing_method: CostingMethod,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub standard_cost: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub purchase_price: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub selling_price: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub min_selling_price: Option<Decimal>,
    pub purchase_tax_code_id: Option<Uuid>,
    pub sales_tax_code_id: Option<Uuid>,
    pub preferred_supplier_id: Option<Uuid>,
    pub lead_time_days: Option<i32>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub min_order_qty: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub order_multiple: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub reorder_level: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub reorder_qty: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub max_level: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub safety_stock: Option<Decimal>,
    pub default_warehouse_id: Option<Uuid>,
    #[serde(default)]
    pub track_batches: bool,
    #[serde(default)]
    pub track_serials: bool,
    pub shelf_life_days: Option<i32>,
    pub warranty_days: Option<i32>,
    #[serde(default)]
    pub allow_negative: bool,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub weight: Option<Decimal>,
    pub weight_uom_id: Option<Uuid>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub volume: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub length_mm: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub width_mm: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub height_mm: Option<Decimal>,
    pub inventory_account_role: Option<String>,
    pub cogs_account_role: Option<String>,
    pub adjustment_account_role: Option<String>,
    pub expense_account_role: Option<String>,
}

fn yes() -> bool {
    true
}

fn default_item_type() -> ItemType {
    ItemType::Stockable
}

fn default_costing_method() -> CostingMethod {
    CostingMethod::MovingAverage
}

/// Data access for items, categories and UoMs on a (tenant) connection.
pub struct Store {
    db: DatabaseConnection,
}

impl Store {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    // -- items --------------------------------------------------------------

    pub async fn list_items(
        &self,
        q: Option<&str>,
        category_id: Option<Uuid>,
        active: Option<bool>,
    ) -> Result<Vec<item::Model>> {
        let mut find = item::Entity::find().order_by_asc(item::Column::Sku);
        if let Some(q) = q.map(str::trim).filter(|q| !q.is_empty()) {
            let pattern = format!("%{q}%");
            find = find.filter(
                item::Column::Sku
                    .contains(q)
                    .or(item::Column::Name.like(&pattern))
                    .or(item::Column::Barcode.eq(q)),
            );
        }
        if let Some(category_id) = category_id {
            find = find.filter(item::Column::CategoryId.eq(category_id));
        }
        if let Some(active) = active {
            find = find.filter(item::Column::IsActive.eq(active));
        }
        find.all(&self.db).await.map_err(Error::from)
    }

    pub async fn find_item(&self, id: Uuid) -> Result<item::Model> {
        item::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or_else(|| Error::NotFound(format!("item {id}")))
    }

    pub async fn create_item(&self, body: ItemBody, created_by: Option<Uuid>) -> Result<item::Model> {
        let (sku, name) = self.validate_item(&body, None).await?;
        let now = chrono::Utc::now();
        let mut active = item_active_model(body, now);
        active.id = Set(Uuid::new_v4());
        active.sku = Set(sku);
        active.name = Set(name);
        active.created_at = Set(now);
        active.created_by = Set(created_by);
        active.insert(&self.db).await.map_err(Error::from)
    }

    pub async fn update_item(
        &self,
        id: Uuid,
        body: ItemBody,
        updated_by: Option<Uuid>,
    ) -> Result<item::Model> {
        let existing = self.find_item(id).await?;
        let (sku, name) = self.validate_item(&body, Some(&existing)).await?;
        let now = chrono::Utc::now();
        let mut active = item_active_model(body, now);
        active.id = Set(existing.id);
        active.sku = Set(sku);
        active.name = Set(name);
        // Maintained by receipts, not the editor.
        active.last_purchase_price = Set(existing.last_purchase_price);
        active.created_at = Set(existing.created_at);
        active.created_by = Set(existing.created_by);
        active.updated_by = Set(updated_by);
        active.update(&self.db).await.map_err(Error::from)
    }

    /// Delete an item, or deactivate it when it has stock history — the
    /// ledger must stay referentially whole.
    pub async fn delete_item(&self, id: Uuid) -> Result<item::Model> {
        let existing = self.find_item(id).await?;
        let moved = super::stock::ledger::Entity::find()
            .filter(super::stock::ledger::Column::ItemId.eq(id))
            .count(&self.db)
            .await?;
        if moved > 0 {
            let mut active: item::ActiveModel = existing.into();
            active.is_active = Set(false);
            active.updated_at = Set(chrono::Utc::now());
            return active.update(&self.db).await.map_err(Error::from);
        }
        item::Entity::delete_by_id(id).exec(&self.db).await?;
        Ok(existing)
    }

    /// Shared create/update validation. Returns the trimmed sku and name.
    async fn validate_item(
        &self,
        body: &ItemBody,
        existing: Option<&item::Model>,
    ) -> Result<(String, String)> {
        let sku = body.sku.trim().to_string();
        if sku.is_empty() {
            return Err(Error::Validation("item sku must not be empty".into()));
        }
        let name = body.name.trim().to_string();
        if name.is_empty() {
            return Err(Error::Validation("item name must not be empty".into()));
        }
        if body.costing_method != CostingMethod::MovingAverage {
            return Err(Error::Validation(format!(
                "costing method {:?} is not available yet; only moving_average is supported",
                body.costing_method.as_str()
            )));
        }

        let taken = item::Entity::find()
            .filter(item::Column::Sku.eq(&sku))
            .one(&self.db)
            .await?;
        if taken.is_some_and(|t| existing.is_none_or(|e| e.id != t.id)) {
            return Err(Error::Conflict(format!("item sku {sku:?} already exists")));
        }
        if let Some(barcode) = body.barcode.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
            let taken = item::Entity::find()
                .filter(item::Column::Barcode.eq(barcode))
                .one(&self.db)
                .await?;
            if taken.is_some_and(|t| existing.is_none_or(|e| e.id != t.id)) {
                return Err(Error::Conflict(format!(
                    "barcode {barcode:?} is already on another item"
                )));
            }
        }

        self.ensure_uom(body.uom_id).await?;
        for uom_id in [body.purchase_uom_id, body.sales_uom_id, body.weight_uom_id]
            .into_iter()
            .flatten()
        {
            self.ensure_uom(uom_id).await?;
        }
        if let Some(category_id) = body.category_id {
            self.find_category(category_id).await?;
        }
        if let Some(warehouse_id) = body.default_warehouse_id {
            let found = super::warehouse::Entity::find_by_id(warehouse_id)
                .one(&self.db)
                .await?;
            if found.is_none() {
                return Err(Error::Validation(format!(
                    "default warehouse {warehouse_id} does not exist"
                )));
            }
        }
        if let Some(supplier_id) = body.preferred_supplier_id {
            let found = crate::scm::procurement::supplier::supplier::Entity::find_by_id(supplier_id)
                .one(&self.db)
                .await?;
            if found.is_none() {
                return Err(Error::Validation(format!(
                    "preferred supplier {supplier_id} does not exist"
                )));
            }
        }

        for (label, value) in [
            ("purchase_uom_factor", body.purchase_uom_factor),
            ("standard_cost", body.standard_cost),
            ("purchase_price", body.purchase_price),
            ("selling_price", body.selling_price),
            ("min_selling_price", body.min_selling_price),
            ("min_order_qty", body.min_order_qty),
            ("order_multiple", body.order_multiple),
            ("reorder_level", body.reorder_level),
            ("reorder_qty", body.reorder_qty),
            ("max_level", body.max_level),
            ("safety_stock", body.safety_stock),
            ("weight", body.weight),
            ("volume", body.volume),
        ] {
            if value.is_some_and(|v| v < Decimal::ZERO) {
                return Err(Error::Validation(format!("{label} must not be negative")));
            }
        }
        Ok((sku, name))
    }

    async fn ensure_uom(&self, id: Uuid) -> Result<uom::Model> {
        uom::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .filter(|u| u.is_active)
            .ok_or_else(|| Error::Validation(format!("unit of measure {id} does not exist")))
    }

    // -- categories ----------------------------------------------------------

    pub async fn list_categories(&self) -> Result<Vec<category::Model>> {
        category::Entity::find()
            .order_by_asc(category::Column::Name)
            .all(&self.db)
            .await
            .map_err(Error::from)
    }

    pub async fn find_category(&self, id: Uuid) -> Result<category::Model> {
        category::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or_else(|| Error::NotFound(format!("category {id}")))
    }

    pub async fn create_category(
        &self,
        body: CategoryBody,
        created_by: Option<Uuid>,
    ) -> Result<category::Model> {
        let name = self.validate_category(&body, None).await?;
        let now = chrono::Utc::now();
        category::ActiveModel {
            id: Set(Uuid::new_v4()),
            code: Set(body.code.map(|c| c.trim().to_string()).filter(|c| !c.is_empty())),
            name: Set(name),
            description: Set(body.description.filter(|d| !d.trim().is_empty())),
            parent_id: Set(body.parent_id),
            default_costing_method: Set(body.default_costing_method.map(|m| m.as_str().to_string())),
            default_uom_id: Set(body.default_uom_id),
            inventory_account_role: Set(body.inventory_account_role),
            cogs_account_role: Set(body.cogs_account_role),
            adjustment_account_role: Set(body.adjustment_account_role),
            is_active: Set(body.is_active),
            created_at: Set(now),
            created_by: Set(created_by),
            updated_at: Set(now),
            updated_by: Set(None),
        }
        .insert(&self.db)
        .await
        .map_err(Error::from)
    }

    pub async fn update_category(
        &self,
        id: Uuid,
        body: CategoryBody,
        updated_by: Option<Uuid>,
    ) -> Result<category::Model> {
        let existing = self.find_category(id).await?;
        let name = self.validate_category(&body, Some(&existing)).await?;
        if let Some(parent_id) = body.parent_id {
            self.ensure_no_category_cycle(id, parent_id).await?;
        }
        let mut active: category::ActiveModel = existing.into();
        active.code = Set(body.code.map(|c| c.trim().to_string()).filter(|c| !c.is_empty()));
        active.name = Set(name);
        active.description = Set(body.description.filter(|d| !d.trim().is_empty()));
        active.parent_id = Set(body.parent_id);
        active.default_costing_method =
            Set(body.default_costing_method.map(|m| m.as_str().to_string()));
        active.default_uom_id = Set(body.default_uom_id);
        active.inventory_account_role = Set(body.inventory_account_role);
        active.cogs_account_role = Set(body.cogs_account_role);
        active.adjustment_account_role = Set(body.adjustment_account_role);
        active.is_active = Set(body.is_active);
        active.updated_at = Set(chrono::Utc::now());
        active.updated_by = Set(updated_by);
        active.update(&self.db).await.map_err(Error::from)
    }

    pub async fn delete_category(&self, id: Uuid) -> Result<category::Model> {
        let existing = self.find_category(id).await?;
        let children = category::Entity::find()
            .filter(category::Column::ParentId.eq(id))
            .count(&self.db)
            .await?;
        if children > 0 {
            return Err(Error::Validation(
                "category has sub-categories and cannot be deleted".into(),
            ));
        }
        let items = item::Entity::find()
            .filter(item::Column::CategoryId.eq(id))
            .count(&self.db)
            .await?;
        if items > 0 {
            return Err(Error::Validation(
                "category has items and cannot be deleted; move or recategorize them first".into(),
            ));
        }
        category::Entity::delete_by_id(id).exec(&self.db).await?;
        Ok(existing)
    }

    async fn validate_category(
        &self,
        body: &CategoryBody,
        existing: Option<&category::Model>,
    ) -> Result<String> {
        let name = body.name.trim().to_string();
        if name.is_empty() {
            return Err(Error::Validation("category name must not be empty".into()));
        }
        let taken = category::Entity::find()
            .filter(category::Column::Name.eq(&name))
            .one(&self.db)
            .await?;
        if taken.is_some_and(|t| existing.is_none_or(|e| e.id != t.id)) {
            return Err(Error::Conflict(format!(
                "category name {name:?} already exists"
            )));
        }
        if let Some(parent_id) = body.parent_id {
            if existing.is_some_and(|e| e.id == parent_id) {
                return Err(Error::Validation("a category cannot be its own parent".into()));
            }
            self.find_category(parent_id).await?;
        }
        if let Some(uom_id) = body.default_uom_id {
            self.ensure_uom(uom_id).await?;
        }
        Ok(name)
    }

    /// Walk up from `parent_id`; hitting `id` on the way means the edit
    /// would close a loop in the tree.
    async fn ensure_no_category_cycle(&self, id: Uuid, parent_id: Uuid) -> Result<()> {
        let mut cursor = Some(parent_id);
        while let Some(current) = cursor {
            if current == id {
                return Err(Error::Validation(
                    "category parent would create a cycle".into(),
                ));
            }
            cursor = self.find_category(current).await?.parent_id;
        }
        Ok(())
    }

    // -- units of measure ----------------------------------------------------

    pub async fn list_uoms(&self) -> Result<Vec<uom::Model>> {
        uom::Entity::find()
            .order_by_asc(uom::Column::Code)
            .all(&self.db)
            .await
            .map_err(Error::from)
    }

    pub async fn create_uom(&self, body: UomBody) -> Result<uom::Model> {
        let code = body.code.trim().to_string();
        if code.is_empty() {
            return Err(Error::Validation("uom code must not be empty".into()));
        }
        if body.name.trim().is_empty() {
            return Err(Error::Validation("uom name must not be empty".into()));
        }
        let taken = uom::Entity::find()
            .filter(uom::Column::Code.eq(&code))
            .count(&self.db)
            .await?;
        if taken > 0 {
            return Err(Error::Conflict(format!("uom code {code:?} already exists")));
        }
        uom::ActiveModel {
            id: Set(Uuid::new_v4()),
            code: Set(code),
            name: Set(body.name.trim().to_string()),
            symbol: Set(body.symbol.filter(|s| !s.trim().is_empty())),
            fractional: Set(body.fractional),
            is_active: Set(true),
            created_at: Set(chrono::Utc::now()),
        }
        .insert(&self.db)
        .await
        .map_err(Error::from)
    }
}

/// Build an ActiveModel from the shared body; identity/audit columns are
/// overridden by the caller.
fn item_active_model(body: ItemBody, now: chrono::DateTime<chrono::Utc>) -> item::ActiveModel {
    item::ActiveModel {
        id: Set(Uuid::nil()),
        sku: Set(String::new()),
        name: Set(String::new()),
        description: Set(body.description.filter(|d| !d.trim().is_empty())),
        category_id: Set(body.category_id),
        brand: Set(body.brand.filter(|v| !v.trim().is_empty())),
        manufacturer: Set(body.manufacturer.filter(|v| !v.trim().is_empty())),
        manufacturer_part_no: Set(body.manufacturer_part_no.filter(|v| !v.trim().is_empty())),
        model: Set(body.model.filter(|v| !v.trim().is_empty())),
        barcode: Set(body.barcode.map(|b| b.trim().to_string()).filter(|b| !b.is_empty())),
        image_file_id: Set(body.image_file_id),
        country_of_origin: Set(body.country_of_origin.filter(|v| !v.trim().is_empty())),
        hs_code: Set(body.hs_code.filter(|v| !v.trim().is_empty())),
        notes: Set(body.notes.filter(|v| !v.trim().is_empty())),
        item_type: Set(body.item_type.as_str().to_string()),
        is_purchasable: Set(body.is_purchasable),
        is_sellable: Set(body.is_sellable),
        is_active: Set(body.is_active),
        uom_id: Set(body.uom_id),
        purchase_uom_id: Set(body.purchase_uom_id),
        sales_uom_id: Set(body.sales_uom_id),
        purchase_uom_factor: Set(body.purchase_uom_factor),
        costing_method: Set(body.costing_method.as_str().to_string()),
        standard_cost: Set(body.standard_cost),
        purchase_price: Set(body.purchase_price),
        last_purchase_price: Set(None),
        selling_price: Set(body.selling_price),
        min_selling_price: Set(body.min_selling_price),
        purchase_tax_code_id: Set(body.purchase_tax_code_id),
        sales_tax_code_id: Set(body.sales_tax_code_id),
        preferred_supplier_id: Set(body.preferred_supplier_id),
        lead_time_days: Set(body.lead_time_days),
        min_order_qty: Set(body.min_order_qty),
        order_multiple: Set(body.order_multiple),
        reorder_level: Set(body.reorder_level),
        reorder_qty: Set(body.reorder_qty),
        max_level: Set(body.max_level),
        safety_stock: Set(body.safety_stock),
        default_warehouse_id: Set(body.default_warehouse_id),
        track_batches: Set(body.track_batches),
        track_serials: Set(body.track_serials),
        shelf_life_days: Set(body.shelf_life_days),
        warranty_days: Set(body.warranty_days),
        allow_negative: Set(body.allow_negative),
        weight: Set(body.weight),
        weight_uom_id: Set(body.weight_uom_id),
        volume: Set(body.volume),
        length_mm: Set(body.length_mm),
        width_mm: Set(body.width_mm),
        height_mm: Set(body.height_mm),
        inventory_account_role: Set(body.inventory_account_role),
        cogs_account_role: Set(body.cogs_account_role),
        adjustment_account_role: Set(body.adjustment_account_role),
        expense_account_role: Set(body.expense_account_role),
        created_at: Set(now),
        created_by: Set(None),
        updated_at: Set(now),
        updated_by: Set(None),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CategoryBody {
    pub code: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub parent_id: Option<Uuid>,
    pub default_costing_method: Option<CostingMethod>,
    pub default_uom_id: Option<Uuid>,
    pub inventory_account_role: Option<String>,
    pub cogs_account_role: Option<String>,
    pub adjustment_account_role: Option<String>,
    #[serde(default = "yes")]
    pub is_active: bool,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct UomBody {
    pub code: String,
    pub name: String,
    pub symbol: Option<String>,
    #[serde(default)]
    pub fractional: bool,
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

pub(crate) fn routes() -> Router {
    Router::new()
        .route("/inventory/items", get(list_items).post(create_item))
        .route(
            "/inventory/items/{id}",
            get(get_item).put(update_item).delete(delete_item),
        )
        .route(
            "/inventory/categories",
            get(list_categories).post(create_category),
        )
        .route(
            "/inventory/categories/{id}",
            axum::routing::put(update_category).delete(delete_category),
        )
        .route("/inventory/uoms", get(list_uoms).post(create_uom))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    list_items,
    get_item,
    create_item,
    update_item,
    delete_item,
    list_categories,
    create_category,
    update_category,
    delete_category,
    list_uoms,
    create_uom
))]
struct ApiDoc;

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListItemsQuery {
    /// Matches sku (contains), name (contains) or barcode (exact).
    pub q: Option<String>,
    pub category_id: Option<Uuid>,
    pub active: Option<bool>,
}

#[utoipa::path(get, path = "/inventory/items", tag = "inventory",
    params(ListItemsQuery),
    responses((status = 200, body = Vec<item::Model>)))]
async fn list_items(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(query): Query<ListItemsQuery>,
) -> Result<Json<Vec<item::Model>>> {
    authz.require(names::ITEMS_VIEW).await?;
    Store::new(db)
        .list_items(query.q.as_deref(), query.category_id, query.active)
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/inventory/items/{id}", tag = "inventory",
    params(("id" = Uuid, Path, description = "Item id")),
    responses((status = 200, body = item::Model)))]
async fn get_item(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<item::Model>> {
    authz.require(names::ITEMS_VIEW).await?;
    Store::new(db).find_item(id).await.map(Json)
}

#[utoipa::path(post, path = "/inventory/items", tag = "inventory",
    request_body = ItemBody,
    responses((status = 200, body = item::Model)))]
async fn create_item(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(body): Json<ItemBody>,
) -> Result<Json<item::Model>> {
    authz.require(names::ITEMS_CREATE).await?;
    let row = Store::new(db).create_item(body, Some(authz.user.id)).await?;
    audit.0.created("scm.item", row.id, &row).await;
    Ok(Json(row))
}

#[utoipa::path(put, path = "/inventory/items/{id}", tag = "inventory",
    params(("id" = Uuid, Path, description = "Item id")),
    request_body = ItemBody,
    responses((status = 200, body = item::Model)))]
async fn update_item(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(body): Json<ItemBody>,
) -> Result<Json<item::Model>> {
    authz.require(names::ITEMS_EDIT).await?;
    let store = Store::new(db);
    let before = store.find_item(id).await?;
    let after = store.update_item(id, body, Some(authz.user.id)).await?;
    audit.0.updated("scm.item", after.id, &before, &after).await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/inventory/items/{id}", tag = "inventory",
    params(("id" = Uuid, Path, description = "Item id")),
    responses((status = 200, body = item::Model)))]
async fn delete_item(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<item::Model>> {
    authz.require(names::ITEMS_DELETE).await?;
    let row = Store::new(db).delete_item(id).await?;
    audit.0.deleted("scm.item", row.id, &row).await;
    Ok(Json(row))
}

#[utoipa::path(get, path = "/inventory/categories", tag = "inventory",
    responses((status = 200, body = Vec<category::Model>)))]
async fn list_categories(
    authz: Authz,
    TenantDb(db): TenantDb,
) -> Result<Json<Vec<category::Model>>> {
    authz.require(names::ITEMS_VIEW).await?;
    Store::new(db).list_categories().await.map(Json)
}

#[utoipa::path(post, path = "/inventory/categories", tag = "inventory",
    request_body = CategoryBody,
    responses((status = 200, body = category::Model)))]
async fn create_category(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(body): Json<CategoryBody>,
) -> Result<Json<category::Model>> {
    authz.require(names::ITEMS_CREATE).await?;
    let row = Store::new(db)
        .create_category(body, Some(authz.user.id))
        .await?;
    audit.0.created("scm.category", row.id, &row).await;
    Ok(Json(row))
}

#[utoipa::path(put, path = "/inventory/categories/{id}", tag = "inventory",
    params(("id" = Uuid, Path, description = "Category id")),
    request_body = CategoryBody,
    responses((status = 200, body = category::Model)))]
async fn update_category(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(body): Json<CategoryBody>,
) -> Result<Json<category::Model>> {
    authz.require(names::ITEMS_EDIT).await?;
    let store = Store::new(db);
    let before = store.find_category(id).await?;
    let after = store
        .update_category(id, body, Some(authz.user.id))
        .await?;
    audit
        .0
        .updated("scm.category", after.id, &before, &after)
        .await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/inventory/categories/{id}", tag = "inventory",
    params(("id" = Uuid, Path, description = "Category id")),
    responses((status = 200, body = category::Model)))]
async fn delete_category(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<category::Model>> {
    authz.require(names::ITEMS_DELETE).await?;
    let row = Store::new(db).delete_category(id).await?;
    audit.0.deleted("scm.category", row.id, &row).await;
    Ok(Json(row))
}

#[utoipa::path(get, path = "/inventory/uoms", tag = "inventory",
    responses((status = 200, body = Vec<uom::Model>)))]
async fn list_uoms(authz: Authz, TenantDb(db): TenantDb) -> Result<Json<Vec<uom::Model>>> {
    authz.require(names::ITEMS_VIEW).await?;
    Store::new(db).list_uoms().await.map(Json)
}

#[utoipa::path(post, path = "/inventory/uoms", tag = "inventory",
    request_body = UomBody,
    responses((status = 200, body = uom::Model)))]
async fn create_uom(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(body): Json<UomBody>,
) -> Result<Json<uom::Model>> {
    authz.require(names::ITEMS_CREATE).await?;
    let row = Store::new(db).create_uom(body).await?;
    audit.0.created("scm.uom", row.id, &row).await;
    Ok(Json(row))
}
