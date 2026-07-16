//! Suppliers: who we buy from, on what terms, and the item-supplier
//! catalog (who sells us what, under which SKU, at what price lately).
//!
//! `on_hold` is softer than deactivation: it blocks *new* purchase orders
//! while documents already in flight finish their lifecycle. Deleting a
//! supplier referenced by any order or invoice deactivates instead — the
//! paper trail keeps its name. Catalog rows are maintained two ways: by
//! hand through the nested endpoints, and automatically by goods receipts,
//! which stamp `last_price`/`last_purchased_on` on every posting.

use crate::scm::inventory::item::item;
use crate::scm::procurement::permissions::names;
use axum::extract::Path;
use axum::routing::get;
use axum::{Json, Router};
use nebula::audit::Audit;
use nebula::auth::Authz;
use nebula::error::{Error, Result};
use nebula::{TenantDb, sea_orm};
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, PaginatorTrait, QueryOrder, Set};
use serde::Deserialize;
use uuid::Uuid;

/// The supplier master.
pub mod supplier {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, utoipa::ToSchema)]
    #[schema(as = ProcurementSupplier)]
    #[sea_orm(table_name = "procurement_suppliers")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        #[sea_orm(unique)]
        pub code: String,
        pub name: String,
        pub legal_name: Option<String>,
        /// company|individual.
        pub supplier_type: String,
        pub registration_no: Option<String>,
        pub tax_number: Option<String>,
        pub industry: Option<String>,
        pub website: Option<String>,
        pub contact_name: Option<String>,
        pub email: Option<String>,
        pub phone: Option<String>,
        pub secondary_contact_name: Option<String>,
        pub secondary_email: Option<String>,
        pub secondary_phone: Option<String>,
        pub address_line1: Option<String>,
        pub address_line2: Option<String>,
        pub city: Option<String>,
        pub region: Option<String>,
        pub postal_code: Option<String>,
        pub country: Option<String>,
        /// ISO 4217; purchase orders default to this.
        pub currency: String,
        pub payment_terms_days: i32,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub credit_limit: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((7, 4)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub default_discount_pct: Option<Decimal>,
        pub default_tax_code_id: Option<Uuid>,
        pub incoterms: Option<String>,
        pub lead_time_days: Option<i32>,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub min_order_value: Option<Decimal>,
        pub bank_name: Option<String>,
        pub bank_branch: Option<String>,
        pub bank_account_name: Option<String>,
        pub bank_account_no: Option<String>,
        pub bank_swift: Option<String>,
        pub mobile_money_no: Option<String>,
        pub payment_notes: Option<String>,
        pub is_preferred: bool,
        /// Blocks new purchase orders; in-flight documents finish.
        pub on_hold: bool,
        pub hold_reason: Option<String>,
        #[sea_orm(column_type = "Decimal(Some((3, 2)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub rating: Option<Decimal>,
        pub is_active: bool,
        pub notes: Option<String>,
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

/// One row of the item-supplier catalog.
pub mod item_supplier {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, utoipa::ToSchema)]
    #[schema(as = ProcurementItemSupplier)]
    #[sea_orm(table_name = "procurement_item_suppliers")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub item_id: Uuid,
        pub supplier_id: Uuid,
        pub supplier_sku: Option<String>,
        pub supplier_item_name: Option<String>,
        pub purchase_uom_id: Option<Uuid>,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub pack_qty: Option<Decimal>,
        /// Supplier currency; goods receipts maintain it.
        #[sea_orm(column_type = "Decimal(Some((20, 6)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub last_price: Option<Decimal>,
        #[schema(value_type = Option<String>, format = Date)]
        pub last_purchased_on: Option<Date>,
        pub lead_time_days: Option<i32>,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub min_order_qty: Option<Decimal>,
        pub is_preferred: bool,
        pub is_active: bool,
        pub notes: Option<String>,
        #[schema(value_type = String, format = DateTime)]
        pub created_at: DateTimeUtc,
        #[schema(value_type = String, format = DateTime)]
        pub updated_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

