//! Purchase requisitions: the internal "we need this" that precedes a
//! purchase order.
//!
//! Lifecycle: draft → submitted (numbered from the REQ series) → approved
//! → converted, with rejected and cancelled exits. A requisition names
//! the warehouse the goods are for and when they are needed — never a
//! supplier or a price; sourcing is the convert step's problem. Convert
//! takes a supplier and produces a *draft* purchase order (prices default
//! from the item-supplier catalog, then the item's purchase price, then
//! its last purchase price), so the buyer still reviews and submits the
//! order through the normal approval gate. No stock or GL effects at any
//! point — a requisition is a request, not a commitment.

use crate::scm::inventory::item::item;
use crate::scm::inventory::warehouse;
use crate::scm::procurement::order::{NewOrder, OrderLineInput, OrderService};
use crate::scm::procurement::permissions::names;
use crate::scm::procurement::supplier::item_supplier;
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
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, QueryOrder, QuerySelect, Set,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Where a purchase requisition is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequisitionStatus {
    Draft,
    Submitted,
    Approved,
    Rejected,
    Converted,
    Cancelled,
}

impl RequisitionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RequisitionStatus::Draft => "draft",
            RequisitionStatus::Submitted => "submitted",
            RequisitionStatus::Approved => "approved",
            RequisitionStatus::Rejected => "rejected",
            RequisitionStatus::Converted => "converted",
            RequisitionStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "draft" => Ok(RequisitionStatus::Draft),
            "submitted" => Ok(RequisitionStatus::Submitted),
            "approved" => Ok(RequisitionStatus::Approved),
            "rejected" => Ok(RequisitionStatus::Rejected),
            "converted" => Ok(RequisitionStatus::Converted),
            "cancelled" => Ok(RequisitionStatus::Cancelled),
            other => Err(Error::internal(format!(
                "unknown requisition status {other:?}"
            ))),
        }
    }
}

/// The requisition header.
pub mod requisition {
    use nebula::sea_orm;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "procurement_requisitions")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub number: Option<String>,
        pub warehouse_id: Uuid,
        pub needed_by: Option<Date>,
        pub memo: Option<String>,
        pub status: String,
        pub reject_reason: Option<String>,
        pub order_id: Option<Uuid>,
        pub submitted_at: Option<DateTimeUtc>,
        pub submitted_by: Option<Uuid>,
        pub approved_at: Option<DateTimeUtc>,
        pub approved_by: Option<Uuid>,
        pub created_at: DateTimeUtc,
        pub created_by: Option<Uuid>,
        pub updated_at: DateTimeUtc,
        pub updated_by: Option<Uuid>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// One requested item.
pub mod requisition_line {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "procurement_requisition_lines")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub requisition_id: Uuid,
        pub line_no: i32,
        pub item_id: Uuid,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub qty: Decimal,
        pub needed_by: Option<Date>,
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

/// A requisition line as supplied by a caller.
pub struct RequisitionLineInput {
    pub item_id: Uuid,
    pub qty: Decimal,
    pub needed_by: Option<chrono::NaiveDate>,
    pub memo: Option<String>,
}

/// A new draft requisition as supplied by a caller.
pub struct NewRequisition {
    pub warehouse_id: Uuid,
    pub needed_by: Option<chrono::NaiveDate>,
    pub memo: Option<String>,
    pub lines: Vec<RequisitionLineInput>,
    pub created_by: Option<Uuid>,
}

/// The requisition service over one (tenant) connection.
pub struct RequisitionService {
    db: DatabaseConnection,
}

impl RequisitionService {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Create a draft requisition.
    pub async fn create_draft(&self, new: NewRequisition) -> Result<RequisitionView> {
        validate_requisition(&self.db, &new).await?;
        let txn = self.db.begin().await?;
        let requisition_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        requisition::ActiveModel {
            id: Set(requisition_id),
            number: Set(None),
            warehouse_id: Set(new.warehouse_id),
            needed_by: Set(new.needed_by),
            memo: Set(clean(new.memo)),
            status: Set(RequisitionStatus::Draft.as_str().to_string()),
            reject_reason: Set(None),
            order_id: Set(None),
            submitted_at: Set(None),
            submitted_by: Set(None),
            approved_at: Set(None),
            approved_by: Set(None),
            created_at: Set(now),
            created_by: Set(new.created_by),
            updated_at: Set(now),
            updated_by: Set(None),
        }
        .insert(&txn)
        .await?;
        insert_lines(&txn, requisition_id, &new.lines).await?;
        txn.commit().await?;
        self.view(requisition_id).await
    }

