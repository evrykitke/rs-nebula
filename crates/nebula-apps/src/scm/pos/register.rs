//! Registers (tills): where sales happen. A register sells from exactly
//! one warehouse — that binding is what lets session close consolidate
//! into a single issue movement without asking any questions. The
//! optional price list overrides the default resolution chain, the
//! optional default customer replaces the seeded walk-in, and
//! `grid_layout` is the client's tile arrangement, kept server-side so a
//! cashier finds the same till on any device.
//!
//! A register with history is deactivated, never deleted — sessions and
//! receipts reference it forever.

use crate::scm::pos::permissions::names;
use crate::scm::sales::customer::customer;
use crate::scm::sales::pricing::price_list;
use axum::extract::Path;
use axum::routing::{get, put};
use axum::{Json, Router};
use nebula::audit::Audit;
use nebula::auth::Authz;
use nebula::error::{Error, Result};
use nebula::{TenantDb, sea_orm};
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, QueryOrder, Set};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::scm::inventory::warehouse;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, utoipa::ToSchema)]
#[schema(as = PosRegister)]
#[sea_orm(table_name = "pos_registers")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    #[sea_orm(unique)]
    pub code: String,
    pub name: String,
    pub warehouse_id: Uuid,
    pub price_list_id: Option<Uuid>,
    /// `None` = the seeded walk-in customer.
    pub default_customer_id: Option<Uuid>,
    pub receipt_header: Option<String>,
    pub receipt_footer: Option<String>,
    /// Offline-overshoot policy at close: whether a session may close
    /// when its consolidated issue would take stock negative.
    pub allow_negative_stock_sales: bool,
    /// The client-owned tile layout; the server stores, never interprets.
    #[sea_orm(column_type = "JsonBinary", nullable)]
    #[schema(value_type = Option<Object>)]
    pub grid_layout: Option<serde_json::Value>,
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

#[derive(Deserialize, utoipa::ToSchema)]
pub struct RegisterBody {
    pub code: String,
    pub name: String,
    pub warehouse_id: Uuid,
    pub price_list_id: Option<Uuid>,
    pub default_customer_id: Option<Uuid>,
    pub receipt_header: Option<String>,
    pub receipt_footer: Option<String>,
    #[serde(default)]
    pub allow_negative_stock_sales: bool,
    #[serde(default = "yes")]
    pub is_active: bool,
}

fn yes() -> bool {
    true
}

/// Data access for registers on a given (tenant) connection.
pub struct Store {
    db: DatabaseConnection,
}

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
            .ok_or_else(|| Error::NotFound(format!("register {id}")))
    }

    pub async fn create(&self, body: RegisterBody, created_by: Option<Uuid>) -> Result<Model> {
        let (code, name) = self.validate(&body, None).await?;
        let now = chrono::Utc::now();
        ActiveModel {
            id: Set(Uuid::new_v4()),
            code: Set(code),
            name: Set(name),
            warehouse_id: Set(body.warehouse_id),
            price_list_id: Set(body.price_list_id),
            default_customer_id: Set(body.default_customer_id),
            receipt_header: Set(body.receipt_header.filter(|v| !v.trim().is_empty())),
            receipt_footer: Set(body.receipt_footer.filter(|v| !v.trim().is_empty())),
            allow_negative_stock_sales: Set(body.allow_negative_stock_sales),
            grid_layout: Set(None),
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

    pub async fn update(
        &self,
        id: Uuid,
        body: RegisterBody,
        updated_by: Option<Uuid>,
    ) -> Result<Model> {
        let existing = self.find_by_id(id).await?;
        let (code, name) = self.validate(&body, Some(&existing)).await?;
        let mut active: ActiveModel = existing.into();
        active.code = Set(code);
        active.name = Set(name);
        active.warehouse_id = Set(body.warehouse_id);
        active.price_list_id = Set(body.price_list_id);
        active.default_customer_id = Set(body.default_customer_id);
        active.receipt_header = Set(body.receipt_header.filter(|v| !v.trim().is_empty()));
        active.receipt_footer = Set(body.receipt_footer.filter(|v| !v.trim().is_empty()));
        active.allow_negative_stock_sales = Set(body.allow_negative_stock_sales);
        active.is_active = Set(body.is_active);
        active.updated_at = Set(chrono::Utc::now());
        active.updated_by = Set(updated_by);
        active.update(&self.db).await.map_err(Error::from)
    }

    /// Deactivate (never delete): sessions and receipts reference the
    /// register forever. Refused while a session is open on it.
    pub async fn deactivate(&self, id: Uuid, updated_by: Option<Uuid>) -> Result<Model> {
        let existing = self.find_by_id(id).await?;
        let open = super::session::session::Entity::find()
            .filter(super::session::session::Column::RegisterId.eq(id))
            .filter(
                super::session::session::Column::Status
                    .is_in(["open", "closing"]),
            )
            .count(&self.db)
            .await?;
        if open > 0 {
            return Err(Error::Validation(
                "close the register's open session before deactivating it".into(),
            ));
        }
        let mut active: ActiveModel = existing.into();
        active.is_active = Set(false);
        active.updated_at = Set(chrono::Utc::now());
        active.updated_by = Set(updated_by);
        active.update(&self.db).await.map_err(Error::from)
    }

    /// Store the client's tile layout as-is.
    pub async fn set_grid(&self, id: Uuid, layout: serde_json::Value) -> Result<Model> {
        let existing = self.find_by_id(id).await?;
        let mut active: ActiveModel = existing.into();
        active.grid_layout = Set(Some(layout));
        active.updated_at = Set(chrono::Utc::now());
        active.update(&self.db).await.map_err(Error::from)
    }

    async fn validate(
        &self,
        body: &RegisterBody,
        existing: Option<&Model>,
    ) -> Result<(String, String)> {
        let code = body.code.trim().to_string();
        if code.is_empty() {
            return Err(Error::Validation("register code must not be empty".into()));
        }
        let name = body.name.trim().to_string();
        if name.is_empty() {
            return Err(Error::Validation("register name must not be empty".into()));
        }
        let taken = Entity::find()
            .filter(Column::Code.eq(&code))
            .one(&self.db)
            .await?;
        if taken.is_some_and(|t| existing.is_none_or(|e| e.id != t.id)) {
            return Err(Error::Conflict(format!(
                "register code {code:?} already exists"
            )));
        }
        let wh = warehouse::Entity::find_by_id(body.warehouse_id)
            .one(&self.db)
            .await?
            .ok_or_else(|| Error::NotFound(format!("warehouse {}", body.warehouse_id)))?;
        if !wh.is_active {
            return Err(Error::Validation(format!(
                "warehouse {} is not active",
                wh.code
            )));
        }
        if let Some(pl_id) = body.price_list_id {
            price_list::Entity::find_by_id(pl_id)
                .one(&self.db)
                .await?
                .ok_or_else(|| Error::NotFound(format!("price list {pl_id}")))?;
        }
        if let Some(cust_id) = body.default_customer_id {
            customer::Entity::find_by_id(cust_id)
                .one(&self.db)
                .await?
                .ok_or_else(|| Error::NotFound(format!("customer {cust_id}")))?;
        }
        Ok((code, name))
    }
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

pub(crate) fn routes() -> Router {
    Router::new()
        .route("/pos/registers", get(list_registers).post(create_register))
        .route(
            "/pos/registers/{id}",
            get(get_register)
                .put(update_register)
                .delete(deactivate_register),
        )
        .route("/pos/registers/{id}/grid", put(set_register_grid))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    list_registers,
    get_register,
    create_register,
    update_register,
    deactivate_register,
    set_register_grid
))]
struct ApiDoc;

