//! The chart of accounts: the financial buckets a tenant books against.
//!
//! Rows live in the tenant's own database (the request-scoped connection
//! from [`nebula::TenantDb`]), created by the `migrations/accounting`
//! SQL. Each account carries a type that fixes its normal balance side
//! and a currency it is denominated in.

use crate::accounting::permissions::names;
use axum::extract::Path;
use axum::routing::get;
use axum::{Json, Router};
use nebula::audit::Audit;
use nebula::auth::Authz;
use nebula::error::{Error, Result};
use nebula::{TenantDb, sea_orm};
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, QueryOrder, Set};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable identifiers for the accounts the platform seeds and other
/// modules resolve by role — so a POS or sales module can post to "the
/// receivables account" without any tenant configuration. A tenant may
/// rename or deactivate these accounts; the key is what stays constant.
pub mod keys {
    pub const CASH: &str = "cash";
    pub const BANK: &str = "bank";
    pub const AR: &str = "ar";
    pub const INVENTORY: &str = "inventory";
    /// Recoverable VAT paid on purchases (an asset).
    pub const VAT_INPUT: &str = "vat_input";
    pub const AP: &str = "ap";
    /// VAT collected on sales, owed to the authority (a liability).
    pub const VAT_OUTPUT: &str = "vat_output";
    pub const TAX_PAYABLE: &str = "tax_payable";
    pub const OWNER_EQUITY: &str = "owner_equity";
    pub const RETAINED_EARNINGS: &str = "retained_earnings";
    pub const SALES: &str = "sales";
    pub const OTHER_INCOME: &str = "other_income";
    pub const COGS: &str = "cogs";
    pub const OPEX: &str = "opex";
    /// Absorbs sub-unit rounding differences on tax and allocation.
    pub const ROUNDING: &str = "rounding";
}

/// The five account classes of double-entry bookkeeping. Each one fixes
/// whether the account increases on the debit or the credit side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccountType {
    Asset,
    Liability,
    Equity,
    Revenue,
    Expense,
}

/// The side on which an account's balance naturally grows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NormalBalance {
    Debit,
    Credit,
}

impl AccountType {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountType::Asset => "asset",
            AccountType::Liability => "liability",
            AccountType::Equity => "equity",
            AccountType::Revenue => "revenue",
            AccountType::Expense => "expense",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "asset" => Ok(AccountType::Asset),
            "liability" => Ok(AccountType::Liability),
            "equity" => Ok(AccountType::Equity),
            "revenue" => Ok(AccountType::Revenue),
            "expense" => Ok(AccountType::Expense),
            other => Err(Error::Validation(format!(
                "unknown account type {other:?} (expected asset, liability, equity, revenue or expense)"
            ))),
        }
    }

    /// Assets and expenses grow on the debit side; the rest on the credit
    /// side. This is what turns a signed posting sum into a balance.
    pub fn normal_balance(self) -> NormalBalance {
        match self {
            AccountType::Asset | AccountType::Expense => NormalBalance::Debit,
            AccountType::Liability | AccountType::Equity | AccountType::Revenue => {
                NormalBalance::Credit
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, utoipa::ToSchema)]
#[schema(as = AccountingAccount)]
#[sea_orm(table_name = "accounting_accounts")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    /// Human-facing identifier, unique per tenant (e.g. `1000`).
    #[sea_orm(unique)]
    pub code: String,
    pub name: String,
    /// One of asset|liability|equity|revenue|expense.
    pub account_type: String,
    /// ISO 4217 code the account is denominated in.
    pub currency: String,
    pub parent_id: Option<Uuid>,
    pub description: Option<String>,
    /// Stable platform role (e.g. `ar`, `vat_output`, `sales`) other
    /// modules resolve this account by; `None` for user-created accounts.
    pub system_key: Option<String>,
    /// Seeded by the platform's default chart of accounts; cannot be
    /// deleted (but may be renamed or deactivated).
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

/// Data access for the chart of accounts on a given (tenant) connection.
pub struct Store {
    db: DatabaseConnection,
}

pub struct NewAccount {
    pub code: String,
    pub name: String,
    pub account_type: AccountType,
    pub currency: String,
    pub parent_id: Option<Uuid>,
    pub description: Option<String>,
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
            .ok_or_else(|| Error::NotFound(format!("account {id}")))
    }

    async fn find_by_code(&self, code: &str) -> Result<Option<Model>> {
        Entity::find()
            .filter(Column::Code.eq(code))
            .one(&self.db)
            .await
            .map_err(Error::from)
    }

    pub async fn create(&self, new: NewAccount) -> Result<Model> {
        let code = new.code.trim().to_string();
        if code.is_empty() {
            return Err(Error::Validation("account code must not be empty".into()));
        }
        if new.name.trim().is_empty() {
            return Err(Error::Validation("account name must not be empty".into()));
        }
        // Same code-shape rules as Money so the account is usable.
        nebula::Currency::new(&new.currency, 2)?;
        if self.find_by_code(&code).await?.is_some() {
            return Err(Error::Conflict(format!(
                "account code {code:?} already exists"
            )));
        }
        if let Some(parent_id) = new.parent_id {
            let parent = self.find_by_id(parent_id).await?;
            if parent.currency != new.currency {
                return Err(Error::Validation(
                    "a sub-account must share its parent's currency".into(),
                ));
            }
        }
        let now = chrono::Utc::now();
        ActiveModel {
            id: Set(Uuid::new_v4()),
            code: Set(code),
            name: Set(new.name.trim().to_string()),
            account_type: Set(new.account_type.as_str().to_string()),
            currency: Set(new.currency),
            parent_id: Set(new.parent_id),
            description: Set(new.description.filter(|d| !d.trim().is_empty())),
            system_key: Set(None),
            is_system: Set(false),
            is_active: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&self.db)
        .await
        .map_err(Error::from)
    }

    /// Resolve the account fulfilling a platform role (e.g. `ar`,
    /// `vat_output`) on this connection, if the default chart has seeded
    /// it. This is how other modules post to "the receivables account"
    /// without any tenant configuration.
    pub async fn find_by_system_key(&self, key: &str) -> Result<Option<Model>> {
        Entity::find()
            .filter(Column::SystemKey.eq(key))
            .one(&self.db)
            .await
            .map_err(Error::from)
    }

    /// Update the mutable fields of an account. The code, type and currency
    /// are identity/ledger-defining and are not changed here.
    pub async fn update(
        &self,
        id: Uuid,
        name: Option<String>,
        description: Option<String>,
        is_active: Option<bool>,
    ) -> Result<Model> {
        let existing = self.find_by_id(id).await?;
        let mut active: ActiveModel = existing.into();
        if let Some(name) = name {
            if name.trim().is_empty() {
                return Err(Error::Validation("account name must not be empty".into()));
            }
            active.name = Set(name.trim().to_string());
        }
        if let Some(description) = description {
            active.description = Set(Some(description).filter(|d| !d.trim().is_empty()));
        }
        if let Some(is_active) = is_active {
            active.is_active = Set(is_active);
        }
        active.updated_at = Set(chrono::Utc::now());
        active.update(&self.db).await.map_err(Error::from)
    }

    /// Delete an account. Refused once it carries postings or has
    /// sub-accounts — the ledger must stay referentially whole, so a
    /// once-used account is deactivated, never removed.
    pub async fn delete(&self, id: Uuid) -> Result<Model> {
        let account = self.find_by_id(id).await?;
        if account.is_system {
            return Err(Error::Validation(
                "this is a system account and cannot be deleted; deactivate it instead".into(),
            ));
        }
        let postings = super::journal::posting::Entity::find()
            .filter(super::journal::posting::Column::AccountId.eq(id))
            .count(&self.db)
            .await?;
        if postings > 0 {
            return Err(Error::Validation(
                "account has postings and cannot be deleted; deactivate it instead".into(),
            ));
        }
        let children = Entity::find()
            .filter(Column::ParentId.eq(id))
            .count(&self.db)
            .await?;
        if children > 0 {
            return Err(Error::Validation(
                "account has sub-accounts and cannot be deleted".into(),
            ));
        }
        Entity::delete_by_id(id).exec(&self.db).await?;
        Ok(account)
    }
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

pub(super) fn routes() -> Router {
    Router::new()
        .route(
            "/accounting/accounts",
            get(list_accounts).post(create_account),
        )
        .route(
            "/accounting/accounts/{id}",
            get(get_account).put(update_account).delete(delete_account),
        )
}

pub(super) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    list_accounts,
    get_account,
    create_account,
    update_account,
    delete_account
))]
struct ApiDoc;

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateAccountRequest {
    pub code: String,
    pub name: String,
    pub account_type: AccountType,
    /// ISO 4217 code the account is denominated in.
    pub currency: String,
    pub parent_id: Option<Uuid>,
    pub description: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct UpdateAccountRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub is_active: Option<bool>,
}