    /// Replace a draft's header and lines wholesale.
    pub async fn update_draft(&self, id: Uuid, new: NewRequisition) -> Result<RequisitionView> {
        validate_requisition(&self.db, &new).await?;
        let txn = self.db.begin().await?;
        let existing = load_requisition_locked(&txn, id).await?;
        if RequisitionStatus::parse(&existing.status)? != RequisitionStatus::Draft {
            return Err(Error::Validation(
                "only a draft requisition can be edited".into(),
            ));
        }
        requisition_line::Entity::delete_many()
            .filter(requisition_line::Column::RequisitionId.eq(id))
            .exec(&txn)
            .await?;
        insert_lines(&txn, id, &new.lines).await?;
        let mut active: requisition::ActiveModel = existing.into();
        active.warehouse_id = Set(new.warehouse_id);
        active.needed_by = Set(new.needed_by);
        active.memo = Set(clean(new.memo));
        active.updated_at = Set(chrono::Utc::now());
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Delete a draft (lines cascade).
    pub async fn delete_draft(&self, id: Uuid) -> Result<RequisitionView> {
        let view = self.view(id).await?;
        let txn = self.db.begin().await?;
        let existing = load_requisition_locked(&txn, id).await?;
        if RequisitionStatus::parse(&existing.status)? != RequisitionStatus::Draft {
            return Err(Error::Validation(
                "only a draft requisition can be deleted".into(),
            ));
        }
        requisition::Entity::delete_by_id(id).exec(&txn).await?;
        txn.commit().await?;
        Ok(view)
    }

    /// Submit a draft for approval; the REQ number is allocated here.
    pub async fn submit(
        &self,
        id: Uuid,
        numbering: &Numbering,
        by: Option<Uuid>,
    ) -> Result<RequisitionView> {
        let txn = self.db.begin().await?;
        let existing = load_requisition_locked(&txn, id).await?;
        if RequisitionStatus::parse(&existing.status)? != RequisitionStatus::Draft {
            return Err(Error::Validation(
                "only a draft requisition can be submitted".into(),
            ));
        }
        let lines = load_lines(&txn, id).await?;
        if lines.is_empty() {
            return Err(Error::Validation(
                "a requisition needs at least one line".into(),
            ));
        }
        let number = numbering.next(&txn, crate::scm::REQUISITION_SERIES).await?;
        let now = chrono::Utc::now();
        let mut active: requisition::ActiveModel = existing.into();
        active.number = Set(Some(number.formatted));
        active.status = Set(RequisitionStatus::Submitted.as_str().to_string());
        active.submitted_at = Set(Some(now));
        active.submitted_by = Set(by);
        active.updated_at = Set(now);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Approve a submitted requisition.
    pub async fn approve(&self, id: Uuid, by: Option<Uuid>) -> Result<RequisitionView> {
        let txn = self.db.begin().await?;
        let existing = load_requisition_locked(&txn, id).await?;
        if RequisitionStatus::parse(&existing.status)? != RequisitionStatus::Submitted {
            return Err(Error::Validation(
                "only a submitted requisition can be approved".into(),
            ));
        }
        let now = chrono::Utc::now();
        let mut active: requisition::ActiveModel = existing.into();
        active.status = Set(RequisitionStatus::Approved.as_str().to_string());
        active.approved_at = Set(Some(now));
        active.approved_by = Set(by);
        active.updated_at = Set(now);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Reject a submitted requisition, keeping the reason.
    pub async fn reject(&self, id: Uuid, reason: &str, by: Option<Uuid>) -> Result<RequisitionView> {
        let txn = self.db.begin().await?;
        let existing = load_requisition_locked(&txn, id).await?;
        if RequisitionStatus::parse(&existing.status)? != RequisitionStatus::Submitted {
            return Err(Error::Validation(
                "only a submitted requisition can be rejected".into(),
            ));
        }
        let now = chrono::Utc::now();
        let mut active: requisition::ActiveModel = existing.into();
        active.status = Set(RequisitionStatus::Rejected.as_str().to_string());
        active.reject_reason = Set(Some(reason.trim().to_string()).filter(|r| !r.is_empty()));
        active.updated_at = Set(now);
        active.updated_by = Set(by);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Cancel a requisition that has not been converted.
    pub async fn cancel(&self, id: Uuid, by: Option<Uuid>) -> Result<RequisitionView> {
        let txn = self.db.begin().await?;
        let existing = load_requisition_locked(&txn, id).await?;
        let status = RequisitionStatus::parse(&existing.status)?;
        if !matches!(
            status,
            RequisitionStatus::Draft | RequisitionStatus::Submitted | RequisitionStatus::Approved
        ) {
            return Err(Error::Validation(format!(
                "a {} requisition cannot be cancelled",
                status.as_str()
            )));
        }
        let now = chrono::Utc::now();
        let mut active: requisition::ActiveModel = existing.into();
        active.status = Set(RequisitionStatus::Cancelled.as_str().to_string());
        active.updated_at = Set(now);
        active.updated_by = Set(by);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Convert an approved requisition into a draft purchase order for the
    /// given supplier. Line prices default from the item-supplier catalog,
    /// then the item's purchase price, then its last purchase price — the
    /// buyer reviews the draft before it is submitted.
    pub async fn convert(
        &self,
        id: Uuid,
        supplier_id: Uuid,
        by: Option<Uuid>,
    ) -> Result<RequisitionView> {
        let existing = load_requisition(&self.db, id).await?;
        if RequisitionStatus::parse(&existing.status)? != RequisitionStatus::Approved {
            return Err(Error::Validation(
                "only an approved requisition can be converted".into(),
            ));
        }
        let lines = load_lines(&self.db, id).await?;
        if lines.is_empty() {
            return Err(Error::Validation(
                "a requisition needs at least one line".into(),
            ));
        }
        let order_lines =
            price_lines(&self.db, supplier_id, lines.iter().map(|l| {
                (l.item_id, l.qty, l.needed_by, l.memo.clone())
            }))
            .await?;

        let order_service = OrderService::new(self.db.clone());
        let order = order_service
            .create_draft(NewOrder {
                supplier_id,
                order_date: chrono::Utc::now().date_naive(),
                expected_date: existing.needed_by,
                deliver_to_warehouse_id: existing.warehouse_id,
                delivery_address: None,
                shipping_method: None,
                incoterms: None,
                supplier_contact: None,
                currency: None,
                payment_terms_days: None,
                tax_inclusive: false,
                discount_pct: None,
                discount_amount: None,
                other_charges: None,
                memo: existing.number.as_deref().map(|n| format!("From requisition {n}")),
                reference: existing.number.clone(),
                terms_and_conditions: None,
                lines: order_lines,
                created_by: by,
            })
            .await?;

        // The order commits in its own transaction; re-check under the row
        // lock so two converts cannot both win — the loser's draft order is
        // removed again.
        let txn = self.db.begin().await?;
        let current = load_requisition_locked(&txn, id).await?;
        if RequisitionStatus::parse(&current.status)? != RequisitionStatus::Approved {
            txn.rollback().await?;
            order_service.delete_draft(order.id).await.ok();
            return Err(Error::Validation(
                "the requisition was converted or cancelled by someone else".into(),
            ));
        }
        let now = chrono::Utc::now();
        let mut active: requisition::ActiveModel = current.into();
        active.status = Set(RequisitionStatus::Converted.as_str().to_string());
        active.order_id = Set(Some(order.id));
        active.updated_at = Set(now);
        active.updated_by = Set(by);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    pub async fn list(&self, filter: RequisitionFilter) -> Result<Vec<RequisitionHeader>> {
        let mut query = requisition::Entity::find();
        if let Some(s) = filter.status {
            query = query.filter(requisition::Column::Status.eq(s.as_str()));
        }
        if let Some(warehouse_id) = filter.warehouse_id {
            query = query.filter(requisition::Column::WarehouseId.eq(warehouse_id));
        }
        let rows = query
            .order_by_desc(requisition::Column::CreatedAt)
            .all(&self.db)
            .await?;
        let wh_ids: Vec<Uuid> = rows.iter().map(|r| r.warehouse_id).collect();
        let warehouses: HashMap<Uuid, warehouse::Model> = warehouse::Entity::find()
            .filter(warehouse::Column::Id.is_in(wh_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|w| (w.id, w))
            .collect();
        rows.into_iter()
            .map(|r| {
                Ok(RequisitionHeader {
                    id: r.id,
                    number: r.number.clone(),
                    warehouse_id: r.warehouse_id,
                    warehouse_code: warehouses
                        .get(&r.warehouse_id)
                        .map(|w| w.code.clone())
                        .unwrap_or_default(),
                    needed_by: r.needed_by,
                    status: RequisitionStatus::parse(&r.status)?,
                })
            })
            .collect()
    }

    /// Load a full requisition with lines and labels.
    pub async fn view(&self, id: Uuid) -> Result<RequisitionView> {
        let row = load_requisition(&self.db, id).await?;
        let lines = load_lines(&self.db, id).await?;
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
        let order_number = match row.order_id {
            Some(order_id) => {
                crate::scm::procurement::order::order::Entity::find_by_id(order_id)
                    .one(&self.db)
                    .await?
                    .and_then(|o| o.number)
            }
            None => None,
        };

        let line_views = lines
            .into_iter()
            .map(|l| {
                let item = items.get(&l.item_id);
                RequisitionLineView {
                    id: l.id,
                    line_no: l.line_no,
                    item_id: l.item_id,
                    sku: item.map(|i| i.sku.clone()).unwrap_or_default(),
                    item_name: item.map(|i| i.name.clone()).unwrap_or_default(),
                    qty: l.qty,
                    needed_by: l.needed_by,
                    memo: l.memo,
                }
            })
            .collect();

        Ok(RequisitionView {
            id: row.id,
            number: row.number,
            warehouse_id: row.warehouse_id,
            warehouse_code: wh.map(|w| w.code).unwrap_or_default(),
            needed_by: row.needed_by,
            memo: row.memo,
            status: RequisitionStatus::parse(&row.status)?,
            reject_reason: row.reject_reason,
            order_id: row.order_id,
            order_number,
            submitted_at: row.submitted_at,
            approved_at: row.approved_at,
            created_at: row.created_at,
            lines: line_views,
        })
    }
}

/// Price a set of (item, qty) demands for a supplier: catalog last price,
/// then the item's purchase price, then its last purchase price, then
/// zero. Shared with RFQ-less sourcing; the resulting order is a draft.
pub(crate) async fn price_lines<C, I>(
    conn: &C,
    supplier_id: Uuid,
    demands: I,
) -> Result<Vec<OrderLineInput>>
where
    C: ConnectionTrait,
    I: Iterator<Item = (Uuid, Decimal, Option<chrono::NaiveDate>, Option<String>)>,
{
    let demands: Vec<_> = demands.collect();
    let item_ids: Vec<Uuid> = demands.iter().map(|(item_id, ..)| *item_id).collect();
    let catalog: HashMap<Uuid, item_supplier::Model> = item_supplier::Entity::find()
        .filter(item_supplier::Column::SupplierId.eq(supplier_id))
        .filter(item_supplier::Column::ItemId.is_in(item_ids.clone()))
        .all(conn)
        .await?
        .into_iter()
        .map(|c| (c.item_id, c))
        .collect();
    let items: HashMap<Uuid, item::Model> = item::Entity::find()
        .filter(item::Column::Id.is_in(item_ids))
        .all(conn)
        .await?
        .into_iter()
        .map(|i| (i.id, i))
        .collect();
    demands
        .into_iter()
        .map(|(item_id, qty, needed_by, memo)| {
            let item = items
                .get(&item_id)
                .ok_or_else(|| Error::NotFound(format!("item {item_id}")))?;
            let unit_price = catalog
                .get(&item_id)
                .and_then(|c| c.last_price)
                .or(item.purchase_price)
                .or(item.last_purchase_price)
                .unwrap_or(Decimal::ZERO);
            Ok(OrderLineInput {
                item_id,
                description: None,
                qty,
                unit_price,
                discount_pct: None,
                tax_code_id: None,
                expected_date: needed_by,
                memo,
            })
        })
        .collect()
}

fn clean(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// Draft-time validation: the warehouse is real and active, every item
/// exists, is active and purchasable, quantities positive.
async fn validate_requisition<C: ConnectionTrait>(conn: &C, new: &NewRequisition) -> Result<()> {
    if new.lines.is_empty() {
        return Err(Error::Validation(
            "a requisition needs at least one line".into(),
        ));
    }
    let wh = warehouse::Entity::find_by_id(new.warehouse_id).one(conn).await?;
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
                "warehouse {} does not exist",
                new.warehouse_id
            )));
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
        let Some(item) = items.get(&l.item_id) else {
            return Err(Error::NotFound(format!("item {}", l.item_id)));
        };
        if !item.is_active {
            return Err(Error::Validation(format!(
                "line {line_no}: item {} is inactive",
                item.sku
            )));
        }
        if !item.is_purchasable {
            return Err(Error::Validation(format!(
                "line {line_no}: item {} is not purchasable",
                item.sku
            )));
        }
        if l.qty <= Decimal::ZERO {
            return Err(Error::Validation(format!(
                "line {line_no}: quantity must be positive"
            )));
        }
    }
    Ok(())
}

async fn insert_lines<C: ConnectionTrait>(
    conn: &C,
    requisition_id: Uuid,
    lines: &[RequisitionLineInput],
) -> Result<()> {
    let now = chrono::Utc::now();
    for (i, l) in lines.iter().enumerate() {
        requisition_line::ActiveModel {
            id: Set(Uuid::new_v4()),
            requisition_id: Set(requisition_id),
            line_no: Set((i + 1) as i32),
            item_id: Set(l.item_id),
            qty: Set(l.qty),
            needed_by: Set(l.needed_by),
            memo: Set(l.memo.clone().filter(|m| !m.trim().is_empty())),
            created_at: Set(now),
        }
        .insert(conn)
        .await?;
    }
    Ok(())
}

async fn load_requisition<C: ConnectionTrait>(conn: &C, id: Uuid) -> Result<requisition::Model> {
    requisition::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("purchase requisition {id}")))
}

pub(crate) async fn load_requisition_locked(
    txn: &DatabaseTransaction,
    id: Uuid,
) -> Result<requisition::Model> {
    requisition::Entity::find_by_id(id)
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("purchase requisition {id}")))
}