#[utoipa::path(get, path = "/pos/registers", tag = "pos",
    responses((status = 200, body = Vec<Model>)))]
async fn list_registers(authz: Authz, TenantDb(db): TenantDb) -> Result<Json<Vec<Model>>> {
    authz.require(names::REGISTERS_VIEW).await?;
    Store::new(db).find_all().await.map(Json)
}

#[utoipa::path(get, path = "/pos/registers/{id}", tag = "pos",
    params(("id" = Uuid, Path, description = "Register id")),
    responses((status = 200, body = Model)))]
async fn get_register(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<Model>> {
    authz.require(names::REGISTERS_VIEW).await?;
    Store::new(db).find_by_id(id).await.map(Json)
}

#[utoipa::path(post, path = "/pos/registers", tag = "pos",
    request_body = RegisterBody,
    responses((status = 200, body = Model)))]
async fn create_register(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(body): Json<RegisterBody>,
) -> Result<Json<Model>> {
    authz.require(names::REGISTERS_MANAGE).await?;
    let row = Store::new(db).create(body, Some(authz.user.id)).await?;
    audit.0.created("pos.register", row.id, &row).await;
    Ok(Json(row))
}

#[utoipa::path(put, path = "/pos/registers/{id}", tag = "pos",
    params(("id" = Uuid, Path, description = "Register id")),
    request_body = RegisterBody,
    responses((status = 200, body = Model)))]
async fn update_register(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(body): Json<RegisterBody>,
) -> Result<Json<Model>> {
    authz.require(names::REGISTERS_MANAGE).await?;
    let store = Store::new(db);
    let before = store.find_by_id(id).await?;
    let after = store.update(id, body, Some(authz.user.id)).await?;
    audit.0.updated("pos.register", id, &before, &after).await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/pos/registers/{id}", tag = "pos",
    params(("id" = Uuid, Path, description = "Register id")),
    responses((status = 200, body = Model)))]
async fn deactivate_register(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<Model>> {
    authz.require(names::REGISTERS_MANAGE).await?;
    let row = Store::new(db).deactivate(id, Some(authz.user.id)).await?;
    audit.0.deleted("pos.register", row.id, &row).await;
    Ok(Json(row))
}

/// The tile layout is the cashier's own arrangement — saving it needs
/// only the sell permission, not register management.
#[utoipa::path(put, path = "/pos/registers/{id}/grid", tag = "pos",
    params(("id" = Uuid, Path, description = "Register id")),
    request_body = serde_json::Value,
    responses((status = 200, body = Model)))]
async fn set_register_grid(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(layout): Json<serde_json::Value>,
) -> Result<Json<Model>> {
    authz.require(names::SELL).await?;
    Store::new(db).set_grid(id, layout).await.map(Json)
}