const SUPPLIER_TYPES: &[&str] = &["company", "individual"];

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SupplierBody {
    pub code: String,
    pub name: String,
    pub legal_name: Option<String>,
    #[serde(default = "default_supplier_type")]
    pub supplier_type: String,
    pub registration_no: Option<String>,
    pub tax_number: Option<String>,
    pub industry: Option<String>,
    pub website: Option<String>,
    pub contact_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub secondary_contact_name: Option<String>,
    pub secondary_email: Option<String>,
    pub secondary_phone: Option<String>,
    pub address_line1: Option<String>,
    pub address_line2: Option<String>,
    pub city: Option<String>,
    pub region: Option<String>,
    pub postal_code: Option<String>,
    pub country: Option<String>,
    pub currency: String,
    #[serde(default)]
    pub payment_terms_days: i32,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub credit_limit: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub default_discount_pct: Option<Decimal>,
    pub default_tax_code_id: Option<Uuid>,
    pub incoterms: Option<String>,
    pub lead_time_days: Option<i32>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub min_order_value: Option<Decimal>,
    pub bank_name: Option<String>,
    pub bank_branch: Option<String>,
    pub bank_account_name: Option<String>,
    pub bank_account_no: Option<String>,
    pub bank_swift: Option<String>,
    pub mobile_money_no: Option<String>,
    pub payment_notes: Option<String>,
    #[serde(default)]
    pub is_preferred: bool,
    #[serde(default)]
    pub on_hold: bool,
    pub hold_reason: Option<String>,
    #[serde(default = "yes")]
    pub is_active: bool,
    pub notes: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ItemSupplierBody {
    pub item_id: Uuid,
    pub supplier_sku: Option<String>,
    pub supplier_item_name: Option<String>,
    pub purchase_uom_id: Option<Uuid>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub pack_qty: Option<Decimal>,
    pub lead_time_days: Option<i32>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub min_order_qty: Option<Decimal>,
    #[serde(default)]
    pub is_preferred: bool,
    #[serde(default = "yes")]
    pub is_active: bool,
    pub notes: Option<String>,
}

fn yes() -> bool {
    true
}

fn default_supplier_type() -> String {
    "company".into()
}

fn clean(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// Data access for suppliers on a given (tenant) connection.
pub struct Store {
    db: DatabaseConnection,
}

impl Store {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn find_all(&self) -> Result<Vec<supplier::Model>> {
        supplier::Entity::find()
            .order_by_asc(supplier::Column::Code)
            .all(&self.db)
            .await
            .map_err(Error::from)
    }

    pub async fn find_by_id(&self, id: Uuid) -> Result<supplier::Model> {
        supplier::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or_else(|| Error::NotFound(format!("supplier {id}")))
    }

    pub async fn create(
        &self,
        body: SupplierBody,
        created_by: Option<Uuid>,
    ) -> Result<supplier::Model> {
        let (code, name, currency) = self.validate(&body, None).await?;
        let now = chrono::Utc::now();
        supplier::ActiveModel {
            id: Set(Uuid::new_v4()),
            code: Set(code),
            name: Set(name),
            legal_name: Set(clean(body.legal_name)),
            supplier_type: Set(body.supplier_type),
            registration_no: Set(clean(body.registration_no)),
            tax_number: Set(clean(body.tax_number)),
            industry: Set(clean(body.industry)),
            website: Set(clean(body.website)),
            contact_name: Set(clean(body.contact_name)),
            email: Set(clean(body.email)),
            phone: Set(clean(body.phone)),
            secondary_contact_name: Set(clean(body.secondary_contact_name)),
            secondary_email: Set(clean(body.secondary_email)),
            secondary_phone: Set(clean(body.secondary_phone)),
            address_line1: Set(clean(body.address_line1)),
            address_line2: Set(clean(body.address_line2)),
            city: Set(clean(body.city)),
            region: Set(clean(body.region)),
            postal_code: Set(clean(body.postal_code)),
            country: Set(clean(body.country)),
            currency: Set(currency),
            payment_terms_days: Set(body.payment_terms_days),
            credit_limit: Set(body.credit_limit),
            default_discount_pct: Set(body.default_discount_pct),
            default_tax_code_id: Set(body.default_tax_code_id),
            incoterms: Set(clean(body.incoterms)),
            lead_time_days: Set(body.lead_time_days),
            min_order_value: Set(body.min_order_value),
            bank_name: Set(clean(body.bank_name)),
            bank_branch: Set(clean(body.bank_branch)),
            bank_account_name: Set(clean(body.bank_account_name)),
            bank_account_no: Set(clean(body.bank_account_no)),
            bank_swift: Set(clean(body.bank_swift)),
            mobile_money_no: Set(clean(body.mobile_money_no)),
            payment_notes: Set(clean(body.payment_notes)),
            is_preferred: Set(body.is_preferred),
            on_hold: Set(body.on_hold),
            hold_reason: Set(clean(body.hold_reason)),
            rating: Set(None),
            is_active: Set(body.is_active),
            notes: Set(clean(body.notes)),
            created_at: Set(now),
            created_by: Set(created_by),
            updated_at: Set(now),
            updated_by: Set(None),
        }
        .insert(&self.db)
        .await
        .map_err(Error::from)
    }

    pub async fn update(
        &self,
        id: Uuid,
        body: SupplierBody,
        updated_by: Option<Uuid>,
    ) -> Result<supplier::Model> {
        let existing = self.find_by_id(id).await?;
        let (code, name, currency) = self.validate(&body, Some(&existing)).await?;
        let mut active: supplier::ActiveModel = existing.into();
        active.code = Set(code);
        active.name = Set(name);
        active.legal_name = Set(clean(body.legal_name));
        active.supplier_type = Set(body.supplier_type);
        active.registration_no = Set(clean(body.registration_no));
        active.tax_number = Set(clean(body.tax_number));
        active.industry = Set(clean(body.industry));
        active.website = Set(clean(body.website));
        active.contact_name = Set(clean(body.contact_name));
        active.email = Set(clean(body.email));
        active.phone = Set(clean(body.phone));
        active.secondary_contact_name = Set(clean(body.secondary_contact_name));
        active.secondary_email = Set(clean(body.secondary_email));
        active.secondary_phone = Set(clean(body.secondary_phone));
        active.address_line1 = Set(clean(body.address_line1));
        active.address_line2 = Set(clean(body.address_line2));
        active.city = Set(clean(body.city));
        active.region = Set(clean(body.region));
        active.postal_code = Set(clean(body.postal_code));
        active.country = Set(clean(body.country));
        active.currency = Set(currency);
        active.payment_terms_days = Set(body.payment_terms_days);
        active.credit_limit = Set(body.credit_limit);
        active.default_discount_pct = Set(body.default_discount_pct);
        active.default_tax_code_id = Set(body.default_tax_code_id);
        active.incoterms = Set(clean(body.incoterms));
        active.lead_time_days = Set(body.lead_time_days);
        active.min_order_value = Set(body.min_order_value);
        active.bank_name = Set(clean(body.bank_name));
        active.bank_branch = Set(clean(body.bank_branch));
        active.bank_account_name = Set(clean(body.bank_account_name));
        active.bank_account_no = Set(clean(body.bank_account_no));
        active.bank_swift = Set(clean(body.bank_swift));
        active.mobile_money_no = Set(clean(body.mobile_money_no));
        active.payment_notes = Set(clean(body.payment_notes));
        active.is_preferred = Set(body.is_preferred);
        active.on_hold = Set(body.on_hold);
        active.hold_reason = Set(clean(body.hold_reason));
        active.is_active = Set(body.is_active);
        active.notes = Set(clean(body.notes));
        active.updated_at = Set(chrono::Utc::now());
        active.updated_by = Set(updated_by);
        active.update(&self.db).await.map_err(Error::from)
    }

    /// Delete a supplier — or deactivate it once purchase orders or
    /// invoices carry its name (history keeps its labels).
    pub async fn delete(&self, id: Uuid) -> Result<supplier::Model> {
        let existing = self.find_by_id(id).await?;
        let orders = super::order::order::Entity::find()
            .filter(super::order::order::Column::SupplierId.eq(id))
            .count(&self.db)
            .await?;
        let invoices = super::invoice::invoice::Entity::find()
            .filter(super::invoice::invoice::Column::SupplierId.eq(id))
            .count(&self.db)
            .await?;
        if orders > 0 || invoices > 0 {
            let mut active: supplier::ActiveModel = existing.into();
            active.is_active = Set(false);
            active.updated_at = Set(chrono::Utc::now());
            return active.update(&self.db).await.map_err(Error::from);
        }
        supplier::Entity::delete_by_id(id).exec(&self.db).await?;
        Ok(existing)
    }

    // -- catalog ------------------------------------------------------------

    pub async fn catalog(&self, supplier_id: Uuid) -> Result<Vec<item_supplier::Model>> {
        self.find_by_id(supplier_id).await?;
        item_supplier::Entity::find()
            .filter(item_supplier::Column::SupplierId.eq(supplier_id))
            .order_by_asc(item_supplier::Column::CreatedAt)
            .all(&self.db)
            .await
            .map_err(Error::from)
    }

    /// Create or update the catalog row for (item, supplier) — the pair is
    /// unique, so writing twice edits in place.
    pub async fn upsert_catalog(
        &self,
        supplier_id: Uuid,
        body: ItemSupplierBody,
    ) -> Result<item_supplier::Model> {
        self.find_by_id(supplier_id).await?;
        let found = item::Entity::find_by_id(body.item_id).one(&self.db).await?;
        let Some(found) = found else {
            return Err(Error::NotFound(format!("item {}", body.item_id)));
        };
        if !found.is_purchasable {
            return Err(Error::Validation(format!(
                "item {} is not purchasable",
                found.sku
            )));
        }
        if let Some(qty) = body.pack_qty {
            if qty <= Decimal::ZERO {
                return Err(Error::Validation("pack quantity must be positive".into()));
            }
        }
        let now = chrono::Utc::now();
        let existing = item_supplier::Entity::find()
            .filter(item_supplier::Column::SupplierId.eq(supplier_id))
            .filter(item_supplier::Column::ItemId.eq(body.item_id))
            .one(&self.db)
            .await?;
        match existing {
            Some(row) => {
                let mut active: item_supplier::ActiveModel = row.into();
                active.supplier_sku = Set(clean(body.supplier_sku));
                active.supplier_item_name = Set(clean(body.supplier_item_name));
                active.purchase_uom_id = Set(body.purchase_uom_id);
                active.pack_qty = Set(body.pack_qty);
                active.lead_time_days = Set(body.lead_time_days);
                active.min_order_qty = Set(body.min_order_qty);
                active.is_preferred = Set(body.is_preferred);
                active.is_active = Set(body.is_active);
                active.notes = Set(clean(body.notes));
                active.updated_at = Set(now);
                active.update(&self.db).await.map_err(Error::from)
            }
            None => item_supplier::ActiveModel {
                id: Set(Uuid::new_v4()),
                item_id: Set(body.item_id),
                supplier_id: Set(supplier_id),
                supplier_sku: Set(clean(body.supplier_sku)),
                supplier_item_name: Set(clean(body.supplier_item_name)),
                purchase_uom_id: Set(body.purchase_uom_id),
                pack_qty: Set(body.pack_qty),
                last_price: Set(None),
                last_purchased_on: Set(None),
                lead_time_days: Set(body.lead_time_days),
                min_order_qty: Set(body.min_order_qty),
                is_preferred: Set(body.is_preferred),
                is_active: Set(body.is_active),
                notes: Set(clean(body.notes)),
                created_at: Set(now),
                updated_at: Set(now),
            }
            .insert(&self.db)
            .await
            .map_err(Error::from),
        }
    }

    pub async fn remove_catalog(
        &self,
        supplier_id: Uuid,
        item_id: Uuid,
    ) -> Result<item_supplier::Model> {
        let row = item_supplier::Entity::find()
            .filter(item_supplier::Column::SupplierId.eq(supplier_id))
            .filter(item_supplier::Column::ItemId.eq(item_id))
            .one(&self.db)
            .await?
            .ok_or_else(|| {
                Error::NotFound(format!("catalog row for item {item_id} at this supplier"))
            })?;
        item_supplier::Entity::delete_by_id(row.id)
            .exec(&self.db)
            .await?;
        Ok(row)
    }

    async fn validate(
        &self,
        body: &SupplierBody,
        existing: Option<&supplier::Model>,
    ) -> Result<(String, String, String)> {
        let code = body.code.trim().to_string();
        if code.is_empty() {
            return Err(Error::Validation("supplier code must not be empty".into()));
        }
        let name = body.name.trim().to_string();
        if name.is_empty() {
            return Err(Error::Validation("supplier name must not be empty".into()));
        }
        if !SUPPLIER_TYPES.contains(&body.supplier_type.as_str()) {
            return Err(Error::Validation(format!(
                "unknown supplier type {:?} (expected company or individual)",
                body.supplier_type
            )));
        }
        let currency = body.currency.trim().to_uppercase();
        if currency.len() != 3 || !currency.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(Error::Validation(format!(
                "currency {:?} is not an ISO 4217 code",
                body.currency
            )));
        }
        if body.payment_terms_days < 0 {
            return Err(Error::Validation(
                "payment terms must not be negative".into(),
            ));
        }
        if let Some(pct) = body.default_discount_pct {
            if pct < Decimal::ZERO || pct > Decimal::ONE_HUNDRED {
                return Err(Error::Validation(
                    "default discount must be between 0 and 100 percent".into(),
                ));
            }
        }
        let taken = supplier::Entity::find()
            .filter(supplier::Column::Code.eq(&code))
            .one(&self.db)
            .await?;
        if taken.is_some_and(|t| existing.is_none_or(|e| e.id != t.id)) {
            return Err(Error::Conflict(format!(
                "supplier code {code:?} already exists"
            )));
        }
        Ok((code, name, currency))
    }
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

pub(crate) fn routes() -> Router {
    Router::new()
        .route(
            "/procurement/suppliers",
            get(list_suppliers).post(create_supplier),
        )
        .route(
            "/procurement/suppliers/{id}",
            get(get_supplier)
                .put(update_supplier)
                .delete(delete_supplier),
        )
        .route(
            "/procurement/suppliers/{id}/items",
            get(list_catalog).post(upsert_catalog),
        )
        .route(
            "/procurement/suppliers/{id}/items/{item_id}",
            axum::routing::delete(remove_catalog),
        )
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    list_suppliers,
    get_supplier,
    create_supplier,
    update_supplier,
    delete_supplier,
    list_catalog,
    upsert_catalog,
    remove_catalog
))]
struct ApiDoc;