pub(crate) async fn load_lines<C: ConnectionTrait>(
    conn: &C,
    requisition_id: Uuid,
) -> Result<Vec<requisition_line::Model>> {
    requisition_line::Entity::find()
        .filter(requisition_line::Column::RequisitionId.eq(requisition_id))
        .order_by_asc(requisition_line::Column::LineNo)
        .all(conn)
        .await
        .map_err(Error::from)
}

// ---------------------------------------------------------------------------
// Views (API DTOs)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RequisitionLineView {
    pub id: Uuid,
    pub line_no: i32,
    pub item_id: Uuid,
    pub sku: String,
    pub item_name: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    #[schema(value_type = Option<String>, format = Date)]
    pub needed_by: Option<chrono::NaiveDate>,
    pub memo: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RequisitionView {
    pub id: Uuid,
    pub number: Option<String>,
    pub warehouse_id: Uuid,
    pub warehouse_code: String,
    #[schema(value_type = Option<String>, format = Date)]
    pub needed_by: Option<chrono::NaiveDate>,
    pub memo: Option<String>,
    pub status: RequisitionStatus,
    pub reject_reason: Option<String>,
    /// The draft purchase order convert produced.
    pub order_id: Option<Uuid>,
    pub order_number: Option<String>,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub submitted_at: Option<chrono::DateTime<chrono::Utc>>,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub approved_at: Option<chrono::DateTime<chrono::Utc>>,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub lines: Vec<RequisitionLineView>,
}

