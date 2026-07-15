//! The stock engine: the one place stock quantities and values change.
//!
//! Movement documents ([`super::moves`]) and procurement's goods receipt
//! call [`StockService::apply`] line by line inside their own posting
//! transaction. Every application locks (or creates) the item×warehouse
//! level row FOR UPDATE first, so concurrent movements of the same stock
//! serialize; negative stock is rejected under that lock; then an
//! immutable ledger row is appended and the level updated — one
//! transaction, one truth.
//!
//! Costing is moving (weighted) average per item × warehouse in the tenant
//! base currency. Rounding rules live here and only here: unit costs carry
//! 6 decimals, ledger money 2. An issue that empties the location flushes
//! the entire remaining value, so zero quantity always means exactly zero
//! value — no residue drift.

use nebula::error::{Error, Result};
use nebula::sea_orm;
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseTransaction, DbBackend, NotSet, QuerySelect, Set, Statement};
use uuid::Uuid;

use super::item::{ItemType, item, uom};

/// One immutable stock ledger row.
pub mod ledger {
    use nebula::sea_orm;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "inventory_stock_ledger")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub seq: i64,
        pub move_id: Uuid,
        pub move_line_id: Uuid,
        pub item_id: Uuid,
        pub warehouse_id: Uuid,
        pub batch_id: Option<Uuid>,
        pub entry_date: Date,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub qty_delta: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub qty_after: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub unit_cost: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))")]
        pub value_delta: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))")]
        pub value_after: Decimal,
        pub posted_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// The per-item×warehouse aggregate (and concurrency gate).
pub mod level {
    use nebula::sea_orm;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "inventory_stock_levels")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub item_id: Uuid,
        #[sea_orm(primary_key, auto_increment = false)]
        pub warehouse_id: Uuid,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub on_hand: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub reserved: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub on_order: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))")]
        pub value: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))", nullable)]
        pub reorder_level: Option<Decimal>,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))", nullable)]
        pub reorder_qty: Option<Decimal>,
        pub updated_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// Ledger money is booked to 2 decimals.
pub(crate) fn round_money(v: Decimal) -> Decimal {
    v.round_dp(2)
}

/// Unit costs carry 6 decimals to limit moving-average drift.
pub(crate) fn round_cost(v: Decimal) -> Decimal {
    v.round_dp(6)
}

/// The running average cost of a level row (zero when empty).
pub(crate) fn level_average(level: &level::Model) -> Decimal {
    if level.on_hand.is_zero() {
        Decimal::ZERO
    } else {
        round_cost(level.value / level.on_hand)
    }
}

/// A single stock effect to apply, already validated for shape.
pub enum Movement {
    /// Stock in at a known cost (purchase receipt, opening stock, positive
    /// adjustment, transfer-in at the issued cost).
    Receipt { qty: Decimal, unit_cost: Decimal },
    /// Stock out at the running average (issue, transfer-out, negative
    /// adjustment).
    Issue {
        qty: Decimal,
        /// How much of this issue is covered by a reservation the caller
        /// holds (a sales delivery consuming its order's reservation).
        /// The availability check becomes
        /// `on_hand − (reserved − covered) ≥ qty`, so a plain issue
        /// (covered = 0) can no longer silently take stock promised to a
        /// confirmed order, while a delivery spends exactly what it
        /// reserved. The engine decrements `reserved` by `covered` in
        /// the same level update.
        covered_by_reservation: Decimal,
    },
}

pub struct StockService;