#[utoipa::path(get, path = "/procurement/suppliers", tag = "procurement",
    responses((status = 200, body = Vec<supplier::Model>)))]
async fn list_suppliers(
    authz: Authz,
    TenantDb(db): TenantDb,
) -> Result<Json<Vec<supplier::Model>>> {
    authz.require(names::SUPPLIERS_VIEW).await?;
    Store::new(db).find_all().await.map(Json)
}

#[utoipa::path(get, path = "/procurement/suppliers/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "Supplier id")),
    responses((status = 200, body = supplier::Model)))]
async fn get_supplier(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<supplier::Model>> {
    authz.require(names::SUPPLIERS_VIEW).await?;
    Store::new(db).find_by_id(id).await.map(Json)
}

#[utoipa::path(post, path = "/procurement/suppliers", tag = "procurement",
    request_body = SupplierBody,
    responses((status = 200, body = supplier::Model)))]
async fn create_supplier(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(body): Json<SupplierBody>,
) -> Result<Json<supplier::Model>> {
    authz.require(names::SUPPLIERS_CREATE).await?;
    let row = Store::new(db).create(body, Some(authz.user.id)).await?;
    audit.0.created("scm.supplier", row.id, &row).await;
    Ok(Json(row))
}

#[utoipa::path(put, path = "/procurement/suppliers/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "Supplier id")),
    request_body = SupplierBody,
    responses((status = 200, body = supplier::Model)))]
