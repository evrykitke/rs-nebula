//! Read-side stock queries: current levels, the stock ledger (SLE) and
//! valuation totals.
//!
//! Everything here reads what the engine maintains — the level rows for
//! "now", the ledger for "how" — and joins the item/warehouse labels a
//! screen needs. `available = on_hand − reserved` is computed here, never
//! stored. The same queries feed the JSON endpoints and the framework
//! reports in [`super::reports`].

use crate::scm::inventory::item::{self, item as item_entity};
use crate::scm::inventory::moves::doc;
use crate::scm::inventory::permissions::names;
use crate::scm::inventory::stock::{ledger, level, level_average};
use crate::scm::inventory::warehouse;
use axum::extract::Query;
use axum::routing::get;
use axum::{Json, Router};
use nebula::auth::Authz;
use nebula::error::Result;
use nebula::{TenantDb, sea_orm};
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseConnection, QueryOrder, QuerySelect};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Ledger pages are capped so one request can never drag a whole history.
const LEDGER_PAGE_MAX: u64 = 1000;
const LEDGER_PAGE_DEFAULT: u64 = 500;

/// One item × warehouse position.
#[derive(Serialize, Deserialize, utoipa::ToSchema)]
pub struct LevelView {
    pub item_id: Uuid,
    pub sku: String,
    pub item_name: String,
    pub warehouse_id: Uuid,
    pub warehouse_code: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub on_hand: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub reserved: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub on_order: Decimal,
    /// on_hand − reserved, computed.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub available: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub value: Decimal,
    /// Running moving-average cost (zero when empty).
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub avg_cost: Decimal,
    /// Effective policy: the level row's override, else the item's.
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub reorder_level: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub reorder_qty: Option<Decimal>,
    /// True when a reorder level is set and on_hand is at or below it.
    pub below_reorder: bool,
}

/// One stock ledger row with its document labels.
#[derive(Serialize, Deserialize, utoipa::ToSchema)]
pub struct LedgerRowView {
    pub seq: i64,
    #[schema(value_type = String, format = Date)]
    pub entry_date: chrono::NaiveDate,
    pub move_id: Uuid,
    pub number: Option<String>,
    pub move_type: Option<String>,
    pub item_id: Uuid,
    pub sku: String,
    pub warehouse_id: Uuid,
    pub warehouse_code: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty_delta: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty_after: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub unit_cost: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub value_delta: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub value_after: Decimal,
    #[schema(value_type = String, format = DateTime)]
    pub posted_at: chrono::DateTime<chrono::Utc>,
}

/// Valuation totals for one warehouse.
#[derive(Serialize, Deserialize, utoipa::ToSchema)]
pub struct WarehouseValuation {
    pub warehouse_id: Uuid,
    pub warehouse_code: String,
    pub warehouse_name: String,
    /// Distinct items with stock on hand.
    pub items: u64,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total_qty: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total_value: Decimal,
}

/// Stock value across warehouses — what the GL books as inventory.
#[derive(Serialize, Deserialize, utoipa::ToSchema)]
pub struct ValuationSummary {
    pub warehouses: Vec<WarehouseValuation>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total_value: Decimal,
}

pub struct LevelsFilter {
    pub warehouse_id: Option<Uuid>,
    pub item_id: Option<Uuid>,
    pub below_reorder: bool,
}

pub struct LedgerFilter {
    pub item_id: Option<Uuid>,
    pub warehouse_id: Option<Uuid>,
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
    /// Page cursor: rows with seq greater than this.
    pub after_seq: Option<i64>,
    pub limit: Option<u64>,
}

/// Read-side stock queries over one (tenant) connection.
pub struct StockQueries {
    db: DatabaseConnection,
}

impl StockQueries {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn levels(&self, filter: LevelsFilter) -> Result<Vec<LevelView>> {
        let mut query = level::Entity::find();
        if let Some(w) = filter.warehouse_id {
            query = query.filter(level::Column::WarehouseId.eq(w));
        }
        if let Some(i) = filter.item_id {
            query = query.filter(level::Column::ItemId.eq(i));
        }
        let rows = query.all(&self.db).await?;

        let items = item_labels(&self.db, rows.iter().map(|r| r.item_id)).await?;
        let warehouses = warehouse_labels(&self.db, rows.iter().map(|r| r.warehouse_id)).await?;

