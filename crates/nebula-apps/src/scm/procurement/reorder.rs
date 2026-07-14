//! Automatic reordering: the worker that turns reorder policy into draft
//! purchase orders.
//!
//! A stock position is short when `on_hand + on_order + drafted` sits at
//! or below its effective reorder level (the level row's override, else
//! the item's) — `drafted` being the undelivered quantity on draft and
//! submitted orders, which `on_order` does not carry yet; counting it is
//! what keeps repeated runs from drafting the same shortage twice. The
//! order quantity is the effective reorder quantity, else up to the
//! item's max level, else back up to the reorder level — then raised to
//! the minimum order quantity and rounded up to the order multiple.
//!
//! The supplier is the item-supplier catalog's preferred entry, else the
//! item's preferred supplier, else the catalog entry bought from most
//! recently; suppliers on hold fall through to the next candidate. One
//! draft order per supplier × warehouse, priced through the same fallback
//! chain requisitions use, all in one transaction under an advisory lock
//! so concurrent runs cannot double-draft. Everything it makes is a
//! *draft* — a buyer still reviews, submits and approves.
//!
//! Runs on an interval per database and on demand via
//! `POST /procurement/reorder/run`.

use crate::scm::inventory::item::item;
use crate::scm::inventory::stock::level;
use crate::scm::inventory::warehouse;
use crate::scm::procurement::order::{NewOrder, create_draft_in, order, order_line};
use crate::scm::procurement::permissions::names;
use crate::scm::procurement::requisition::price_lines;
use crate::scm::procurement::supplier::{item_supplier, supplier};
use axum::routing::post;
use axum::{Json, Router};
use nebula::audit::Audit;
use nebula::auth::Authz;
use nebula::error::Result;
use nebula::{ModuleContext, TenantDb, sea_orm};
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, DbBackend, Statement, TransactionTrait};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// How often each database is scanned for shortages.
const RUN_INTERVAL_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

/// One draft order a run produced.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ReorderOrderView {
    pub order_id: Uuid,
    pub supplier_id: Uuid,
    pub supplier_code: String,
    pub supplier_name: String,
    pub warehouse_id: Uuid,
    pub warehouse_code: String,
    pub lines: i64,
}

/// One shortage a run could not order for, and why.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ReorderSkipView {
    pub item_id: Uuid,
    pub sku: String,
    pub warehouse_code: String,
    pub reason: String,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ReorderRunView {
    pub orders: Vec<ReorderOrderView>,
    pub skipped: Vec<ReorderSkipView>,
}