/// A row of the requisition register.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RequisitionHeader {
    pub id: Uuid,
    pub number: Option<String>,
    pub warehouse_id: Uuid,
    pub warehouse_code: String,
    #[schema(value_type = Option<String>, format = Date)]
    pub needed_by: Option<chrono::NaiveDate>,
    pub status: RequisitionStatus,
}

pub struct RequisitionFilter {
    pub status: Option<RequisitionStatus>,
    pub warehouse_id: Option<Uuid>,
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
pub struct RequisitionLineRequest {
    pub item_id: Uuid,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    #[schema(value_type = Option<String>, format = Date)]
    pub needed_by: Option<chrono::NaiveDate>,
    pub memo: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateRequisitionRequest {
    pub warehouse_id: Uuid,
    #[schema(value_type = Option<String>, format = Date)]
    pub needed_by: Option<chrono::NaiveDate>,
    pub memo: Option<String>,
    pub lines: Vec<RequisitionLineRequest>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct RejectRequisitionRequest {
    #[serde(default)]
    pub reason: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ConvertRequisitionRequest {
    /// The supplier the draft purchase order goes to.
    pub supplier_id: Uuid,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct ListRequisitionsQuery {
    pub status: Option<RequisitionStatus>,
    pub warehouse_id: Option<Uuid>,
}

fn new_requisition(req: CreateRequisitionRequest, created_by: Option<Uuid>) -> NewRequisition {
    NewRequisition {
        warehouse_id: req.warehouse_id,
        needed_by: req.needed_by,
        memo: req.memo,
        lines: req
            .lines
            .into_iter()
            .map(|l| RequisitionLineInput {
                item_id: l.item_id,
                qty: l.qty,
                needed_by: l.needed_by,
                memo: l.memo,
            })
            .collect(),
        created_by,
    }
}

pub(crate) fn routes() -> Router {
    Router::new()
        .route(
            "/procurement/requisitions",
            get(list_requisitions).post(create_requisition),
        )
        .route(
            "/procurement/requisitions/{id}",
            get(get_requisition)
                .put(update_requisition)
                .delete(delete_requisition),
        )
        .route(
            "/procurement/requisitions/{id}/submit",
            post(submit_requisition),
        )
        .route(
            "/procurement/requisitions/{id}/approve",
            post(approve_requisition),
        )
        .route(
            "/procurement/requisitions/{id}/reject",
            post(reject_requisition),
        )
        .route(
            "/procurement/requisitions/{id}/cancel",
            post(cancel_requisition),
        )
        .route(
            "/procurement/requisitions/{id}/convert",
            post(convert_requisition),
        )
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    list_requisitions,
    get_requisition,
    create_requisition,
    update_requisition,
    delete_requisition,
    submit_requisition,
    approve_requisition,
    reject_requisition,
    cancel_requisition,
    convert_requisition
))]
struct ApiDoc;

#[utoipa::path(get, path = "/procurement/requisitions", tag = "procurement",
    params(ListRequisitionsQuery),
    responses((status = 200, body = Vec<RequisitionHeader>)))]
async fn list_requisitions(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<ListRequisitionsQuery>,
) -> Result<Json<Vec<RequisitionHeader>>> {
    authz.require(names::REQUISITIONS_VIEW).await?;
    RequisitionService::new(db)
        .list(RequisitionFilter {
            status: q.status,
            warehouse_id: q.warehouse_id,
        })
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/procurement/requisitions/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "Requisition id")),
    responses((status = 200, body = RequisitionView)))]
