//! Warehouses: the places stock lives. One row per physical (or logical)
//! location; stock levels are kept per item × warehouse. A tenant gets a
//! default warehouse ("Main") seeded so movements work with zero setup —
//! `is_default` marks the one documents prefill, and at most one row may
//! carry it.

use crate::scm::inventory::permissions::names;
use axum::extract::Path;
use axum::routing::get;
use axum::{Json, Router};
use nebula::audit::Audit;
use nebula::auth::Authz;
use nebula::error::{Error, Result};
use nebula::{TenantDb, sea_orm};
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, QueryOrder, Set, TransactionTrait};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, utoipa::ToSchema)]
#[schema(as = InventoryWarehouse)]
#[sea_orm(table_name = "inventory_warehouses")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    #[sea_orm(unique)]
    pub code: String,
    pub name: String,
    /// standard|transit|scrap — only 'standard' is in use for now.
    pub warehouse_type: String,
    pub parent_id: Option<Uuid>,
    pub address_line1: Option<String>,
    pub address_line2: Option<String>,
    pub city: Option<String>,
    pub region: Option<String>,
    pub postal_code: Option<String>,
    pub country: Option<String>,
    pub phone: Option<String>,
    pub email: Option<String>,
    pub contact_name: Option<String>,
    /// The warehouse documents prefill; at most one.
    pub is_default: bool,
    pub allow_negative: bool,
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

/// Data access for warehouses on a given (tenant) connection.
pub struct Store {
    db: DatabaseConnection,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct WarehouseBody {
    pub code: String,
    pub name: String,
    #[serde(default = "default_type")]
    pub warehouse_type: String,
    pub parent_id: Option<Uuid>,
    pub address_line1: Option<String>,
    pub address_line2: Option<String>,
    pub city: Option<String>,
    pub region: Option<String>,
    pub postal_code: Option<String>,
    pub country: Option<String>,
    pub phone: Option<String>,
    pub email: Option<String>,
    pub contact_name: Option<String>,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub allow_negative: bool,
    #[serde(default = "yes")]
    pub is_active: bool,
    pub notes: Option<String>,
}

fn yes() -> bool {
    true
}

fn default_type() -> String {
    "standard".into()
}

const WAREHOUSE_TYPES: &[&str] = &["standard", "transit", "scrap"];

impl Store {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn find_all(&self) -> Result<Vec<Model>> {
        Entity::find()
            .order_by_asc(Column::Code)
            .all(&self.db)
            .await
            .map_err(Error::from)
    }

    pub async fn find_by_id(&self, id: Uuid) -> Result<Model> {
        Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or_else(|| Error::NotFound(format!("warehouse {id}")))
    }

    pub async fn create(&self, body: WarehouseBody, created_by: Option<Uuid>) -> Result<Model> {
        let (code, name) = self.validate(&body, None).await?;
        let now = chrono::Utc::now();
        let txn = self.db.begin().await?;
        if body.is_default {
            clear_default(&txn).await?;
        }
        let row = ActiveModel {
            id: Set(Uuid::new_v4()),
            code: Set(code),
            name: Set(name),
            warehouse_type: Set(body.warehouse_type),
            parent_id: Set(body.parent_id),
            address_line1: Set(body.address_line1.filter(|v| !v.trim().is_empty())),
            address_line2: Set(body.address_line2.filter(|v| !v.trim().is_empty())),
            city: Set(body.city.filter(|v| !v.trim().is_empty())),
            region: Set(body.region.filter(|v| !v.trim().is_empty())),
            postal_code: Set(body.postal_code.filter(|v| !v.trim().is_empty())),
            country: Set(body.country.filter(|v| !v.trim().is_empty())),
            phone: Set(body.phone.filter(|v| !v.trim().is_empty())),
            email: Set(body.email.filter(|v| !v.trim().is_empty())),
            contact_name: Set(body.contact_name.filter(|v| !v.trim().is_empty())),
            is_default: Set(body.is_default),
            allow_negative: Set(body.allow_negative),
            is_active: Set(body.is_active),
            notes: Set(body.notes.filter(|v| !v.trim().is_empty())),
            created_at: Set(now),
            created_by: Set(created_by),
            updated_at: Set(now),
            updated_by: Set(None),
        }
        .insert(&txn)
        .await?;
        txn.commit().await?;
        Ok(row)
    }