/// Scan every stock position and draft purchase orders for the short
/// ones. The whole run is one transaction under an advisory lock, so a
/// worker tick and a manual run cannot draft the same shortage twice.
pub async fn run(db: &DatabaseConnection) -> Result<ReorderRunView> {
    let txn = db.begin().await?;
    txn.execute(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 74))",
        ["scm.reorder".into()],
    ))
    .await?;

    let levels = level::Entity::find().all(&txn).await?;
    let item_ids: Vec<Uuid> = levels.iter().map(|l| l.item_id).collect();
    let items: HashMap<Uuid, item::Model> = item::Entity::find()
        .filter(item::Column::Id.is_in(item_ids))
        .all(&txn)
        .await?
        .into_iter()
        .map(|i| (i.id, i))
        .collect();
    let warehouses: HashMap<Uuid, warehouse::Model> = warehouse::Entity::find()
        .all(&txn)
        .await?
        .into_iter()
        .map(|w| (w.id, w))
        .collect();

    // Undelivered quantity already drafted or submitted, per item ×
    // warehouse — `on_order` only counts approved orders.
    let pipeline_orders: HashMap<Uuid, order::Model> = order::Entity::find()
        .filter(order::Column::Status.is_in(["draft", "submitted"]))
        .all(&txn)
        .await?
        .into_iter()
        .map(|o| (o.id, o))
        .collect();
    let mut drafted: HashMap<(Uuid, Uuid), Decimal> = HashMap::new();
    if !pipeline_orders.is_empty() {
        let order_ids: Vec<Uuid> = pipeline_orders.keys().copied().collect();
        for l in order_line::Entity::find()
            .filter(order_line::Column::OrderId.is_in(order_ids))
            .all(&txn)
            .await?
        {
            let Some(o) = pipeline_orders.get(&l.order_id) else {
                continue;
            };
            let remaining = l.qty - l.received_qty;
            if remaining > Decimal::ZERO {
                *drafted
                    .entry((l.item_id, o.deliver_to_warehouse_id))
                    .or_default() += remaining;
            }
        }
    }

    // Find the shortages and how much to order.
    struct Shortage {
        item_id: Uuid,
        warehouse_id: Uuid,
        qty: Decimal,
        memo: String,
    }
    let mut shortages: Vec<Shortage> = Vec::new();
    let mut skipped: Vec<ReorderSkipView> = Vec::new();
    let wh_code = |warehouses: &HashMap<Uuid, warehouse::Model>, id: Uuid| {
        warehouses
            .get(&id)
            .map(|w| w.code.clone())
            .unwrap_or_default()
    };
    for row in &levels {
        let Some(it) = items.get(&row.item_id) else {
            continue;
        };
        let Some(reorder_level) = row.reorder_level.or(it.reorder_level) else {
            continue;
        };
        let position = row.on_hand
            + row.on_order
            + drafted
                .get(&(row.item_id, row.warehouse_id))
                .copied()
                .unwrap_or_default();
        if position > reorder_level {
            continue;
        }
        if !it.is_active || !it.is_purchasable {
            skipped.push(ReorderSkipView {
                item_id: it.id,
                sku: it.sku.clone(),
                warehouse_code: wh_code(&warehouses, row.warehouse_id),
                reason: "item is inactive or not purchasable".into(),
            });
            continue;
        }
        if !warehouses.get(&row.warehouse_id).is_some_and(|w| w.is_active) {
            skipped.push(ReorderSkipView {
                item_id: it.id,
                sku: it.sku.clone(),
                warehouse_code: wh_code(&warehouses, row.warehouse_id),
                reason: "warehouse is inactive".into(),
            });
            continue;
        }
        let mut qty = row
            .reorder_qty
            .or(it.reorder_qty)
            .or(it.max_level.map(|m| m - position))
            .unwrap_or(reorder_level - position);
        if let Some(min) = it.min_order_qty {
            qty = qty.max(min);
        }
        if let Some(multiple) = it.order_multiple.filter(|m| *m > Decimal::ZERO) {
            let steps = (qty / multiple).ceil();
            qty = steps * multiple;
        }
        if qty <= Decimal::ZERO {
            skipped.push(ReorderSkipView {
                item_id: it.id,
                sku: it.sku.clone(),
                warehouse_code: wh_code(&warehouses, row.warehouse_id),
                reason: "reorder policy yields nothing to order".into(),
            });
            continue;
        }
        shortages.push(Shortage {
            item_id: it.id,
            warehouse_id: row.warehouse_id,
            qty,
            memo: format!(
                "Auto reorder: on hand {}, on order {}, reorder level {}",
                row.on_hand.normalize(),
                row.on_order.normalize(),
                reorder_level.normalize()
            ),
        });
    }

    // Choose a supplier per shortage: catalog preferred, item preferred,
    // most recently bought-from catalog entry; skip anyone on hold.
    let shortage_item_ids: Vec<Uuid> = shortages.iter().map(|s| s.item_id).collect();
    let mut catalog: HashMap<Uuid, Vec<item_supplier::Model>> = HashMap::new();
    for c in item_supplier::Entity::find()
        .filter(item_supplier::Column::ItemId.is_in(shortage_item_ids))
        .filter(item_supplier::Column::IsActive.eq(true))
        .all(&txn)
        .await?
    {
        catalog.entry(c.item_id).or_default().push(c);
    }
    let suppliers: HashMap<Uuid, supplier::Model> = supplier::Entity::find()
        .all(&txn)
        .await?
        .into_iter()
        .map(|s| (s.id, s))
        .collect();
    let usable = |id: Uuid| {
        suppliers
            .get(&id)
            .is_some_and(|s| s.is_active && !s.on_hold)
    };

    let mut groups: HashMap<(Uuid, Uuid), Vec<Shortage>> = HashMap::new();
    for s in shortages {
        let it = &items[&s.item_id];
        let mut candidates: Vec<Uuid> = Vec::new();
        let entries = catalog.get(&s.item_id).map(|v| v.as_slice()).unwrap_or(&[]);
        candidates.extend(entries.iter().filter(|c| c.is_preferred).map(|c| c.supplier_id));
        candidates.extend(it.preferred_supplier_id);
        let mut by_recency: Vec<&item_supplier::Model> = entries.iter().collect();
        by_recency.sort_by(|a, b| b.last_purchased_on.cmp(&a.last_purchased_on));
        candidates.extend(by_recency.iter().map(|c| c.supplier_id));
        match candidates.into_iter().find(|id| usable(*id)) {
            Some(supplier_id) => groups
                .entry((supplier_id, s.warehouse_id))
                .or_default()
                .push(s),
            None => skipped.push(ReorderSkipView {
                item_id: s.item_id,
                sku: it.sku.clone(),
                warehouse_code: wh_code(&warehouses, s.warehouse_id),
                reason: "no usable supplier (none known, inactive, or on hold)".into(),
            }),
        }
    }

    // One draft order per supplier × warehouse, priced like a requisition
    // convert.
    let mut orders: Vec<ReorderOrderView> = Vec::new();
    let today = chrono::Utc::now().date_naive();
    let mut ordered_groups: Vec<((Uuid, Uuid), Vec<Shortage>)> = groups.into_iter().collect();
    ordered_groups.sort_by_key(|((s, w), _)| (*s, *w));
    for ((supplier_id, warehouse_id), group) in ordered_groups {
        let lines = price_lines(
            &txn,
            supplier_id,
            group
                .iter()
                .map(|s| (s.item_id, s.qty, None, Some(s.memo.clone()))),
        )
        .await?;
        let line_count = lines.len() as i64;
        let order_id = create_draft_in(
            &txn,
            NewOrder {
                supplier_id,
                order_date: today,
                expected_date: None,
                deliver_to_warehouse_id: warehouse_id,
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
                memo: Some("Automatic reorder".into()),
                reference: None,
                terms_and_conditions: None,
                lines,
                created_by: None,
            },
        )
        .await?;
        let s = suppliers.get(&supplier_id);
        orders.push(ReorderOrderView {
            order_id,
            supplier_id,
            supplier_code: s.map(|s| s.code.clone()).unwrap_or_default(),
            supplier_name: s.map(|s| s.name.clone()).unwrap_or_default(),
            warehouse_id,
            warehouse_code: wh_code(&warehouses, warehouse_id),
            lines: line_count,
        });
    }
    txn.commit().await?;
    Ok(ReorderRunView { orders, skipped })
}