        let mut views = Vec::with_capacity(rows.len());
        for row in rows {
            let item = items.get(&row.item_id);
            let wh = warehouses.get(&row.warehouse_id);
            let reorder_level = row
                .reorder_level
                .or(item.and_then(|i| i.reorder_level));
            let reorder_qty = row.reorder_qty.or(item.and_then(|i| i.reorder_qty));
            let below = reorder_level.is_some_and(|r| row.on_hand <= r);
            if filter.below_reorder && !below {
                continue;
            }
            views.push(LevelView {
                item_id: row.item_id,
                sku: item.map(|i| i.sku.clone()).unwrap_or_default(),
                item_name: item.map(|i| i.name.clone()).unwrap_or_default(),
                warehouse_id: row.warehouse_id,
                warehouse_code: wh.map(|w| w.code.clone()).unwrap_or_default(),
                on_hand: row.on_hand,
                reserved: row.reserved,
                on_order: row.on_order,
                available: row.on_hand - row.reserved,
                value: row.value,
                avg_cost: level_average(&row),
                reorder_level,
                reorder_qty,
                below_reorder: below,
            });
        }
        views.sort_by(|a, b| (&a.sku, &a.warehouse_code).cmp(&(&b.sku, &b.warehouse_code)));
        Ok(views)
    }