async fn update_supplier(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(body): Json<SupplierBody>,
) -> Result<Json<supplier::Model>> {
    authz.require(names::SUPPLIERS_EDIT).await?;
    let store = Store::new(db);
    let before = store.find_by_id(id).await?;
    let after = store.update(id, body, Some(authz.user.id)).await?;
    audit.0.updated("scm.supplier", id, &before, &after).await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/procurement/suppliers/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "Supplier id")),
    responses((status = 200, body = supplier::Model)))]
async fn delete_supplier(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<supplier::Model>> {
    authz.require(names::SUPPLIERS_DELETE).await?;
    let row = Store::new(db).delete(id).await?;
    audit.0.deleted("scm.supplier", id, &row).await;
    Ok(Json(row))
}

#[utoipa::path(get, path = "/procurement/suppliers/{id}/items", tag = "procurement",
    params(("id" = Uuid, Path, description = "Supplier id")),
    responses((status = 200, body = Vec<item_supplier::Model>)))]
async fn list_catalog(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<item_supplier::Model>>> {
    authz.require(names::SUPPLIERS_VIEW).await?;
    Store::new(db).catalog(id).await.map(Json)
}

#[utoipa::path(post, path = "/procurement/suppliers/{id}/items", tag = "procurement",
    params(("id" = Uuid, Path, description = "Supplier id")),
    request_body = ItemSupplierBody,
    responses((status = 200, body = item_supplier::Model)))]
async fn upsert_catalog(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(body): Json<ItemSupplierBody>,
) -> Result<Json<item_supplier::Model>> {
    authz.require(names::SUPPLIERS_EDIT).await?;
    let row = Store::new(db).upsert_catalog(id, body).await?;
    audit.0.created("scm.item_supplier", row.id, &row).await;
    Ok(Json(row))
}

#[utoipa::path(delete, path = "/procurement/suppliers/{id}/items/{item_id}", tag = "procurement",
    params(
        ("id" = Uuid, Path, description = "Supplier id"),
        ("item_id" = Uuid, Path, description = "Item id")
    ),
    responses((status = 200, body = item_supplier::Model)))]
async fn remove_catalog(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path((id, item_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<item_supplier::Model>> {
    authz.require(names::SUPPLIERS_EDIT).await?;
    let row = Store::new(db).remove_catalog(id, item_id).await?;
    audit.0.deleted("scm.item_supplier", row.id, &row).await;
    Ok(Json(row))
}