async fn get_requisition(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<RequisitionView>> {
    authz.require(names::REQUISITIONS_VIEW).await?;
    RequisitionService::new(db).view(id).await.map(Json)
}

#[utoipa::path(post, path = "/procurement/requisitions", tag = "procurement",
    request_body = CreateRequisitionRequest,
    responses((status = 200, body = RequisitionView)))]
async fn create_requisition(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(req): Json<CreateRequisitionRequest>,
) -> Result<Json<RequisitionView>> {
    authz.require(names::REQUISITIONS_CREATE).await?;
    let view = RequisitionService::new(db)
        .create_draft(new_requisition(req, Some(authz.user.id)))
        .await?;
    audit.0.created("scm.requisition", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(put, path = "/procurement/requisitions/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "Requisition id")),
    request_body = CreateRequisitionRequest,
    responses((status = 200, body = RequisitionView)))]
async fn update_requisition(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<CreateRequisitionRequest>,
) -> Result<Json<RequisitionView>> {
    authz.require(names::REQUISITIONS_CREATE).await?;
    let service = RequisitionService::new(db);
    let before = service.view(id).await?;
    let after = service.update_draft(id, new_requisition(req, None)).await?;
    audit.0.updated("scm.requisition", id, &before, &after).await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/procurement/requisitions/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "Requisition id")),
    responses((status = 200, body = RequisitionView)))]
