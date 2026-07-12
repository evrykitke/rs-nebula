//! The tax system: named tax codes (a rate plus the account the tax is
//! booked to) that source documents apply to a taxable base.
//!
//! A tax code is `output` (collected on sales, a liability) or `input`
//! (paid on purchases, recoverable as an asset). The rate is a percentage;
//! [`TaxDirection`] and the linked account tell a posting engine which
//! side the computed tax lands on. Seeded with editable defaults so a
//! business transacts out of the box, without denying full control to one
//! that wants it.

use crate::accounting::permissions::names;
use axum::extract::Path;
use axum::routing::get;
use axum::{Json, Router};
use nebula::audit::Audit;
use nebula::auth::Authz;
use nebula::error::{Error, Result};
use nebula::{TenantDb, sea_orm};
use rust_decimal::{Decimal, RoundingStrategy};
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, QueryOrder, Set};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Which side of the ledger a tax code lands on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaxDirection {
    /// Tax collected on sales — a liability owed to the authority.
    Output,
    /// Tax paid on purchases — recoverable, an asset.
    Input,
}

impl TaxDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            TaxDirection::Output => "output",
            TaxDirection::Input => "input",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "output" => Ok(TaxDirection::Output),
            "input" => Ok(TaxDirection::Input),
            other => Err(Error::Validation(format!(
                "unknown tax direction {other:?} (expected output or input)"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, utoipa::ToSchema)]
#[schema(as = AccountingTaxCode)]
#[sea_orm(table_name = "accounting_tax_codes")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    #[sea_orm(unique)]
    pub code: String,
    pub name: String,
    /// Percentage rate, e.g. `16` for 16%.
    #[sea_orm(column_type = "Decimal(Some((9, 4)))")]
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub rate: Decimal,
    pub account_id: Option<Uuid>,
    /// output|input.
    pub direction: String,
    pub is_system: bool,
    pub is_active: bool,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: DateTimeUtc,
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl Model {
    /// The tax due on a taxable base, rounded to two minor units with
    /// banker's rounding. `base` is the net amount the rate applies to.
    pub fn tax_on(&self, base: Decimal) -> Decimal {
        (base * self.rate / Decimal::ONE_HUNDRED)
            .round_dp_with_strategy(2, RoundingStrategy::MidpointNearestEven)
    }
}

/// Data access for tax codes on a given (tenant) connection.
pub struct Store {
    db: DatabaseConnection,
}

pub struct NewTaxCode {
    pub code: String,
    pub name: String,
    pub rate: Decimal,
    pub account_id: Option<Uuid>,
    pub direction: TaxDirection,
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
            .ok_or_else(|| Error::NotFound(format!("tax code {id}")))
    }

    async fn find_by_code(&self, code: &str) -> Result<Option<Model>> {
        Entity::find()
            .filter(Column::Code.eq(code))
            .one(&self.db)
            .await
            .map_err(Error::from)
    }

    pub async fn create(&self, new: NewTaxCode) -> Result<Model> {
        let code = new.code.trim().to_string();
        if code.is_empty() {
            return Err(Error::Validation("tax code must not be empty".into()));
        }
        if new.name.trim().is_empty() {
            return Err(Error::Validation("tax name must not be empty".into()));
        }
        if new.rate < Decimal::ZERO {
            return Err(Error::Validation("tax rate must not be negative".into()));
        }
        if self.find_by_code(&code).await?.is_some() {
            return Err(Error::Conflict(format!("tax code {code:?} already exists")));
        }
        let now = chrono::Utc::now();
        ActiveModel {
            id: Set(Uuid::new_v4()),
            code: Set(code),
            name: Set(new.name.trim().to_string()),
            rate: Set(new.rate),
            account_id: Set(new.account_id),
            direction: Set(new.direction.as_str().to_string()),
            is_system: Set(false),
            is_active: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&self.db)
        .await
        .map_err(Error::from)
    }

    pub async fn update(
        &self,
        id: Uuid,
        name: Option<String>,
        rate: Option<Decimal>,
        account_id: Option<Option<Uuid>>,
        is_active: Option<bool>,
    ) -> Result<Model> {
        let existing = self.find_by_id(id).await?;
        let mut active: ActiveModel = existing.into();
        if let Some(name) = name {
            if name.trim().is_empty() {
                return Err(Error::Validation("tax name must not be empty".into()));
            }
            active.name = Set(name.trim().to_string());
        }
        if let Some(rate) = rate {
            if rate < Decimal::ZERO {
                return Err(Error::Validation("tax rate must not be negative".into()));
            }
            active.rate = Set(rate);
        }
        if let Some(account_id) = account_id {
            active.account_id = Set(account_id);
        }
        if let Some(is_active) = is_active {
            active.is_active = Set(is_active);
        }
        active.updated_at = Set(chrono::Utc::now());
        active.update(&self.db).await.map_err(Error::from)
    }

    pub async fn delete(&self, id: Uuid) -> Result<Model> {
        let code = self.find_by_id(id).await?;
        if code.is_system {
            return Err(Error::Validation(
                "this is a system tax code and cannot be deleted; deactivate it instead".into(),
            ));
        }
        Entity::delete_by_id(id).exec(&self.db).await?;
        Ok(code)
    }
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

pub(super) fn routes() -> Router {
    Router::new()
        .route(
            "/accounting/tax-codes",
            get(list_tax_codes).post(create_tax_code),
        )
        .route(
            "/accounting/tax-codes/{id}",
            get(get_tax_code)
                .put(update_tax_code)
                .delete(delete_tax_code),
        )
}

pub(super) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    list_tax_codes,
    get_tax_code,
    create_tax_code,
    update_tax_code,
    delete_tax_code
))]
struct ApiDoc;

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateTaxCodeRequest {
    pub code: String,
    pub name: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub rate: Decimal,
    pub account_id: Option<Uuid>,
    #[serde(default = "default_direction")]
    pub direction: TaxDirection,
}