    pub async fn ledger(&self, filter: LedgerFilter) -> Result<Vec<LedgerRowView>> {
        let mut query = ledger::Entity::find();
        if let Some(i) = filter.item_id {
            query = query.filter(ledger::Column::ItemId.eq(i));
        }
        if let Some(w) = filter.warehouse_id {
            query = query.filter(ledger::Column::WarehouseId.eq(w));
        }
        if let Some(from) = filter.from {
            query = query.filter(ledger::Column::EntryDate.gte(from));
        }
        if let Some(to) = filter.to {
            query = query.filter(ledger::Column::EntryDate.lte(to));
        }
        if let Some(after) = filter.after_seq {
            query = query.filter(ledger::Column::Seq.gt(after));
        }
        let limit = filter
            .limit
            .unwrap_or(LEDGER_PAGE_DEFAULT)
            .min(LEDGER_PAGE_MAX);
        let rows = query
            .order_by_asc(ledger::Column::Seq)
            .limit(limit)
            .all(&self.db)
            .await?;

        let items = item_labels(&self.db, rows.iter().map(|r| r.item_id)).await?;
        let warehouses = warehouse_labels(&self.db, rows.iter().map(|r| r.warehouse_id)).await?;
        let move_ids: Vec<Uuid> = rows.iter().map(|r| r.move_id).collect();
        let moves: HashMap<Uuid, doc::Model> = doc::Entity::find()
            .filter(doc::Column::Id.is_in(move_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|m| (m.id, m))
            .collect();

        Ok(rows
            .into_iter()
            .map(|r| {
                let mv = moves.get(&r.move_id);
                LedgerRowView {
                    seq: r.seq,
                    entry_date: r.entry_date,
                    move_id: r.move_id,
                    number: mv.and_then(|m| m.number.clone()),
                    move_type: mv.map(|m| m.move_type.clone()),
                    item_id: r.item_id,
                    sku: items.get(&r.item_id).map(|i| i.sku.clone()).unwrap_or_default(),
                    warehouse_id: r.warehouse_id,
                    warehouse_code: warehouses
                        .get(&r.warehouse_id)
                        .map(|w| w.code.clone())
                        .unwrap_or_default(),
                    qty_delta: r.qty_delta,
                    qty_after: r.qty_after,
                    unit_cost: r.unit_cost,
                    value_delta: r.value_delta,
                    value_after: r.value_after,
                    posted_at: r.posted_at,
                }
            })
            .collect())
    }

    pub async fn valuation(&self, warehouse_id: Option<Uuid>) -> Result<ValuationSummary> {
        let mut query = level::Entity::find();
        if let Some(w) = warehouse_id {
            query = query.filter(level::Column::WarehouseId.eq(w));
        }
        let rows = query.all(&self.db).await?;
        let warehouses = warehouse_labels(&self.db, rows.iter().map(|r| r.warehouse_id)).await?;

        let mut by_warehouse: HashMap<Uuid, WarehouseValuation> = HashMap::new();
        let mut total_value = Decimal::ZERO;
        for row in rows {
            let entry = by_warehouse.entry(row.warehouse_id).or_insert_with(|| {
                let wh = warehouses.get(&row.warehouse_id);
                WarehouseValuation {
                    warehouse_id: row.warehouse_id,
                    warehouse_code: wh.map(|w| w.code.clone()).unwrap_or_default(),
                    warehouse_name: wh.map(|w| w.name.clone()).unwrap_or_default(),
                    items: 0,
                    total_qty: Decimal::ZERO,
                    total_value: Decimal::ZERO,
                }
            });
            if !row.on_hand.is_zero() {
                entry.items += 1;
            }
            entry.total_qty += row.on_hand;
            entry.total_value += row.value;
            total_value += row.value;
        }
        let mut warehouses: Vec<WarehouseValuation> = by_warehouse.into_values().collect();
        warehouses.sort_by(|a, b| a.warehouse_code.cmp(&b.warehouse_code));
        Ok(ValuationSummary {
            warehouses,
            total_value,
        })
    }
}

async fn item_labels<C: ConnectionTrait>(
    conn: &C,
    ids: impl Iterator<Item = Uuid>,
) -> Result<HashMap<Uuid, item_entity::Model>> {
    let ids: Vec<Uuid> = ids.collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(item::item::Entity::find()
        .filter(item::item::Column::Id.is_in(ids))
        .all(conn)
        .await?
        .into_iter()
        .map(|i| (i.id, i))
        .collect())
}

async fn warehouse_labels<C: ConnectionTrait>(
    conn: &C,
    ids: impl Iterator<Item = Uuid>,
) -> Result<HashMap<Uuid, warehouse::Model>> {
    let ids: Vec<Uuid> = ids.collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(warehouse::Entity::find()
        .filter(warehouse::Column::Id.is_in(ids))
        .all(conn)
        .await?
        .into_iter()
        .map(|w| (w.id, w))
        .collect())
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

pub(crate) fn routes() -> Router {
    Router::new()
        .route("/inventory/stock/levels", get(list_levels))
        .route("/inventory/stock/ledger", get(list_ledger))
        .route("/inventory/stock/valuation", get(get_valuation))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(list_levels, list_ledger, get_valuation))]
struct ApiDoc;

#[derive(Deserialize, utoipa::IntoParams)]
pub struct LevelsQuery {
    pub warehouse_id: Option<Uuid>,
    pub item_id: Option<Uuid>,
    /// Only positions at or below their effective reorder level.
    #[serde(default)]
    pub below_reorder: bool,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct LedgerQuery {
    pub item_id: Option<Uuid>,
    pub warehouse_id: Option<Uuid>,
    /// Entry date range, inclusive.
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
    /// Page cursor: rows with seq greater than this.
    pub after_seq: Option<i64>,
    /// Page size, capped at 1000 (default 500).
    pub limit: Option<u64>,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct ValuationQuery {
    pub warehouse_id: Option<Uuid>,
}

#[utoipa::path(get, path = "/inventory/stock/levels", tag = "inventory",
    params(LevelsQuery),
    responses((status = 200, body = Vec<LevelView>)))]
async fn list_levels(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<LevelsQuery>,
) -> Result<Json<Vec<LevelView>>> {
    authz.require(names::MOVEMENTS_VIEW).await?;
    StockQueries::new(db)
        .levels(LevelsFilter {
            warehouse_id: q.warehouse_id,
            item_id: q.item_id,
            below_reorder: q.below_reorder,
        })
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/inventory/stock/ledger", tag = "inventory",
    params(LedgerQuery),
    responses((status = 200, body = Vec<LedgerRowView>)))]
async fn list_ledger(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<LedgerQuery>,
) -> Result<Json<Vec<LedgerRowView>>> {
    authz.require(names::MOVEMENTS_VIEW).await?;
    StockQueries::new(db)
        .ledger(LedgerFilter {
            item_id: q.item_id,
            warehouse_id: q.warehouse_id,
            from: q.from,
            to: q.to,
            after_seq: q.after_seq,
            limit: q.limit,
        })
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/inventory/stock/valuation", tag = "inventory",
    params(ValuationQuery),
    responses((status = 200, body = ValuationSummary)))]
async fn get_valuation(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<ValuationQuery>,
) -> Result<Json<ValuationSummary>> {
    authz.require(names::REPORTS_VIEW).await?;
    StockQueries::new(db).valuation(q.warehouse_id).await.map(Json)
}