impl StockService {
    /// Apply one movement of `item` in `warehouse_id`, appending the
    /// ledger row and updating the level, all on the caller's transaction.
    /// Returns the ledger row (an issue's `unit_cost` is the average the
    /// stock went out at — transfers feed it to their receipt half).
    /// `batch_id` stamps the lot dimension on the ledger row; costing is
    /// untouched by it (moving average stays per item × warehouse).
    pub async fn apply(
        txn: &DatabaseTransaction,
        move_id: Uuid,
        move_line_id: Uuid,
        entry_date: chrono::NaiveDate,
        item: &item::Model,
        stock_uom: &uom::Model,
        warehouse_id: Uuid,
        batch_id: Option<Uuid>,
        mv: Movement,
    ) -> Result<ledger::Model> {
        if ItemType::parse(&item.item_type)? != ItemType::Stockable {
            return Err(Error::Validation(format!(
                "item {} is not stockable and cannot move through the ledger",
                item.sku
            )));
        }

        // 1. Lock (or create) the level row for item x warehouse.
        let level = lock_or_init_level(txn, item.id, warehouse_id).await?;

        // 2. Compute the delta under moving-average rules.
        let (qty_delta, unit_cost, value_delta, reserved_after) = match mv {
            Movement::Receipt { qty, unit_cost } => {
                ensure_valid_qty(qty, item, stock_uom)?;
                if unit_cost < Decimal::ZERO {
                    return Err(Error::Validation("unit cost must not be negative".into()));
                }
                let unit_cost = round_cost(unit_cost);
                let value = round_money(qty * unit_cost);
                (qty, unit_cost, value, level.reserved)
            }
            Movement::Issue {
                qty,
                covered_by_reservation: covered,
            } => {
                ensure_valid_qty(qty, item, stock_uom)?;
                if covered < Decimal::ZERO || covered > qty {
                    return Err(Error::internal(format!(
                        "reservation coverage {covered} outside 0..={qty} for {}",
                        item.sku
                    )));
                }
                if covered > level.reserved {
                    return Err(Error::internal(format!(
                        "issue of {} claims {covered} reserved but only {} is held",
                        item.sku, level.reserved
                    )));
                }
                // Free stock = on hand minus what other documents hold.
                // An issue may spend its own reservation plus free stock,
                // but never stock promised elsewhere.
                let held_by_others = level.reserved - covered;
                if level.on_hand - held_by_others < qty {
                    return Err(Error::Validation(format!(
                        "insufficient stock of {}: on hand {}, reserved for others {}, requested {}",
                        item.sku, level.on_hand, held_by_others, qty
                    )));
                }
                let avg = if level.on_hand.is_zero() {
                    Decimal::ZERO
                } else {
                    round_cost(level.value / level.on_hand)
                };
                // Emptying the location flushes the full remaining value so
                // zero quantity always means exactly zero value.
                let value = if level.on_hand == qty {
                    level.value
                } else {
                    round_money(qty * avg)
                };
                (-qty, avg, -value, level.reserved - covered)
            }
        };

        let qty_after = level.on_hand + qty_delta;
        let value_after = level.value + value_delta;

        // 3. Append the ledger row (immutable from birth). seq is assigned
        //    by Postgres (GENERATED BY DEFAULT AS IDENTITY).
        let row = ledger::ActiveModel {
            id: Set(Uuid::new_v4()),
            seq: NotSet,
            move_id: Set(move_id),
            move_line_id: Set(move_line_id),
            item_id: Set(item.id),
            warehouse_id: Set(warehouse_id),
            batch_id: Set(batch_id),
            entry_date: Set(entry_date),
            qty_delta: Set(qty_delta),
            qty_after: Set(qty_after),
            unit_cost: Set(unit_cost),
            value_delta: Set(value_delta),
            value_after: Set(value_after),
            posted_at: Set(chrono::Utc::now()),
        }
        .insert(txn)
        .await?;

        // 4. Update the locked level (an issue that consumed reservation
        //    releases it in the same write).
        let mut active: level::ActiveModel = level.into();
        active.on_hand = Set(qty_after);
        active.value = Set(value_after);
        active.reserved = Set(reserved_after);
        active.updated_at = Set(chrono::Utc::now());
        active.update(txn).await?;

        Ok(row)
    }