    pub async fn update(
        &self,
        id: Uuid,
        body: WarehouseBody,
        updated_by: Option<Uuid>,
    ) -> Result<Model> {
        let existing = self.find_by_id(id).await?;
        let (code, name) = self.validate(&body, Some(&existing)).await?;
        let txn = self.db.begin().await?;
        if body.is_default && !existing.is_default {
            clear_default(&txn).await?;
        }
        let mut active: ActiveModel = existing.into();
        active.code = Set(code);
        active.name = Set(name);
        active.warehouse_type = Set(body.warehouse_type);
        active.parent_id = Set(body.parent_id);
        active.address_line1 = Set(body.address_line1.filter(|v| !v.trim().is_empty()));
        active.address_line2 = Set(body.address_line2.filter(|v| !v.trim().is_empty()));
        active.city = Set(body.city.filter(|v| !v.trim().is_empty()));
        active.region = Set(body.region.filter(|v| !v.trim().is_empty()));
        active.postal_code = Set(body.postal_code.filter(|v| !v.trim().is_empty()));
        active.country = Set(body.country.filter(|v| !v.trim().is_empty()));
        active.phone = Set(body.phone.filter(|v| !v.trim().is_empty()));
        active.email = Set(body.email.filter(|v| !v.trim().is_empty()));
        active.contact_name = Set(body.contact_name.filter(|v| !v.trim().is_empty()));
        active.is_default = Set(body.is_default);
        active.allow_negative = Set(body.allow_negative);
        active.is_active = Set(body.is_active);
        active.notes = Set(body.notes.filter(|v| !v.trim().is_empty()));
        active.updated_at = Set(chrono::Utc::now());
        active.updated_by = Set(updated_by);
        let row = active.update(&txn).await?;
        txn.commit().await?;
        Ok(row)
    }

    async fn validate(
        &self,
        body: &WarehouseBody,
        existing: Option<&Model>,
    ) -> Result<(String, String)> {
        let code = body.code.trim().to_string();
        if code.is_empty() {
            return Err(Error::Validation("warehouse code must not be empty".into()));
        }
        let name = body.name.trim().to_string();
        if name.is_empty() {
            return Err(Error::Validation("warehouse name must not be empty".into()));
        }
        if !WAREHOUSE_TYPES.contains(&body.warehouse_type.as_str()) {
            return Err(Error::Validation(format!(
                "unknown warehouse type {:?} (expected standard, transit or scrap)",
                body.warehouse_type
            )));
        }
        let taken = Entity::find()
            .filter(Column::Code.eq(&code))
            .one(&self.db)
            .await?;
        if taken.is_some_and(|t| existing.is_none_or(|e| e.id != t.id)) {
            return Err(Error::Conflict(format!(
                "warehouse code {code:?} already exists"
            )));
        }
        if let Some(parent_id) = body.parent_id {
            if existing.is_some_and(|e| e.id == parent_id) {
                return Err(Error::Validation(
                    "a warehouse cannot be its own parent".into(),
                ));
            }
            self.find_by_id(parent_id).await?;
        }
        Ok((code, name))
    }
}

/// Demote the current default so a new one can take the flag (the partial
/// unique index allows at most one).
async fn clear_default<C: sea_orm::ConnectionTrait>(conn: &C) -> Result<()> {
    Entity::update_many()
        .col_expr(Column::IsDefault, Expr::value(false))
        .filter(Column::IsDefault.eq(true))
        .exec(conn)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

pub(crate) fn routes() -> Router {
    Router::new()
        .route(
            "/inventory/warehouses",
            get(list_warehouses).post(create_warehouse),
        )
        .route(
            "/inventory/warehouses/{id}",
            get(get_warehouse).put(update_warehouse),
        )
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(list_warehouses, get_warehouse, create_warehouse, update_warehouse))]
struct ApiDoc;

#[utoipa::path(get, path = "/inventory/warehouses", tag = "inventory",
    responses((status = 200, body = Vec<Model>)))]
async fn list_warehouses(authz: Authz, TenantDb(db): TenantDb) -> Result<Json<Vec<Model>>> {
    authz.require(names::WAREHOUSES_VIEW).await?;
    Store::new(db).find_all().await.map(Json)
}

#[utoipa::path(get, path = "/inventory/warehouses/{id}", tag = "inventory",
    params(("id" = Uuid, Path, description = "Warehouse id")),
    responses((status = 200, body = Model)))]
async fn get_warehouse(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<Model>> {
    authz.require(names::WAREHOUSES_VIEW).await?;
    Store::new(db).find_by_id(id).await.map(Json)
}

#[utoipa::path(post, path = "/inventory/warehouses", tag = "inventory",
    request_body = WarehouseBody,
    responses((status = 200, body = Model)))]
async fn create_warehouse(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(body): Json<WarehouseBody>,
) -> Result<Json<Model>> {
    authz.require(names::WAREHOUSES_MANAGE).await?;
    let row = Store::new(db).create(body, Some(authz.user.id)).await?;
    audit.0.created("scm.warehouse", row.id, &row).await;
    Ok(Json(row))
}

#[utoipa::path(put, path = "/inventory/warehouses/{id}", tag = "inventory",
    params(("id" = Uuid, Path, description = "Warehouse id")),
    request_body = WarehouseBody,
    responses((status = 200, body = Model)))]
async fn update_warehouse(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(body): Json<WarehouseBody>,
) -> Result<Json<Model>> {
    authz.require(names::WAREHOUSES_MANAGE).await?;
    let store = Store::new(db);
    let before = store.find_by_id(id).await?;
    let after = store.update(id, body, Some(authz.user.id)).await?;
    audit
        .0
        .updated("scm.warehouse", after.id, &before, &after)
        .await;
    Ok(Json(after))
}