fn default_direction() -> TaxDirection {
    TaxDirection::Output
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct UpdateTaxCodeRequest {
    pub name: Option<String>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub rate: Option<Decimal>,
    /// Present to change the linked account (may be null to clear it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<Option<Uuid>>,
    pub is_active: Option<bool>,
}

#[utoipa::path(get, path = "/accounting/tax-codes", tag = "accounting",
    responses((status = 200, body = Vec<Model>)))]
async fn list_tax_codes(authz: Authz, TenantDb(db): TenantDb) -> Result<Json<Vec<Model>>> {
    authz.require(names::TAX_VIEW).await?;
    Store::new(db).find_all().await.map(Json)
}

#[utoipa::path(get, path = "/accounting/tax-codes/{id}", tag = "accounting",
    params(("id" = Uuid, Path, description = "Tax code id")),
    responses((status = 200, body = Model)))]
async fn get_tax_code(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<Model>> {
    authz.require(names::TAX_VIEW).await?;
    Store::new(db).find_by_id(id).await.map(Json)
}

#[utoipa::path(post, path = "/accounting/tax-codes", tag = "accounting",
    request_body = CreateTaxCodeRequest,
    responses((status = 200, body = Model)))]
async fn create_tax_code(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(req): Json<CreateTaxCodeRequest>,
) -> Result<Json<Model>> {
    authz.require(names::TAX_CREATE).await?;
    let row = Store::new(db)
        .create(NewTaxCode {
            code: req.code,
            name: req.name,
            rate: req.rate,
            account_id: req.account_id,
            direction: req.direction,
        })
        .await?;
    audit.0.created("accounting.tax_code", row.id, &row).await;
    Ok(Json(row))
}

#[utoipa::path(put, path = "/accounting/tax-codes/{id}", tag = "accounting",
    params(("id" = Uuid, Path, description = "Tax code id")),
    request_body = UpdateTaxCodeRequest,
    responses((status = 200, body = Model)))]
async fn update_tax_code(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateTaxCodeRequest>,
) -> Result<Json<Model>> {
    authz.require(names::TAX_EDIT).await?;
    let store = Store::new(db);
    let before = store.find_by_id(id).await?;
    let after = store
        .update(id, req.name, req.rate, req.account_id, req.is_active)
        .await?;
    audit
        .0
        .updated("accounting.tax_code", after.id, &before, &after)
        .await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/accounting/tax-codes/{id}", tag = "accounting",
    params(("id" = Uuid, Path, description = "Tax code id")),
    responses((status = 200, body = Model)))]
async fn delete_tax_code(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<Model>> {
    authz.require(names::TAX_DELETE).await?;
    let row = Store::new(db).delete(id).await?;
    audit.0.deleted("accounting.tax_code", row.id, &row).await;
    Ok(Json(row))
}