    /// Reserve up to `want` of an item in a warehouse for a demand
    /// document (a confirmed sales order line), granting what free stock
    /// allows: `granted = min(want, on_hand − reserved)`, floored at
    /// zero. Computed and written under the level row lock, so the grant
    /// can never promise the same stock twice; the caller records the
    /// granted quantity on its own line and the shortfall stays visible.
    pub async fn reserve_up_to(
        txn: &DatabaseTransaction,
        item_id: Uuid,
        warehouse_id: Uuid,
        want: Decimal,
    ) -> Result<Decimal> {
        if want <= Decimal::ZERO {
            return Ok(Decimal::ZERO);
        }
        let level = lock_or_init_level(txn, item_id, warehouse_id).await?;
        let free = (level.on_hand - level.reserved).max(Decimal::ZERO);
        let granted = want.min(free);
        if granted.is_zero() {
            return Ok(Decimal::ZERO);
        }
        let next = level.reserved + granted;
        let mut active: level::ActiveModel = level.into();
        active.reserved = Set(next);
        active.updated_at = Set(chrono::Utc::now());
        active.update(txn).await?;
        Ok(granted)
    }

    /// Release `qty` of reservation on a level row (order cancellation,
    /// short-close, or a delivery reversal returning less than was
    /// held). Clamped at zero: releasing more than is held is a caller
    /// bookkeeping slip that must not wedge the level, but it is logged
    /// by the clamp being visible in the returned value.
    pub async fn release_reserved(
        txn: &DatabaseTransaction,
        item_id: Uuid,
        warehouse_id: Uuid,
        qty: Decimal,
    ) -> Result<Decimal> {
        if qty <= Decimal::ZERO {
            return Ok(Decimal::ZERO);
        }
        let level = lock_or_init_level(txn, item_id, warehouse_id).await?;
        let released = qty.min(level.reserved);
        if released.is_zero() {
            return Ok(Decimal::ZERO);
        }
        let next = level.reserved - released;
        let mut active: level::ActiveModel = level.into();
        active.reserved = Set(next);
        active.updated_at = Set(chrono::Utc::now());
        active.update(txn).await?;
        Ok(released)
    }

    /// Adjust the open-purchase-order quantity on a level row (procurement
    /// maintains it at PO approve / receive / cancel / close). Locks the
    /// row like any other level mutation; the result is floored at zero
    /// rather than erroring, since `on_order` is advisory.
    pub async fn adjust_on_order(
        txn: &DatabaseTransaction,
        item_id: Uuid,
        warehouse_id: Uuid,
        delta: Decimal,
    ) -> Result<()> {
        let level = lock_or_init_level(txn, item_id, warehouse_id).await?;
        let next = (level.on_order + delta).max(Decimal::ZERO);
        let mut active: level::ActiveModel = level.into();
        active.on_order = Set(next);
        active.updated_at = Set(chrono::Utc::now());
        active.update(txn).await?;
        Ok(())
    }
}

/// Lock the level row FOR UPDATE, inserting a zero row first if the pair
/// has never moved. The bare INSERT .. ON CONFLICT DO NOTHING is raw SQL:
/// SeaORM's on_conflict().do_nothing() reports the conflict as an error
/// instead of moving on. Concurrent first-touch is safe — the second
/// insert waits on the first, then both queue on the row lock.
pub(crate) async fn lock_or_init_level(
    txn: &DatabaseTransaction,
    item_id: Uuid,
    warehouse_id: Uuid,
) -> Result<level::Model> {
    txn.execute(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "INSERT INTO inventory_stock_levels (item_id, warehouse_id) VALUES ($1, $2) \
         ON CONFLICT DO NOTHING",
        [item_id.into(), warehouse_id.into()],
    ))
    .await?;
    level::Entity::find_by_id((item_id, warehouse_id))
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(|| Error::internal("stock level row vanished under its lock"))
}

/// Quantities are positive, and whole numbers when the item's stock UoM
/// does not allow fractions.
fn ensure_valid_qty(qty: Decimal, item: &item::Model, stock_uom: &uom::Model) -> Result<()> {
    if qty <= Decimal::ZERO {
        return Err(Error::Validation(format!(
            "quantity of {} must be positive",
            item.sku
        )));
    }
    if !stock_uom.fractional && qty.normalize().scale() > 0 {
        return Err(Error::Validation(format!(
            "item {} is stocked in whole {} and cannot move a fractional quantity",
            item.sku, stock_uom.code
        )));
    }
    Ok(())
}