// ---------------------------------------------------------------------------
// The worker
// ---------------------------------------------------------------------------

/// Scan the main and every tenant database on an interval. Draft orders
/// are harmless if nobody looks at them, and the drafted-quantity check
/// makes ticks idempotent while a shortage persists.
pub(crate) fn spawn_worker(ctx: &mut ModuleContext) {
    let tenants = ctx.tenants();
    let main = ctx.db().cloned();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(RUN_INTERVAL_SECS)).await;
            let mut dbs: Vec<(String, DatabaseConnection)> = Vec::new();
            if let Some(db) = &main {
                dbs.push(("main".into(), db.clone()));
            }
            if let Some(tenants) = &tenants {
                match tenants.find_all().await {
                    Ok(list) => {
                        for tenant in list.into_iter().filter(|t| t.is_active) {
                            match tenants.connection_for(&tenant).await {
                                Ok(db) => dbs.push((tenant.name.clone(), db)),
                                Err(e) => tracing::warn!(tenant = %tenant.name, error = %e,
                                    "reorder worker could not reach tenant database"),
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "reorder worker could not list tenants")
                    }
                }
            }
            for (name, db) in dbs {
                match run(&db).await {
                    Ok(view) if !view.orders.is_empty() => {
                        tracing::info!(database = %name, orders = view.orders.len(),
                            "auto reorder drafted purchase orders");
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(database = %name, error = %e, "auto reorder run failed")
                    }
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

pub(crate) fn routes() -> Router {
    Router::new().route("/procurement/reorder/run", post(run_reorder))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(run_reorder))]
struct ApiDoc;

#[utoipa::path(post, path = "/procurement/reorder/run", tag = "procurement",
    responses((status = 200, body = ReorderRunView)))]
async fn run_reorder(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
) -> Result<Json<ReorderRunView>> {
    authz.require(names::ORDERS_CREATE).await?;
    let view = run(&db).await?;
    audit
        .0
        .event(format!(
            "ran auto reorder: {} draft orders, {} skipped positions",
            view.orders.len(),
            view.skipped.len()
        ))
        .await;
    Ok(Json(view))
}