async fn delete_requisition(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<RequisitionView>> {
    authz.require(names::REQUISITIONS_CREATE).await?;
    let view = RequisitionService::new(db).delete_draft(id).await?;
    audit.0.deleted("scm.requisition", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/requisitions/{id}/submit", tag = "procurement",
    params(("id" = Uuid, Path, description = "Requisition id")),
    responses((status = 200, body = RequisitionView)))]
async fn submit_requisition(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Extension(numbering): Extension<Numbering>,
    Path(id): Path<Uuid>,
) -> Result<Json<RequisitionView>> {
    authz.require(names::REQUISITIONS_SUBMIT).await?;
    let view = RequisitionService::new(db)
        .submit(id, &numbering, Some(authz.user.id))
        .await?;
    audit
        .0
        .event(format!(
            "submitted requisition {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/requisitions/{id}/approve", tag = "procurement",
    params(("id" = Uuid, Path, description = "Requisition id")),
    responses((status = 200, body = RequisitionView)))]
async fn approve_requisition(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<RequisitionView>> {
    authz.require(names::REQUISITIONS_APPROVE).await?;
    let view = RequisitionService::new(db)
        .approve(id, Some(authz.user.id))
        .await?;
    audit
        .0
        .event(format!(
            "approved requisition {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/requisitions/{id}/reject", tag = "procurement",
    params(("id" = Uuid, Path, description = "Requisition id")),
    request_body = RejectRequisitionRequest,
    responses((status = 200, body = RequisitionView)))]
async fn reject_requisition(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<RejectRequisitionRequest>,
) -> Result<Json<RequisitionView>> {
    authz.require(names::REQUISITIONS_APPROVE).await?;
    let view = RequisitionService::new(db)
        .reject(id, &req.reason, Some(authz.user.id))
        .await?;
    audit
        .0
        .event(format!(
            "rejected requisition {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/requisitions/{id}/cancel", tag = "procurement",
    params(("id" = Uuid, Path, description = "Requisition id")),
    responses((status = 200, body = RequisitionView)))]
async fn cancel_requisition(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<RequisitionView>> {
    authz.require(names::REQUISITIONS_CREATE).await?;
    let view = RequisitionService::new(db)
        .cancel(id, Some(authz.user.id))
        .await?;
    audit
        .0
        .event(format!(
            "cancelled requisition {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/requisitions/{id}/convert", tag = "procurement",
    params(("id" = Uuid, Path, description = "Requisition id")),
    request_body = ConvertRequisitionRequest,
    responses((status = 200, body = RequisitionView)))]
async fn convert_requisition(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<ConvertRequisitionRequest>,
) -> Result<Json<RequisitionView>> {
    authz.require(names::REQUISITIONS_CONVERT).await?;
    let view = RequisitionService::new(db)
        .convert(id, req.supplier_id, Some(authz.user.id))
        .await?;
    audit
        .0
        .event(format!(
            "converted requisition {} to purchase order {}",
            view.number.as_deref().unwrap_or(""),
            view.order_number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}
