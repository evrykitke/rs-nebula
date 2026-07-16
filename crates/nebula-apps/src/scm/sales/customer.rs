//! Customers and customer groups: who we sell to, on what terms.
//!
//! `on_hold` is softer than deactivation: it blocks *new* sales documents
//! while documents already in flight finish their lifecycle. Deleting a
//! customer referenced by any document deactivates instead — the paper
//! trail keeps its name. `credit_limit`: NULL = unlimited, 0 = cash only,
//! > 0 = the ceiling checked when commitments are made (order confirm,
//! invoice post). Groups exist for pricing tiers (trade/retail/wholesale)
//! and reporting rollups; a customer's own price list beats its group's.

use crate::scm::sales::permissions::names;
use crate::scm::sales::pricing;
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

/// A pricing/reporting tier of customers.
pub mod group {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, utoipa::ToSchema)]
    #[schema(as = SalesCustomerGroup)]
    #[sea_orm(table_name = "sales_customer_groups")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        #[sea_orm(unique)]
        pub name: String,
        pub description: Option<String>,
        pub price_list_id: Option<Uuid>,
        #[sea_orm(column_type = "Decimal(Some((7, 4)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub default_discount_pct: Option<Decimal>,
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

/// The customer master.
pub mod customer {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, utoipa::ToSchema)]
    #[schema(as = SalesCustomer)]
    #[sea_orm(table_name = "sales_customers")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        #[sea_orm(unique)]
        pub code: String,
        pub name: String,
        pub legal_name: Option<String>,
        /// company|individual.
        pub customer_type: String,
        pub registration_no: Option<String>,
        pub tax_number: Option<String>,
        pub industry: Option<String>,
        pub website: Option<String>,
        pub group_id: Option<Uuid>,
        pub contact_name: Option<String>,
        pub email: Option<String>,
        pub phone: Option<String>,
        pub secondary_contact_name: Option<String>,
        pub secondary_email: Option<String>,
        pub secondary_phone: Option<String>,
        pub billing_address_line1: Option<String>,
        pub billing_address_line2: Option<String>,
        pub billing_city: Option<String>,
        pub billing_region: Option<String>,
        pub billing_postal_code: Option<String>,
        pub billing_country: Option<String>,
        pub shipping_address_line1: Option<String>,
        pub shipping_address_line2: Option<String>,
        pub shipping_city: Option<String>,
        pub shipping_region: Option<String>,
        pub shipping_postal_code: Option<String>,
        pub shipping_country: Option<String>,
        /// ISO 4217; sales documents default to this.
        pub currency: String,
        pub payment_terms_days: i32,
        /// NULL = unlimited, 0 = cash only, > 0 = the ceiling.
        #[sea_orm(column_type = "Decimal(Some((20, 4)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub credit_limit: Option<Decimal>,
        pub price_list_id: Option<Uuid>,
        #[sea_orm(column_type = "Decimal(Some((7, 4)))", nullable)]
        #[serde(with = "rust_decimal::serde::str_option")]
        #[schema(value_type = Option<String>)]
        pub default_discount_pct: Option<Decimal>,
        pub default_tax_code_id: Option<Uuid>,
        pub tax_exempt: bool,
        pub tax_exemption_no: Option<String>,
        pub default_warehouse_id: Option<Uuid>,
        pub salesperson_id: Option<Uuid>,
        pub incoterms: Option<String>,
        pub loyalty_no: Option<String>,
        /// Blocks new sales documents; in-flight documents finish.
        pub on_hold: bool,
        pub hold_reason: Option<String>,
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

const CUSTOMER_TYPES: &[&str] = &["company", "individual"];

