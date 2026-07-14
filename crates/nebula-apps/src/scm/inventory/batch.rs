//! Batch (lot) and serial-number tracking: the dormant dimensions wake up.
//!
//! An item opts in per dimension (`track_batches`, `track_serials`); from
//! then on posting **demands** the identifiers — a receipt names (and
//! creates) the lot it brings in, an issue names the lot it takes and
//! exactly which serial units leave. Draft lines capture the *names*
//! (`batch_no`, `serial_nos`); the masters are only written when a
//! document posts, so deleted drafts never leave orphan lots behind.
//!
//! Costing is untouched: valuation stays moving-average per
//! item × warehouse. A batch is a **tracking** dimension — its per-lot
//! quantity is the sum of its ledger rows, validated under the same level
//! row lock every movement already takes, so lot balances can never go
//! negative and never race.
//!
//! Serial lifecycle: `in_stock` (with its current warehouse) → `issued`
//! (left through an issue) or `scrapped` (count-down, reversal of the
//! receipt that brought it in). A serial that comes back (issue reversal,
//! re-receipt) returns to `in_stock` — the master remembers the whole
//! history through the `inventory_move_line_serials` joins.

use crate::scm::inventory::item::item;
use crate::scm::inventory::permissions::names;
use crate::scm::inventory::stock::ledger;
use axum::extract::{Path, Query};
use axum::routing::get;
use axum::{Json, Router};
use nebula::auth::Authz;
use nebula::error::{Error, Result};
use nebula::{TenantDb, sea_orm};
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseTransaction, FromQueryResult, QueryOrder, QuerySelect, Set};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use uuid::Uuid;

/// The batch (lot) master.
pub mod batch {
    use nebula::sea_orm;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, utoipa::ToSchema)]
    #[schema(as = InventoryBatch)]
    #[sea_orm(table_name = "inventory_batches")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub item_id: Uuid,
        pub batch_no: String,
        #[schema(value_type = Option<String>, format = Date)]
        pub manufactured_on: Option<Date>,
        #[schema(value_type = Option<String>, format = Date)]
        pub expires_on: Option<Date>,
        pub supplier_batch_no: Option<String>,
        pub notes: Option<String>,
        pub is_active: bool,
        #[schema(value_type = String, format = DateTime)]
        pub created_at: DateTimeUtc,
        pub created_by: Option<Uuid>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// The serial-number master.
pub mod serial {
    use nebula::sea_orm;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, utoipa::ToSchema)]
    #[schema(as = InventorySerial)]
    #[sea_orm(table_name = "inventory_serials")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub item_id: Uuid,
        pub serial_no: String,
        pub batch_id: Option<Uuid>,
        pub warehouse_id: Option<Uuid>,
        pub status: String,
        #[schema(value_type = Option<String>, format = Date)]
        pub warranty_until: Option<Date>,
        pub notes: Option<String>,
        #[schema(value_type = String, format = DateTime)]
        pub created_at: DateTimeUtc,
        pub created_by: Option<Uuid>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// Which serials a posted movement line touched (traceability).
pub mod line_serial {
    use nebula::sea_orm;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "inventory_move_line_serials")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub move_line_id: Uuid,
        #[sea_orm(primary_key, auto_increment = false)]
        pub serial_id: Uuid,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// Where a serialized unit is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SerialStatus {
    InStock,
    Issued,
    Scrapped,
}