#[utoipa::path(get, path = "/accounting/accounts", tag = "accounting",
    responses((status = 200, body = Vec<Model>)))]
async fn list_accounts(authz: Authz, TenantDb(db): TenantDb) -> Result<Json<Vec<Model>>> {
    authz.require(names::ACCOUNTS_VIEW).await?;
    Store::new(db).find_all().await.map(Json)
}

#[utoipa::path(get, path = "/accounting/accounts/{id}", tag = "accounting",
    params(("id" = Uuid, Path, description = "Account id")),
    responses((status = 200, body = Model)))]
async fn get_account(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<Model>> {
    authz.require(names::ACCOUNTS_VIEW).await?;
    Store::new(db).find_by_id(id).await.map(Json)
}

#[utoipa::path(post, path = "/accounting/accounts", tag = "accounting",
    request_body = CreateAccountRequest,
    responses((status = 200, body = Model)))]
async fn create_account(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(req): Json<CreateAccountRequest>,
) -> Result<Json<Model>> {
    authz.require(names::ACCOUNTS_CREATE).await?;
    let row = Store::new(db)
        .create(NewAccount {
            code: req.code,
            name: req.name,
            account_type: req.account_type,
            currency: req.currency,
            parent_id: req.parent_id,
            description: req.description,
        })
        .await?;
    audit.0.created("accounting.account", row.id, &row).await;
    Ok(Json(row))
}

#[utoipa::path(put, path = "/accounting/accounts/{id}", tag = "accounting",
    params(("id" = Uuid, Path, description = "Account id")),
    request_body = UpdateAccountRequest,
    responses((status = 200, body = Model)))]
async fn update_account(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateAccountRequest>,
) -> Result<Json<Model>> {
    authz.require(names::ACCOUNTS_EDIT).await?;
    let store = Store::new(db);
    let before = store.find_by_id(id).await?;
    let after = store
        .update(id, req.name, req.description, req.is_active)
        .await?;
    audit
        .0
        .updated("accounting.account", after.id, &before, &after)
        .await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/accounting/accounts/{id}", tag = "accounting",
    params(("id" = Uuid, Path, description = "Account id")),
    responses((status = 200, body = Model)))]
async fn delete_account(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<Model>> {
    authz.require(names::ACCOUNTS_DELETE).await?;
    let row = Store::new(db).delete(id).await?;
    audit.0.deleted("accounting.account", row.id, &row).await;
    Ok(Json(row))
}