// ---------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
pub struct GroupBody {
    pub name: String,
    pub description: Option<String>,
    pub price_list_id: Option<Uuid>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub default_discount_pct: Option<Decimal>,
    #[serde(default = "yes")]
    pub is_active: bool,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CustomerBody {
    pub code: String,
    pub name: String,
    pub legal_name: Option<String>,
    #[serde(default = "default_customer_type")]
    pub customer_type: String,
    pub registration_no: Option<String>,
    pub tax_number: Option<String>,
    pub industry: Option<String>,
    pub website: Option<String>,
    pub group_id: Option<Uuid>,
    pub contact_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub secondary_contact_name: Option<String>,
    pub secondary_email: Option<String>,
    pub secondary_phone: Option<String>,
    pub billing_address_line1: Option<String>,
    pub billing_address_line2: Option<String>,
    pub billing_city: Option<String>,
    pub billing_region: Option<String>,
    pub billing_postal_code: Option<String>,
    pub billing_country: Option<String>,
    pub shipping_address_line1: Option<String>,
    pub shipping_address_line2: Option<String>,
    pub shipping_city: Option<String>,
    pub shipping_region: Option<String>,
    pub shipping_postal_code: Option<String>,
    pub shipping_country: Option<String>,
    pub currency: String,
    #[serde(default)]
    pub payment_terms_days: i32,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub credit_limit: Option<Decimal>,
    pub price_list_id: Option<Uuid>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub default_discount_pct: Option<Decimal>,
    pub default_tax_code_id: Option<Uuid>,
    #[serde(default)]
    pub tax_exempt: bool,
    pub tax_exemption_no: Option<String>,
    pub default_warehouse_id: Option<Uuid>,
    pub salesperson_id: Option<Uuid>,
    pub incoterms: Option<String>,
    pub loyalty_no: Option<String>,
    #[serde(default)]
    pub on_hold: bool,
    pub hold_reason: Option<String>,
    #[serde(default = "yes")]
    pub is_active: bool,
    pub notes: Option<String>,
}

fn yes() -> bool {
    true
}

fn default_customer_type() -> String {
    "company".into()
}

fn clean(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// Data access for customers and groups on a given (tenant) connection.
pub struct Store {
    db: DatabaseConnection,
}

impl Store {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    // -- groups -------------------------------------------------------------

    pub async fn find_all_groups(&self) -> Result<Vec<group::Model>> {
        group::Entity::find()
            .order_by_asc(group::Column::Name)
            .all(&self.db)
            .await
            .map_err(Error::from)
    }

    pub async fn find_group(&self, id: Uuid) -> Result<group::Model> {
        group::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or_else(|| Error::NotFound(format!("customer group {id}")))
    }

    pub async fn create_group(
        &self,
        body: GroupBody,
        created_by: Option<Uuid>,
    ) -> Result<group::Model> {
        let name = self.validate_group(&body, None).await?;
        let now = chrono::Utc::now();
        group::ActiveModel {
            id: Set(Uuid::new_v4()),
            name: Set(name),
            description: Set(clean(body.description)),
            price_list_id: Set(body.price_list_id),
            default_discount_pct: Set(body.default_discount_pct),
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

    pub async fn update_group(
        &self,
        id: Uuid,
        body: GroupBody,
        updated_by: Option<Uuid>,
    ) -> Result<group::Model> {
        let existing = self.find_group(id).await?;
        let name = self.validate_group(&body, Some(&existing)).await?;
        let mut active: group::ActiveModel = existing.into();
        active.name = Set(name);
        active.description = Set(clean(body.description));
        active.price_list_id = Set(body.price_list_id);
        active.default_discount_pct = Set(body.default_discount_pct);
        active.is_active = Set(body.is_active);
        active.updated_at = Set(chrono::Utc::now());
        active.updated_by = Set(updated_by);
        active.update(&self.db).await.map_err(Error::from)
    }

    /// Delete a group — or deactivate it once customers belong to it.
    pub async fn delete_group(&self, id: Uuid) -> Result<group::Model> {
        let existing = self.find_group(id).await?;
        let members = customer::Entity::find()
            .filter(customer::Column::GroupId.eq(id))
            .count(&self.db)
            .await?;
        if members > 0 {
            let mut active: group::ActiveModel = existing.into();
            active.is_active = Set(false);
            active.updated_at = Set(chrono::Utc::now());
            return active.update(&self.db).await.map_err(Error::from);
        }
        group::Entity::delete_by_id(id).exec(&self.db).await?;
        Ok(existing)
    }

    async fn validate_group(
        &self,
        body: &GroupBody,
        existing: Option<&group::Model>,
    ) -> Result<String> {
        let name = body.name.trim().to_string();
        if name.is_empty() {
            return Err(Error::Validation("group name must not be empty".into()));
        }
        if let Some(pct) = body.default_discount_pct {
            if pct < Decimal::ZERO || pct > Decimal::ONE_HUNDRED {
                return Err(Error::Validation(
                    "default discount must be between 0 and 100 percent".into(),
                ));
            }
        }
        if let Some(list_id) = body.price_list_id {
            self.require_group_list(list_id).await?;
        }
        let taken = group::Entity::find()
            .filter(group::Column::Name.eq(&name))
            .one(&self.db)
            .await?;
        if taken.is_some_and(|t| existing.is_none_or(|e| e.id != t.id)) {
            return Err(Error::Conflict(format!(
                "customer group {name:?} already exists"
            )));
        }
        Ok(name)
    }

    /// A group's price list must exist and be scoped for groups.
    async fn require_group_list(&self, list_id: Uuid) -> Result<()> {
        let list = pricing::price_list::Entity::find_by_id(list_id)
            .one(&self.db)
            .await?
            .ok_or_else(|| Error::NotFound(format!("price list {list_id}")))?;
        if list.scope != pricing::ListScope::Group.as_str() {
            return Err(Error::Validation(format!(
                "price list {:?} is not a group-scoped list",
                list.name
            )));
        }
        Ok(())
    }

    // -- customers ----------------------------------------------------------

    pub async fn find_all(&self) -> Result<Vec<customer::Model>> {
        customer::Entity::find()
            .order_by_asc(customer::Column::Code)
            .all(&self.db)
            .await
            .map_err(Error::from)
    }

    pub async fn find_by_id(&self, id: Uuid) -> Result<customer::Model> {
        customer::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or_else(|| Error::NotFound(format!("customer {id}")))
    }

    pub async fn create(
        &self,
        body: CustomerBody,
        created_by: Option<Uuid>,
    ) -> Result<customer::Model> {
        let (code, name, currency) = self.validate(&body, None).await?;
        let now = chrono::Utc::now();
        customer::ActiveModel {
            id: Set(Uuid::new_v4()),
            code: Set(code),
            name: Set(name),
            legal_name: Set(clean(body.legal_name)),
            customer_type: Set(body.customer_type),
            registration_no: Set(clean(body.registration_no)),
            tax_number: Set(clean(body.tax_number)),
            industry: Set(clean(body.industry)),
            website: Set(clean(body.website)),
            group_id: Set(body.group_id),
            contact_name: Set(clean(body.contact_name)),
            email: Set(clean(body.email)),
            phone: Set(clean(body.phone)),
            secondary_contact_name: Set(clean(body.secondary_contact_name)),
            secondary_email: Set(clean(body.secondary_email)),
            secondary_phone: Set(clean(body.secondary_phone)),
            billing_address_line1: Set(clean(body.billing_address_line1)),
            billing_address_line2: Set(clean(body.billing_address_line2)),
            billing_city: Set(clean(body.billing_city)),
            billing_region: Set(clean(body.billing_region)),
            billing_postal_code: Set(clean(body.billing_postal_code)),
            billing_country: Set(clean(body.billing_country)),
            shipping_address_line1: Set(clean(body.shipping_address_line1)),
            shipping_address_line2: Set(clean(body.shipping_address_line2)),
            shipping_city: Set(clean(body.shipping_city)),
            shipping_region: Set(clean(body.shipping_region)),
            shipping_postal_code: Set(clean(body.shipping_postal_code)),
            shipping_country: Set(clean(body.shipping_country)),
            currency: Set(currency),
            payment_terms_days: Set(body.payment_terms_days),
            credit_limit: Set(body.credit_limit),
            price_list_id: Set(body.price_list_id),
            default_discount_pct: Set(body.default_discount_pct),
            default_tax_code_id: Set(body.default_tax_code_id),
            tax_exempt: Set(body.tax_exempt),
            tax_exemption_no: Set(clean(body.tax_exemption_no)),
            default_warehouse_id: Set(body.default_warehouse_id),
            salesperson_id: Set(body.salesperson_id),
            incoterms: Set(clean(body.incoterms)),
            loyalty_no: Set(clean(body.loyalty_no)),
            on_hold: Set(body.on_hold),
            hold_reason: Set(clean(body.hold_reason)),
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
        body: CustomerBody,
        updated_by: Option<Uuid>,
    ) -> Result<customer::Model> {
        let existing = self.find_by_id(id).await?;
        let (code, name, currency) = self.validate(&body, Some(&existing)).await?;
        let mut active: customer::ActiveModel = existing.into();
        active.code = Set(code);
        active.name = Set(name);
        active.legal_name = Set(clean(body.legal_name));
        active.customer_type = Set(body.customer_type);
        active.registration_no = Set(clean(body.registration_no));
        active.tax_number = Set(clean(body.tax_number));
        active.industry = Set(clean(body.industry));
        active.website = Set(clean(body.website));
        active.group_id = Set(body.group_id);
        active.contact_name = Set(clean(body.contact_name));
        active.email = Set(clean(body.email));
        active.phone = Set(clean(body.phone));
        active.secondary_contact_name = Set(clean(body.secondary_contact_name));
        active.secondary_email = Set(clean(body.secondary_email));
        active.secondary_phone = Set(clean(body.secondary_phone));
        active.billing_address_line1 = Set(clean(body.billing_address_line1));
        active.billing_address_line2 = Set(clean(body.billing_address_line2));
        active.billing_city = Set(clean(body.billing_city));
        active.billing_region = Set(clean(body.billing_region));
        active.billing_postal_code = Set(clean(body.billing_postal_code));
        active.billing_country = Set(clean(body.billing_country));
        active.shipping_address_line1 = Set(clean(body.shipping_address_line1));
        active.shipping_address_line2 = Set(clean(body.shipping_address_line2));
        active.shipping_city = Set(clean(body.shipping_city));
        active.shipping_region = Set(clean(body.shipping_region));
        active.shipping_postal_code = Set(clean(body.shipping_postal_code));
        active.shipping_country = Set(clean(body.shipping_country));
        active.currency = Set(currency);
        active.payment_terms_days = Set(body.payment_terms_days);
        active.credit_limit = Set(body.credit_limit);
        active.price_list_id = Set(body.price_list_id);
        active.default_discount_pct = Set(body.default_discount_pct);
        active.default_tax_code_id = Set(body.default_tax_code_id);
        active.tax_exempt = Set(body.tax_exempt);
        active.tax_exemption_no = Set(clean(body.tax_exemption_no));
        active.default_warehouse_id = Set(body.default_warehouse_id);
        active.salesperson_id = Set(body.salesperson_id);
        active.incoterms = Set(clean(body.incoterms));
        active.loyalty_no = Set(clean(body.loyalty_no));
        active.on_hold = Set(body.on_hold);
        active.hold_reason = Set(clean(body.hold_reason));
        active.is_active = Set(body.is_active);
        active.notes = Set(clean(body.notes));
        active.updated_at = Set(chrono::Utc::now());
        active.updated_by = Set(updated_by);
        active.update(&self.db).await.map_err(Error::from)
    }

    /// Delete a customer. Once sales documents carry its name this will
    /// deactivate instead (the supplier precedent) — the reference checks
    /// arrive with each document type, starting with quotations and
    /// orders in the next phase.
    pub async fn delete(&self, id: Uuid) -> Result<customer::Model> {
        let existing = self.find_by_id(id).await?;
        customer::Entity::delete_by_id(id).exec(&self.db).await?;
        Ok(existing)
    }

    async fn validate(
        &self,
        body: &CustomerBody,
        existing: Option<&customer::Model>,
    ) -> Result<(String, String, String)> {
        let code = body.code.trim().to_string();
        if code.is_empty() {
            return Err(Error::Validation("customer code must not be empty".into()));
        }
        let name = body.name.trim().to_string();
        if name.is_empty() {
            return Err(Error::Validation("customer name must not be empty".into()));
        }
        if !CUSTOMER_TYPES.contains(&body.customer_type.as_str()) {
            return Err(Error::Validation(format!(
                "unknown customer type {:?} (expected company or individual)",
                body.customer_type
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
        if let Some(limit) = body.credit_limit {
            if limit < Decimal::ZERO {
                return Err(Error::Validation(
                    "credit limit must not be negative".into(),
                ));
            }
        }
        if let Some(pct) = body.default_discount_pct {
            if pct < Decimal::ZERO || pct > Decimal::ONE_HUNDRED {
                return Err(Error::Validation(
                    "default discount must be between 0 and 100 percent".into(),
                ));
            }
        }
        if let Some(group_id) = body.group_id {
            self.find_group(group_id).await?;
        }
        if let Some(list_id) = body.price_list_id {
            let list = pricing::price_list::Entity::find_by_id(list_id)
                .one(&self.db)
                .await?
                .ok_or_else(|| Error::NotFound(format!("price list {list_id}")))?;
            if list.scope != pricing::ListScope::Customer.as_str() {
                return Err(Error::Validation(format!(
                    "price list {:?} is not a customer-scoped list",
                    list.name
                )));
            }
        }
        let taken = customer::Entity::find()
            .filter(customer::Column::Code.eq(&code))
            .one(&self.db)
            .await?;
        if taken.is_some_and(|t| existing.is_none_or(|e| e.id != t.id)) {
            return Err(Error::Conflict(format!(
                "customer code {code:?} already exists"
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
            "/sales/customers",
            get(list_customers).post(create_customer),
        )
        .route(
            "/sales/customers/{id}",
            get(get_customer)
                .put(update_customer)
                .delete(delete_customer),
        )
        .route(
            "/sales/customer-groups",
            get(list_groups).post(create_group),
        )
        .route(
            "/sales/customer-groups/{id}",
            axum::routing::put(update_group).delete(delete_group),
        )
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    list_customers,
    get_customer,
    create_customer,
    update_customer,
    delete_customer,
    list_groups,
    create_group,
    update_group,
    delete_group
))]
struct ApiDoc;

#[utoipa::path(get, path = "/sales/customers", tag = "sales",
    responses((status = 200, body = Vec<customer::Model>)))]
async fn list_customers(
    authz: Authz,
    TenantDb(db): TenantDb,
) -> Result<Json<Vec<customer::Model>>> {
    authz.require(names::CUSTOMERS_VIEW).await?;
    Store::new(db).find_all().await.map(Json)
}

#[utoipa::path(get, path = "/sales/customers/{id}", tag = "sales",
    params(("id" = Uuid, Path, description = "Customer id")),
    responses((status = 200, body = customer::Model)))]
async fn get_customer(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<customer::Model>> {
    authz.require(names::CUSTOMERS_VIEW).await?;
    Store::new(db).find_by_id(id).await.map(Json)
}

#[utoipa::path(post, path = "/sales/customers", tag = "sales",
    request_body = CustomerBody,
    responses((status = 200, body = customer::Model)))]
async fn create_customer(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(body): Json<CustomerBody>,
) -> Result<Json<customer::Model>> {
    authz.require(names::CUSTOMERS_CREATE).await?;
    let row = Store::new(db).create(body, Some(authz.user.id)).await?;
    audit.0.created("scm.customer", row.id, &row).await;
    Ok(Json(row))
}

#[utoipa::path(put, path = "/sales/customers/{id}", tag = "sales",
    params(("id" = Uuid, Path, description = "Customer id")),
    request_body = CustomerBody,
    responses((status = 200, body = customer::Model)))]
async fn update_customer(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(body): Json<CustomerBody>,
) -> Result<Json<customer::Model>> {
    authz.require(names::CUSTOMERS_EDIT).await?;
    let store = Store::new(db);
    let before = store.find_by_id(id).await?;
    let after = store.update(id, body, Some(authz.user.id)).await?;
    audit.0.updated("scm.customer", id, &before, &after).await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/sales/customers/{id}", tag = "sales",
    params(("id" = Uuid, Path, description = "Customer id")),
    responses((status = 200, body = customer::Model)))]
async fn delete_customer(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<customer::Model>> {
    authz.require(names::CUSTOMERS_DELETE).await?;
    let row = Store::new(db).delete(id).await?;
    audit.0.deleted("scm.customer", id, &row).await;
    Ok(Json(row))
}

#[utoipa::path(get, path = "/sales/customer-groups", tag = "sales",
    responses((status = 200, body = Vec<group::Model>)))]
async fn list_groups(authz: Authz, TenantDb(db): TenantDb) -> Result<Json<Vec<group::Model>>> {
    authz.require(names::CUSTOMERS_VIEW).await?;
    Store::new(db).find_all_groups().await.map(Json)
}

#[utoipa::path(post, path = "/sales/customer-groups", tag = "sales",
    request_body = GroupBody,
    responses((status = 200, body = group::Model)))]