impl SerialStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SerialStatus::InStock => "in_stock",
            SerialStatus::Issued => "issued",
            SerialStatus::Scrapped => "scrapped",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "in_stock" => Ok(SerialStatus::InStock),
            "issued" => Ok(SerialStatus::Issued),
            "scrapped" => Ok(SerialStatus::Scrapped),
            other => Err(Error::internal(format!("unknown serial status {other:?}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// Posting-time helpers (all on the caller's transaction)
// ---------------------------------------------------------------------------

/// Find the item's lot by number, creating it on first receipt. Expiry
/// defaults from the item's shelf life against the receipt date; a lot
/// received again keeps its original dates.
pub(crate) async fn find_or_create_batch(
    txn: &DatabaseTransaction,
    item: &item::Model,
    batch_no: &str,
    received_on: chrono::NaiveDate,
    supplier_batch_no: Option<&str>,
    created_by: Option<Uuid>,
) -> Result<batch::Model> {
    let batch_no = batch_no.trim();
    if batch_no.is_empty() {
        return Err(Error::Validation(format!(
            "item {} tracks batches; the batch number must not be blank",
            item.sku
        )));
    }
    if let Some(existing) = batch::Entity::find()
        .filter(batch::Column::ItemId.eq(item.id))
        .filter(batch::Column::BatchNo.eq(batch_no))
        .one(txn)
        .await?
    {
        return Ok(existing);
    }
    let expires_on = item
        .shelf_life_days
        .map(|days| received_on + chrono::Duration::days(days as i64));
    batch::ActiveModel {
        id: Set(Uuid::new_v4()),
        item_id: Set(item.id),
        batch_no: Set(batch_no.to_string()),
        manufactured_on: Set(None),
        expires_on: Set(expires_on),
        supplier_batch_no: Set(supplier_batch_no
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())),
        notes: Set(None),
        is_active: Set(true),
        created_at: Set(chrono::Utc::now()),
        created_by: Set(created_by),
    }
    .insert(txn)
    .await
    .map_err(Error::from)
}

/// The item's existing lot by number — for movements that take stock out.
pub(crate) async fn find_batch(
    txn: &DatabaseTransaction,
    item: &item::Model,
    batch_no: &str,
) -> Result<batch::Model> {
    batch::Entity::find()
        .filter(batch::Column::ItemId.eq(item.id))
        .filter(batch::Column::BatchNo.eq(batch_no.trim()))
        .one(txn)
        .await?
        .ok_or_else(|| {
            Error::Validation(format!(
                "item {} has no batch {:?}",
                item.sku,
                batch_no.trim()
            ))
        })
}

/// The lot's on-hand at one warehouse: the sum of its ledger rows. Safe
/// under the item×warehouse level lock the caller already holds.
pub(crate) async fn batch_on_hand(
    txn: &DatabaseTransaction,
    item_id: Uuid,
    warehouse_id: Uuid,
    batch_id: Uuid,
) -> Result<Decimal> {
    #[derive(FromQueryResult)]
    struct Sum {
        total: Option<Decimal>,
    }
    let row = ledger::Entity::find()
        .select_only()
        .column_as(ledger::Column::QtyDelta.sum(), "total")
        .filter(ledger::Column::ItemId.eq(item_id))
        .filter(ledger::Column::WarehouseId.eq(warehouse_id))
        .filter(ledger::Column::BatchId.eq(batch_id))
        .into_model::<Sum>()
        .one(txn)
        .await?;
    Ok(row.and_then(|r| r.total).unwrap_or(Decimal::ZERO))
}

/// Validate a serial-name list against its line quantity: whole quantity,
/// one distinct name per unit.
pub(crate) fn check_serial_names(
    item: &item::Model,
    qty: Decimal,
    names: &[String],
) -> Result<Vec<String>> {
    let names: Vec<String> = names
        .iter()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect();
    let count = Decimal::from(names.len());
    if count != qty {
        return Err(Error::Validation(format!(
            "item {} tracks serial numbers: the line moves {} but names {} serials",
            item.sku,
            qty.normalize(),
            names.len()
        )));
    }
    let distinct: HashSet<&String> = names.iter().collect();
    if distinct.len() != names.len() {
        return Err(Error::Validation(format!(
            "item {}: a serial number is listed twice on one line",
            item.sku
        )));
    }
    Ok(names)
}

/// Bring serial units into stock at `warehouse_id` (receipt, adjustment
/// up, issue reversal). A brand-new serial is created; a returning one
/// must not already be in stock.
pub(crate) async fn serials_in(
    txn: &DatabaseTransaction,
    item: &item::Model,
    move_line_id: Uuid,
    warehouse_id: Uuid,
    batch_id: Option<Uuid>,
    names: &[String],
    received_on: chrono::NaiveDate,
    created_by: Option<Uuid>,
) -> Result<()> {
    for name in names {
        let existing = serial::Entity::find()
            .filter(serial::Column::ItemId.eq(item.id))
            .filter(serial::Column::SerialNo.eq(name))
            .one(txn)
            .await?;
        let serial_id = match existing {
            Some(row) => {
                if SerialStatus::parse(&row.status)? == SerialStatus::InStock {
                    return Err(Error::Validation(format!(
                        "serial {name} of {} is already in stock",
                        item.sku
                    )));
                }
                let id = row.id;
                let mut active: serial::ActiveModel = row.into();
                active.status = Set(SerialStatus::InStock.as_str().to_string());
                active.warehouse_id = Set(Some(warehouse_id));
                if let Some(batch_id) = batch_id {
                    active.batch_id = Set(Some(batch_id));
                }
                active.update(txn).await?;
                id
            }
            None => {
                let id = Uuid::new_v4();
                serial::ActiveModel {
                    id: Set(id),
                    item_id: Set(item.id),
                    serial_no: Set(name.clone()),
                    batch_id: Set(batch_id),
                    warehouse_id: Set(Some(warehouse_id)),
                    status: Set(SerialStatus::InStock.as_str().to_string()),
                    warranty_until: Set(item
                        .warranty_days
                        .map(|days| received_on + chrono::Duration::days(days as i64))),
                    notes: Set(None),
                    created_at: Set(chrono::Utc::now()),
                    created_by: Set(created_by),
                }
                .insert(txn)
                .await?;
                id
            }
        };
        link(txn, move_line_id, serial_id).await?;
    }
    Ok(())
}

/// Take serial units out of stock from `warehouse_id` — `Issued` when they
/// leave through an issue, `Scrapped` for count-downs and reversals.
pub(crate) async fn serials_out(
    txn: &DatabaseTransaction,
    item: &item::Model,
    move_line_id: Uuid,
    warehouse_id: Uuid,
    names: &[String],
    to_status: SerialStatus,
) -> Result<()> {
    for name in names {
        let row = in_stock_at(txn, item, warehouse_id, name).await?;
        let serial_id = row.id;
        let mut active: serial::ActiveModel = row.into();
        active.status = Set(to_status.as_str().to_string());
        active.update(txn).await?;
        link(txn, move_line_id, serial_id).await?;
    }
    Ok(())
}

/// Move serial units between warehouses (transfer or its reversal).
pub(crate) async fn serials_move(
    txn: &DatabaseTransaction,
    item: &item::Model,
    move_line_id: Uuid,
    from_warehouse_id: Uuid,
    to_warehouse_id: Uuid,
    names: &[String],
) -> Result<()> {
    for name in names {
        let row = in_stock_at(txn, item, from_warehouse_id, name).await?;
        let serial_id = row.id;
        let mut active: serial::ActiveModel = row.into();
        active.warehouse_id = Set(Some(to_warehouse_id));
        active.update(txn).await?;
        link(txn, move_line_id, serial_id).await?;
    }
    Ok(())
}

/// The serial names a posted line touched — reversals mirror exactly them.
pub(crate) async fn serial_names_of_line(
    txn: &DatabaseTransaction,
    move_line_id: Uuid,
) -> Result<Vec<String>> {
    let joins = line_serial::Entity::find()
        .filter(line_serial::Column::MoveLineId.eq(move_line_id))
        .all(txn)
        .await?;
    let ids: Vec<Uuid> = joins.iter().map(|j| j.serial_id).collect();
    let rows = serial::Entity::find()
        .filter(serial::Column::Id.is_in(ids))
        .all(txn)
        .await?;
    Ok(rows.into_iter().map(|r| r.serial_no).collect())
}

async fn in_stock_at(
    txn: &DatabaseTransaction,
    item: &item::Model,
    warehouse_id: Uuid,
    name: &str,
) -> Result<serial::Model> {
    let row = serial::Entity::find()
        .filter(serial::Column::ItemId.eq(item.id))
        .filter(serial::Column::SerialNo.eq(name))
        .one(txn)
        .await?
        .ok_or_else(|| {
            Error::Validation(format!("item {} has no serial {name:?}", item.sku))
        })?;
    if SerialStatus::parse(&row.status)? != SerialStatus::InStock
        || row.warehouse_id != Some(warehouse_id)
    {
        return Err(Error::Validation(format!(
            "serial {name} of {} is not in stock at this warehouse",
            item.sku
        )));
    }
    Ok(row)
}

async fn link(txn: &DatabaseTransaction, move_line_id: Uuid, serial_id: Uuid) -> Result<()> {
    line_serial::ActiveModel {
        move_line_id: Set(move_line_id),
        serial_id: Set(serial_id),
    }
    .insert(txn)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Read side (API DTOs + endpoints)
// ---------------------------------------------------------------------------

/// One lot's position, FEFO-ordered when expiries exist.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BatchLevelView {
    pub batch_id: Uuid,
    pub batch_no: String,
    #[schema(value_type = Option<String>, format = Date)]
    pub expires_on: Option<chrono::NaiveDate>,
    pub supplier_batch_no: Option<String>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub on_hand: Decimal,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct BatchLevelsQuery {
    /// Restrict to one warehouse; otherwise all warehouses summed.
    pub warehouse_id: Option<Uuid>,
    /// Include exhausted lots (zero on hand). Default false.
    #[serde(default)]
    pub include_empty: bool,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct SerialsQuery {
    pub status: Option<SerialStatus>,
    pub warehouse_id: Option<Uuid>,
}

pub(crate) fn routes() -> Router {
    Router::new()
        .route("/inventory/items/{id}/batches", get(item_batches))
        .route("/inventory/items/{id}/serials", get(item_serials))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(
    paths(item_batches, item_serials),
    // Referenced from SerialsQuery's query string, which IntoParams does
    // not auto-register the way response bodies are.
    components(schemas(SerialStatus))
)]
struct ApiDoc;

#[utoipa::path(get, path = "/inventory/items/{id}/batches", tag = "inventory",
    params(("id" = Uuid, Path, description = "Item id"), BatchLevelsQuery),
    responses((status = 200, body = Vec<BatchLevelView>)))]
async fn item_batches(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Query(q): Query<BatchLevelsQuery>,
) -> Result<Json<Vec<BatchLevelView>>> {
    authz.require(names::ITEMS_VIEW).await?;

    let batches = batch::Entity::find()
        .filter(batch::Column::ItemId.eq(id))
        .order_by_asc(batch::Column::ExpiresOn)
        .order_by_asc(batch::Column::BatchNo)
        .all(&db)
        .await?;

    #[derive(FromQueryResult)]
    struct BatchSum {
        batch_id: Uuid,
        total: Option<Decimal>,
    }
    let mut sums = ledger::Entity::find()
        .select_only()
        .column(ledger::Column::BatchId)
        .column_as(ledger::Column::QtyDelta.sum(), "total")
        .filter(ledger::Column::ItemId.eq(id))
        .filter(ledger::Column::BatchId.is_not_null())
        .group_by(ledger::Column::BatchId);
    if let Some(wh) = q.warehouse_id {
        sums = sums.filter(ledger::Column::WarehouseId.eq(wh));
    }
    let sums: std::collections::HashMap<Uuid, Decimal> = sums
        .into_model::<BatchSum>()
        .all(&db)
        .await?
        .into_iter()
        .map(|r| (r.batch_id, r.total.unwrap_or(Decimal::ZERO)))
        .collect();

    let rows = batches
        .into_iter()
        .map(|b| BatchLevelView {
            on_hand: sums.get(&b.id).copied().unwrap_or(Decimal::ZERO),
            batch_id: b.id,
            batch_no: b.batch_no,
            expires_on: b.expires_on,
            supplier_batch_no: b.supplier_batch_no,
        })
        .filter(|r| q.include_empty || !r.on_hand.is_zero())
        .collect();
    Ok(Json(rows))
}

#[utoipa::path(get, path = "/inventory/items/{id}/serials", tag = "inventory",
    params(("id" = Uuid, Path, description = "Item id"), SerialsQuery),
    responses((status = 200, body = Vec<serial::Model>)))]
async fn item_serials(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Query(q): Query<SerialsQuery>,
) -> Result<Json<Vec<serial::Model>>> {
    authz.require(names::ITEMS_VIEW).await?;
    let mut query = serial::Entity::find().filter(serial::Column::ItemId.eq(id));
    if let Some(status) = q.status {
        query = query.filter(serial::Column::Status.eq(status.as_str()));
    }
    if let Some(wh) = q.warehouse_id {
        query = query.filter(serial::Column::WarehouseId.eq(wh));
    }
    let rows = query
        .order_by_asc(serial::Column::SerialNo)
        .all(&db)
        .await?;
    Ok(Json(rows))
}