async fn create_group(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(body): Json<GroupBody>,
) -> Result<Json<group::Model>> {
    authz.require(names::CUSTOMERS_CREATE).await?;
    let row = Store::new(db)
        .create_group(body, Some(authz.user.id))
        .await?;
    audit.0.created("scm.customer_group", row.id, &row).await;
    Ok(Json(row))
}

#[utoipa::path(put, path = "/sales/customer-groups/{id}", tag = "sales",
    params(("id" = Uuid, Path, description = "Group id")),
    request_body = GroupBody,
    responses((status = 200, body = group::Model)))]
async fn update_group(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(body): Json<GroupBody>,
) -> Result<Json<group::Model>> {
    authz.require(names::CUSTOMERS_EDIT).await?;
    let store = Store::new(db);
    let before = store.find_group(id).await?;
    let after = store.update_group(id, body, Some(authz.user.id)).await?;
    audit
        .0
        .updated("scm.customer_group", id, &before, &after)
        .await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/sales/customer-groups/{id}", tag = "sales",
    params(("id" = Uuid, Path, description = "Group id")),
    responses((status = 200, body = group::Model)))]
async fn delete_group(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<group::Model>> {
    authz.require(names::CUSTOMERS_DELETE).await?;
    let row = Store::new(db).delete_group(id).await?;
    audit.0.deleted("scm.customer_group", id, &row).await;
    Ok(Json(row))
}
