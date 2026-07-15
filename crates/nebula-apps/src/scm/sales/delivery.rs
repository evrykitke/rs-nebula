//! Delivery notes: where sold goods leave stock.
//!
//! Posting is the transactional heart of the outbound cycle, the mirror of
//! procurement's goods receipt — one database transaction locks the
//! delivery, then the sales order (the serialization point against sibling
//! deliveries), validates every line against the order's undelivered
//! balance, writes a posted issue movement through the engine (`source =
//! "sales.delivery:{id}"`, sharing the DN- number), consumes the line's
//! reservation first and takes any remainder from free stock, bumps
//! `delivered_qty`, releases the covered reservation and recomputes the
//! order status. COGS (Dr COGS / Cr Inventory) rides on the issue
//! movement's own ledger value, staged in the same transaction and
//! published after commit.
//!
//! Tracked items name their lot (with enough in that lot at the fulfilment
//! warehouse) and the exact serial units shipped. Reversal mirrors the
//! stock back in at the issue costs, restores `delivered_qty`, re-reserves
//! what returns to the still-open order, and is blocked once the delivered
//! quantities have been billed (cancel the invoice first).

use crate::scm::gl;
use crate::scm::inventory::batch;
use crate::scm::inventory::item::{item, uom};
use crate::scm::inventory::moves::{MoveStatus, MoveType, doc as move_doc, line as move_line};
use crate::scm::inventory::stock::{self, Movement, StockService, ledger};
use crate::scm::sales::customer::customer;
use crate::scm::sales::order::{
    self, OrderStatus, load_lines as load_order_lines, load_order, load_order_locked, order_line,
    recompute_status,
};
use crate::scm::sales::permissions::names;
use axum::extract::{Path, Query};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use nebula::audit::Audit;
use nebula::auth::Authz;
use nebula::error::{Error, Result};
use nebula::{CurrentTenant, Events, Numbering, TenantDb, sea_orm};
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, QueryOrder, QuerySelect, Set,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Where a delivery note is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Draft,
    Posted,
    Reversed,
}

impl DeliveryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryStatus::Draft => "draft",
            DeliveryStatus::Posted => "posted",
            DeliveryStatus::Reversed => "reversed",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "draft" => Ok(DeliveryStatus::Draft),
            "posted" => Ok(DeliveryStatus::Posted),
            "reversed" => Ok(DeliveryStatus::Reversed),
            other => Err(Error::internal(format!("unknown delivery status {other:?}"))),
        }
    }
}

/// The delivery note header.
pub mod delivery {
    use nebula::sea_orm;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "sales_deliveries")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub number: Option<String>,
        pub order_id: Uuid,
        pub delivery_date: Date,
        pub carrier: Option<String>,
        pub tracking_no: Option<String>,
        pub vehicle_reg: Option<String>,
        pub driver_name: Option<String>,
        pub dispatched_by: Option<Uuid>,
        pub received_by_name: Option<String>,
        pub shipping_address: Option<String>,
        pub memo: Option<String>,
        pub status: String,
        pub move_id: Option<Uuid>,
        pub reverses_id: Option<Uuid>,
        pub reversed_by_id: Option<Uuid>,
        pub posted_at: Option<DateTimeUtc>,
        pub created_at: DateTimeUtc,
        pub created_by: Option<Uuid>,
        pub updated_at: DateTimeUtc,
        pub updated_by: Option<Uuid>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// One delivery line, always against a sales order line.
pub mod delivery_line {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "sales_delivery_lines")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub delivery_id: Uuid,
        pub order_line_id: Uuid,
        pub line_no: i32,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub qty: Decimal,
        pub batch_no: Option<String>,
        pub batch_id: Option<Uuid>,
        #[sea_orm(column_type = "JsonBinary", nullable)]
        pub serial_nos: Option<Json>,
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

/// A delivery line as supplied by a caller. Batch and serial names are
/// required exactly when the item tracks them.
pub struct DeliveryLineInput {
    pub order_line_id: Uuid,
    pub qty: Decimal,
    pub batch_no: Option<String>,
    pub serial_nos: Option<Vec<String>>,
    pub memo: Option<String>,
}

/// A new draft delivery as supplied by a caller.
pub struct NewDelivery {
    pub order_id: Uuid,
    pub delivery_date: chrono::NaiveDate,
    pub carrier: Option<String>,
    pub tracking_no: Option<String>,
    pub vehicle_reg: Option<String>,
    pub driver_name: Option<String>,
    pub received_by_name: Option<String>,
    pub shipping_address: Option<String>,
    pub memo: Option<String>,
    pub lines: Vec<DeliveryLineInput>,
    pub created_by: Option<Uuid>,
}

/// The delivery service over one (tenant) connection.
pub struct DeliveryService {
    db: DatabaseConnection,
}

impl DeliveryService {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Create a draft delivery against an order that can still ship.
    pub async fn create_draft(&self, new: NewDelivery) -> Result<DeliveryView> {
        validate_delivery(&self.db, &new).await?;
        let txn = self.db.begin().await?;
        let delivery_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        delivery::ActiveModel {
            id: Set(delivery_id),
            number: Set(None),
            order_id: Set(new.order_id),
            delivery_date: Set(new.delivery_date),
            carrier: Set(clean(new.carrier)),
            tracking_no: Set(clean(new.tracking_no)),
            vehicle_reg: Set(clean(new.vehicle_reg)),
            driver_name: Set(clean(new.driver_name)),
            dispatched_by: Set(new.created_by),
            received_by_name: Set(clean(new.received_by_name)),
            shipping_address: Set(clean(new.shipping_address)),
            memo: Set(clean(new.memo)),
            status: Set(DeliveryStatus::Draft.as_str().to_string()),
            move_id: Set(None),
            reverses_id: Set(None),
            reversed_by_id: Set(None),
            posted_at: Set(None),
            created_at: Set(now),
            created_by: Set(new.created_by),
            updated_at: Set(now),
            updated_by: Set(None),
        }
        .insert(&txn)
        .await?;
        insert_lines(&txn, delivery_id, &new.lines).await?;
        txn.commit().await?;
        self.view(delivery_id).await
    }

    /// Replace a draft's header and lines wholesale. The order is fixed.
    pub async fn update_draft(&self, id: Uuid, new: NewDelivery) -> Result<DeliveryView> {
        let txn = self.db.begin().await?;
        let existing = load_delivery_locked(&txn, id).await?;
        if DeliveryStatus::parse(&existing.status)? != DeliveryStatus::Draft {
            return Err(Error::Validation("only a draft delivery can be edited".into()));
        }
        if existing.order_id != new.order_id {
            return Err(Error::Validation(
                "a delivery's order cannot change; delete the draft and create a new one".into(),
            ));
        }
        validate_delivery(&txn, &new).await?;
        delivery_line::Entity::delete_many()
            .filter(delivery_line::Column::DeliveryId.eq(id))
            .exec(&txn)
            .await?;
        insert_lines(&txn, id, &new.lines).await?;
        let mut active: delivery::ActiveModel = existing.into();
        active.delivery_date = Set(new.delivery_date);
        active.carrier = Set(clean(new.carrier));
        active.tracking_no = Set(clean(new.tracking_no));
        active.vehicle_reg = Set(clean(new.vehicle_reg));
        active.driver_name = Set(clean(new.driver_name));
        active.received_by_name = Set(clean(new.received_by_name));
        active.shipping_address = Set(clean(new.shipping_address));
        active.memo = Set(clean(new.memo));
        active.updated_at = Set(chrono::Utc::now());
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Delete a draft (lines cascade).
    pub async fn delete_draft(&self, id: Uuid) -> Result<DeliveryView> {
        let view = self.view(id).await?;
        let txn = self.db.begin().await?;
        let existing = load_delivery_locked(&txn, id).await?;
        if DeliveryStatus::parse(&existing.status)? != DeliveryStatus::Draft {
            return Err(Error::Validation("only a draft delivery can be deleted".into()));
        }
        delivery::Entity::delete_by_id(id).exec(&txn).await?;
        txn.commit().await?;
        Ok(view)
    }

    /// Post a draft delivery: stock out consuming the order's reservation,
    /// order counters up, COGS booked — one transaction, locks in the
    /// global order (documents, then levels ascending, then the number
    /// last).
    pub async fn post(&self, id: Uuid, numbering: &Numbering, gl: &gl::Gl) -> Result<DeliveryView> {
        let txn = self.db.begin().await?;
        let delivery_row = load_delivery_locked(&txn, id).await?;
        if DeliveryStatus::parse(&delivery_row.status)? != DeliveryStatus::Draft {
            return Err(Error::Validation("only a draft delivery can be posted".into()));
        }
        let order_row = load_order_locked(&txn, delivery_row.order_id).await?;
        let order_status = OrderStatus::parse(&order_row.status)?;
        if !order_status.deliverable() {
            return Err(Error::Validation(format!(
                "sales order {} is {} and cannot ship goods",
                order_row.number.as_deref().unwrap_or("?"),
                order_status.as_str()
            )));
        }

        let lines = load_delivery_lines(&txn, id).await?;
        if lines.is_empty() {
            return Err(Error::Validation("a delivery needs at least one line".into()));
        }
        let order_lines: HashMap<Uuid, order_line::Model> = load_order_lines(&txn, order_row.id)
            .await?
            .into_iter()
            .map(|l| (l.id, l))
            .collect();

        // 1. Validate against the order's undelivered balance, accumulating
        //    per order line so two delivery lines cannot slip past together.
        let mut delivering: HashMap<Uuid, Decimal> = HashMap::new();
        for line in &lines {
            let ol = order_lines.get(&line.order_line_id).ok_or_else(|| {
                Error::Validation(format!(
                    "line {} does not belong to this order",
                    line.line_no
                ))
            })?;
            let already = delivering.entry(ol.id).or_default();
            *already += line.qty;
            if ol.delivered_qty + *already > ol.qty {
                return Err(Error::Validation(format!(
                    "line {}: delivering {} exceeds the {} still open on the order",
                    line.line_no,
                    line.qty,
                    ol.qty - ol.delivered_qty
                )));
            }
        }

        let (items, uoms) = load_items_for(&txn, order_lines.values()).await?;

        // 2. The outbound stock movement, in this same transaction. Level
        //    rows are pre-locked ascending; the number comes last and is
        //    stamped on both documents.
        let move_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        move_doc::ActiveModel {
            id: Set(move_id),
            number: Set(None),
            move_type: Set(MoveType::Issue.as_str().to_string()),
            entry_date: Set(delivery_row.delivery_date),
            memo: Set(format!(
                "Delivery against {}",
                order_row.number.as_deref().unwrap_or("sales order")
            )),
            reference: Set(delivery_row.tracking_no.clone()),
            from_warehouse_id: Set(Some(order_row.warehouse_id)),
            to_warehouse_id: Set(None),
            status: Set(MoveStatus::Posted.as_str().to_string()),
            source: Set(Some(format!("sales.delivery:{id}"))),
            reverses_id: Set(None),
            reversed_by_id: Set(None),
            posted_at: Set(Some(now)),
            created_at: Set(now),
            created_by: Set(delivery_row.created_by),
        }
        .insert(&txn)
        .await?;

        let mut gates: Vec<(Uuid, Uuid)> = lines
            .iter()
            .filter_map(|l| order_lines.get(&l.order_line_id))
            .map(|ol| (ol.item_id, ol.warehouse_id.unwrap_or(order_row.warehouse_id)))
            .collect();
        gates.sort();
        gates.dedup();
        for (item_id, wh) in &gates {
            stock::lock_or_init_level(&txn, *item_id, *wh).await?;
        }

        // Remaining reservation per order line, spent covered-first across
        // however many delivery lines target the same order line.
        let mut reserved_left: HashMap<Uuid, Decimal> = order_lines
            .values()
            .map(|ol| (ol.id, ol.reserved_qty))
            .collect();

        for line in &lines {
            let ol = &order_lines[&line.order_line_id];
            let item = &items[&ol.item_id];
            let stock_uom = uoms.get(&item.uom_id).ok_or_else(|| {
                Error::internal(format!("stock uom missing for item {}", item.sku))
            })?;
            let warehouse_id = ol.warehouse_id.unwrap_or(order_row.warehouse_id);

            // Tracking dimensions: a tracked item must name its lot (with
            // enough in the lot here) and the exact serial units shipped.
            let serial_names = line_serial_names(line)?;
            if !item.track_serials && !serial_names.is_empty() {
                return Err(Error::Validation(format!(
                    "line {}: item {} does not track serial numbers",
                    line.line_no, item.sku
                )));
            }
            if !item.track_batches && line.batch_no.is_some() {
                return Err(Error::Validation(format!(
                    "line {}: item {} does not track batches",
                    line.line_no, item.sku
                )));
            }
            let batch_id = match (&line.batch_no, item.track_batches) {
                (Some(no), _) => {
                    let b = batch::find_batch(&txn, item, no).await?;
                    let in_lot = batch::batch_on_hand(&txn, item.id, warehouse_id, b.id).await?;
                    if in_lot < line.qty {
                        return Err(Error::Validation(format!(
                            "line {}: batch {} of {} holds {} here, cannot deliver {}",
                            line.line_no,
                            b.batch_no,
                            item.sku,
                            in_lot.normalize(),
                            line.qty.normalize()
                        )));
                    }
                    Some(b.id)
                }
                (None, true) => {
                    return Err(Error::Validation(format!(
                        "line {}: item {} tracks batches; name the lot being shipped",
                        line.line_no, item.sku
                    )));
                }
                (None, false) => None,
            };
            let names = if item.track_serials {
                batch::check_serial_names(item, line.qty, &serial_names)?
            } else {
                Vec::new()
            };

            // Spend this line's share of the order line's reservation first;
            // the engine takes the rest from free stock (and rejects the
            // issue if free stock is short).
            let left = reserved_left.entry(ol.id).or_default();
            let covered = (*left).min(line.qty);
            *left -= covered;

            let ml = move_line::ActiveModel {
                id: Set(Uuid::new_v4()),
                move_id: Set(move_id),
                line_no: Set(line.line_no),
                item_id: Set(ol.item_id),
                qty: Set(line.qty),
                entered_uom_id: Set(None),
                unit_cost: Set(None),
                batch_no: Set(line.batch_no.clone()),
                batch_id: Set(batch_id),
                serial_nos: Set(line.serial_nos.clone()),
                memo: Set(line.memo.clone()),
                created_at: Set(now),
            }
            .insert(&txn)
            .await?;
            StockService::apply(
                &txn,
                move_id,
                ml.id,
                delivery_row.delivery_date,
                item,
                stock_uom,
                warehouse_id,
                batch_id,
                Movement::Issue {
                    qty: line.qty,
                    covered_by_reservation: covered,
                },
            )
            .await?;
            if !names.is_empty() {
                batch::serials_out(
                    &txn,
                    item,
                    ml.id,
                    warehouse_id,
                    &names,
                    batch::SerialStatus::Issued,
                )
                .await?;
            }
            if batch_id != line.batch_id {
                let mut active: delivery_line::ActiveModel = line.clone().into();
                active.batch_id = Set(batch_id);
                active.update(&txn).await?;
            }
        }

        // 3. Order counters: delivered up, reservation down to what is
        //    left, status refreshed.
        for (ol_id, delivered) in &delivering {
            let ol = order_lines[ol_id].clone();
            let base = ol.delivered_qty;
            let mut active: order_line::ActiveModel = ol.into();
            active.delivered_qty = Set(base + delivered);
            active.reserved_qty = Set(reserved_left[ol_id]);
            active.update(&txn).await?;
        }
        recompute_status(&txn, load_order_locked(&txn, order_row.id).await?).await?;

        // 4. Number both papers from the DN series, freeze the delivery.
        let number = numbering.next(&txn, crate::scm::SALES_DELIVERY_SERIES).await?;
        let mut mv: move_doc::ActiveModel = move_doc::Entity::find_by_id(move_id)
            .one(&txn)
            .await?
            .ok_or_else(|| Error::internal("movement vanished inside its transaction"))?
            .into();
        mv.number = Set(Some(number.formatted.clone()));
        mv.update(&txn).await?;

        let delivery_date = delivery_row.delivery_date;
        let order_number = order_row.number.clone();
        let mut active: delivery::ActiveModel = delivery_row.into();
        active.status = Set(DeliveryStatus::Posted.as_str().to_string());
        active.number = Set(Some(number.formatted.clone()));
        active.move_id = Set(Some(move_id));
        active.posted_at = Set(Some(now));
        active.updated_at = Set(now);
        active.update(&txn).await?;

        // 5. COGS rides on the issue movement's ledger value.
        let request = gl::cogs_move_request(
            &txn,
            format!("sales.delivery:{id}:post"),
            move_id,
            format!(
                "Delivery {} against {}",
                number.formatted,
                order_number.as_deref().unwrap_or("sales order")
            ),
            delivery_date,
            gl.tenant_id(),
        )
        .await?;
        if let Some(req) = &request {
            gl::stage(&txn, req).await?;
        }
        txn.commit().await?;
        if let Some(req) = request {
            gl.publish(req).await;
        }
        self.view(id).await
    }

    /// Reverse a posted delivery: the goods, the counters and the serial
    /// units come home, what returns re-reserves onto the still-open order,
    /// and the GL books the mirror (Dr Inventory / Cr COGS). Blocked once
    /// the delivered quantities have been billed.
    pub async fn reverse(
        &self,
        id: Uuid,
        reason: &str,
        numbering: &Numbering,
        by: Option<Uuid>,
        gl: &gl::Gl,
    ) -> Result<DeliveryView> {
        let txn = self.db.begin().await?;
        let original = load_delivery_locked(&txn, id).await?;
        match DeliveryStatus::parse(&original.status)? {
            DeliveryStatus::Posted => {}
            DeliveryStatus::Draft => {
                return Err(Error::Validation(
                    "a draft delivery has not been posted and cannot be reversed".into(),
                ));
            }
            DeliveryStatus::Reversed => {
                return Err(Error::Validation("delivery is already reversed".into()));
            }
        }
        let order_row = load_order_locked(&txn, original.order_id).await?;
        let lines = load_delivery_lines(&txn, id).await?;
        let order_lines: HashMap<Uuid, order_line::Model> = load_order_lines(&txn, order_row.id)
            .await?
            .into_iter()
            .map(|l| (l.id, l))
            .collect();

        // Billing is an outer commitment: billed goods cannot be silently
        // un-delivered. Cancel the invoice first.
        let mut delivering: HashMap<Uuid, Decimal> = HashMap::new();
        for line in &lines {
            *delivering.entry(line.order_line_id).or_default() += line.qty;
        }
        for (ol_id, qty) in &delivering {
            let ol = order_lines
                .get(ol_id)
                .ok_or_else(|| Error::internal("delivery line lost its order line"))?;
            if ol.billed_qty > ol.delivered_qty - qty {
                return Err(Error::Validation(format!(
                    "order line {} has been billed for {}; cancel the invoice before reversing",
                    ol.line_no, ol.billed_qty
                )));
            }
        }

        let original_move_id = original
            .move_id
            .ok_or_else(|| Error::internal("posted delivery without a stock movement"))?;
        let original_move = move_doc::Entity::find_by_id(original_move_id)
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or_else(|| Error::internal("delivery's stock movement is missing"))?;
        let rows = ledger::Entity::find()
            .filter(ledger::Column::MoveId.eq(original_move_id))
            .order_by_asc(ledger::Column::Seq)
            .all(&txn)
            .await?;
        let (items, uoms) = load_items_for(&txn, order_lines.values()).await?;

        let mut gates: Vec<(Uuid, Uuid)> =
            rows.iter().map(|r| (r.item_id, r.warehouse_id)).collect();
        gates.sort();
        gates.dedup();
        for (item_id, wh) in &gates {
            stock::lock_or_init_level(&txn, *item_id, *wh).await?;
        }

        let now = chrono::Utc::now();
        let reversal_id = Uuid::new_v4();
        let reversal_move_id = Uuid::new_v4();
        let memo = if reason.trim().is_empty() {
            format!(
                "Reversal of {}",
                original.number.as_deref().unwrap_or("delivery")
            )
        } else {
            format!(
                "Reversal of {}: {}",
                original.number.as_deref().unwrap_or("delivery"),
                reason.trim()
            )
        };

        delivery::ActiveModel {
            id: Set(reversal_id),
            number: Set(None),
            order_id: Set(original.order_id),
            delivery_date: Set(now.date_naive()),
            carrier: Set(None),
            tracking_no: Set(None),
            vehicle_reg: Set(None),
            driver_name: Set(None),
            dispatched_by: Set(by),
            received_by_name: Set(None),
            shipping_address: Set(None),
            memo: Set(Some(memo.clone())),
            status: Set(DeliveryStatus::Posted.as_str().to_string()),
            move_id: Set(Some(reversal_move_id)),
            reverses_id: Set(Some(original.id)),
            reversed_by_id: Set(None),
            posted_at: Set(Some(now)),
            created_at: Set(now),
            created_by: Set(by),
            updated_at: Set(now),
            updated_by: Set(None),
        }
        .insert(&txn)
        .await?;
        move_doc::ActiveModel {
            id: Set(reversal_move_id),
            number: Set(None),
            move_type: Set(MoveType::Receipt.as_str().to_string()),
            entry_date: Set(now.date_naive()),
            memo: Set(memo),
            reference: Set(original_move.number.clone()),
            from_warehouse_id: Set(None),
            to_warehouse_id: Set(original_move.from_warehouse_id),
            status: Set(MoveStatus::Posted.as_str().to_string()),
            source: Set(Some(format!("sales.delivery:{reversal_id}"))),
            reverses_id: Set(Some(original_move.id)),
            reversed_by_id: Set(None),
            posted_at: Set(Some(now)),
            created_at: Set(now),
            created_by: Set(by),
        }
        .insert(&txn)
        .await?;

        // Copy the lines and mirror each ledger row back in at its issue
        // cost.
        let original_move_lines = move_line::Entity::find()
            .filter(move_line::Column::MoveId.eq(original_move_id))
            .order_by_asc(move_line::Column::LineNo)
            .all(&txn)
            .await?;
        let mut mirror_line_ids: HashMap<Uuid, Uuid> = HashMap::new();
        for ml in &original_move_lines {
            let new_id = Uuid::new_v4();
            mirror_line_ids.insert(ml.id, new_id);
            move_line::ActiveModel {
                id: Set(new_id),
                move_id: Set(reversal_move_id),
                line_no: Set(ml.line_no),
                item_id: Set(ml.item_id),
                qty: Set(ml.qty),
                entered_uom_id: Set(ml.entered_uom_id),
                unit_cost: Set(ml.unit_cost),
                batch_no: Set(ml.batch_no.clone()),
                batch_id: Set(ml.batch_id),
                serial_nos: Set(ml.serial_nos.clone()),
                memo: Set(ml.memo.clone()),
                created_at: Set(now),
            }
            .insert(&txn)
            .await?;
        }
        for line in &lines {
            delivery_line::ActiveModel {
                id: Set(Uuid::new_v4()),
                delivery_id: Set(reversal_id),
                order_line_id: Set(line.order_line_id),
                line_no: Set(line.line_no),
                qty: Set(line.qty),
                batch_no: Set(line.batch_no.clone()),
                batch_id: Set(line.batch_id),
                serial_nos: Set(line.serial_nos.clone()),
                memo: Set(line.memo.clone()),
                created_at: Set(now),
            }
            .insert(&txn)
            .await?;
        }
        for row in &rows {
            let item = items.get(&row.item_id).ok_or_else(|| {
                Error::internal(format!("item {} missing for reversal", row.item_id))
            })?;
            let stock_uom = uoms.get(&item.uom_id).ok_or_else(|| {
                Error::internal(format!("stock uom missing for item {}", item.sku))
            })?;
            let mirror_line = *mirror_line_ids
                .get(&row.move_line_id)
                .ok_or_else(|| Error::internal("ledger row without a document line"))?;
            // An issue carries a negative qty_delta; the reversal receives
            // that quantity back in at the very cost it left.
            StockService::apply(
                &txn,
                reversal_move_id,
                mirror_line,
                now.date_naive(),
                item,
                stock_uom,
                row.warehouse_id,
                row.batch_id,
                Movement::Receipt {
                    qty: -row.qty_delta,
                    unit_cost: row.unit_cost,
                },
            )
            .await?;
        }
        // Serial units the delivery shipped come home.
        for ml in &original_move_lines {
            let names = batch::serial_names_of_line(&txn, ml.id).await?;
            if names.is_empty() {
                continue;
            }
            let item = items.get(&ml.item_id).ok_or_else(|| {
                Error::internal(format!("item {} missing for reversal", ml.item_id))
            })?;
            let ol = order_lines.get(&find_order_line_for(&lines, ml.line_no));
            let warehouse_id = ol
                .and_then(|ol| ol.warehouse_id)
                .unwrap_or(order_row.warehouse_id);
            batch::serials_in(
                &txn,
                item,
                mirror_line_ids[&ml.id],
                warehouse_id,
                ml.batch_id,
                &names,
                now.date_naive(),
                by,
            )
            .await?;
        }

        // The counters walk back: delivered down, and what returns to a
        // still-open order re-reserves so it stays committed.
        let order_status = OrderStatus::parse(&order_row.status)?;
        let reopen = order_status.deliverable()
            || matches!(order_status, OrderStatus::Delivered);
        for (ol_id, qty) in &delivering {
            let ol = order_lines[ol_id].clone();
            let item_id = ol.item_id;
            let warehouse_id = ol.warehouse_id.unwrap_or(order_row.warehouse_id);
            let base_delivered = ol.delivered_qty;
            let base_reserved = ol.reserved_qty;
            let mut active: order_line::ActiveModel = ol.into();
            active.delivered_qty = Set(base_delivered - qty);
            if reopen {
                let granted =
                    StockService::reserve_up_to(&txn, item_id, warehouse_id, *qty).await?;
                active.reserved_qty = Set(base_reserved + granted);
            }
            active.update(&txn).await?;
        }
        recompute_status(&txn, load_order_locked(&txn, order_row.id).await?).await?;

        // Number and link everything.
        let number = numbering.next(&txn, crate::scm::SALES_DELIVERY_SERIES).await?;
        let mut mv: move_doc::ActiveModel = move_doc::Entity::find_by_id(reversal_move_id)
            .one(&txn)
            .await?
            .ok_or_else(|| Error::internal("movement vanished inside its transaction"))?
            .into();
        mv.number = Set(Some(number.formatted.clone()));
        mv.update(&txn).await?;
        let mut rev: delivery::ActiveModel = delivery::Entity::find_by_id(reversal_id)
            .one(&txn)
            .await?
            .ok_or_else(|| Error::internal("reversal vanished inside its transaction"))?
            .into();
        rev.number = Set(Some(number.formatted.clone()));
        rev.update(&txn).await?;

        let mut orig_mv: move_doc::ActiveModel = original_move.into();
        orig_mv.status = Set(MoveStatus::Reversed.as_str().to_string());
        orig_mv.reversed_by_id = Set(Some(reversal_move_id));
        orig_mv.update(&txn).await?;

        let order_number = order_row.number.clone();
        let mut active: delivery::ActiveModel = original.into();
        active.status = Set(DeliveryStatus::Reversed.as_str().to_string());
        active.reversed_by_id = Set(Some(reversal_id));
        active.updated_at = Set(now);
        active.update(&txn).await?;

        // The reversal movement's ledger rows carry the mirrored signs, so
        // the same builder yields Dr Inventory / Cr COGS.
        let request = gl::cogs_move_request(
            &txn,
            format!("sales.delivery:{reversal_id}:post"),
            reversal_move_id,
            format!(
                "Reversal of delivery against {}",
                order_number.as_deref().unwrap_or("sales order")
            ),
            now.date_naive(),
            gl.tenant_id(),
        )
        .await?;
        if let Some(req) = &request {
            gl::stage(&txn, req).await?;
        }
        txn.commit().await?;
        if let Some(req) = request {
            gl.publish(req).await;
        }
        self.view(reversal_id).await
    }

    pub async fn list(&self, filter: DeliveryFilter) -> Result<Vec<DeliveryHeader>> {
        let mut query = delivery::Entity::find();
        if let Some(order_id) = filter.order_id {
            query = query.filter(delivery::Column::OrderId.eq(order_id));
        }
        if let Some(s) = filter.status {
            query = query.filter(delivery::Column::Status.eq(s.as_str()));
        }
        if let Some(from) = filter.from {
            query = query.filter(delivery::Column::DeliveryDate.gte(from));
        }
        if let Some(to) = filter.to {
            query = query.filter(delivery::Column::DeliveryDate.lte(to));
        }
        let rows = query
            .order_by_desc(delivery::Column::DeliveryDate)
            .order_by_desc(delivery::Column::CreatedAt)
            .all(&self.db)
            .await?;
        let order_ids: Vec<Uuid> = rows.iter().map(|r| r.order_id).collect();
        let orders: HashMap<Uuid, order::order::Model> = order::order::Entity::find()
            .filter(order::order::Column::Id.is_in(order_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|o| (o.id, o))
            .collect();
        rows.into_iter()
            .map(|r| {
                Ok(DeliveryHeader {
                    id: r.id,
                    number: r.number.clone(),
                    order_id: r.order_id,
                    order_number: orders.get(&r.order_id).and_then(|o| o.number.clone()),
                    delivery_date: r.delivery_date,
                    carrier: r.carrier.clone(),
                    status: DeliveryStatus::parse(&r.status)?,
                })
            })
            .collect()
    }

    /// Load a full delivery with lines and labels.
    pub async fn view(&self, id: Uuid) -> Result<DeliveryView> {
        let row = load_delivery(&self.db, id).await?;
        let lines = load_delivery_lines(&self.db, id).await?;
        let order_row = load_order(&self.db, row.order_id).await?;
        let order_lines: HashMap<Uuid, order_line::Model> =
            load_order_lines(&self.db, row.order_id)
                .await?
                .into_iter()
                .map(|l| (l.id, l))
                .collect();
        let items: HashMap<Uuid, item::Model> = item::Entity::find()
            .filter(
                item::Column::Id.is_in(
                    order_lines
                        .values()
                        .map(|l| l.item_id)
                        .collect::<Vec<Uuid>>(),
                ),
            )
            .all(&self.db)
            .await?
            .into_iter()
            .map(|i| (i.id, i))
            .collect();
        let customer = customer::Entity::find_by_id(order_row.customer_id)
            .one(&self.db)
            .await?;

        let line_views = lines
            .into_iter()
            .map(|l| {
                let ol = order_lines.get(&l.order_line_id);
                let item = ol.and_then(|ol| items.get(&ol.item_id));
                let serial_nos = line_serial_names(&l).unwrap_or_default();
                DeliveryLineView {
                    id: l.id,
                    line_no: l.line_no,
                    order_line_id: l.order_line_id,
                    item_id: ol.map(|ol| ol.item_id),
                    sku: item.map(|i| i.sku.clone()).unwrap_or_default(),
                    item_name: item.map(|i| i.name.clone()).unwrap_or_default(),
                    qty: l.qty,
                    batch_no: l.batch_no,
                    serial_nos,
                    memo: l.memo,
                }
            })
            .collect();

        Ok(DeliveryView {
            id: row.id,
            number: row.number,
            order_id: row.order_id,
            order_number: order_row.number,
            customer_id: order_row.customer_id,
            customer_name: customer.map(|c| c.name).unwrap_or_default(),
            delivery_date: row.delivery_date,
            carrier: row.carrier,
            tracking_no: row.tracking_no,
            vehicle_reg: row.vehicle_reg,
            driver_name: row.driver_name,
            received_by_name: row.received_by_name,
            shipping_address: row.shipping_address,
            memo: row.memo,
            status: DeliveryStatus::parse(&row.status)?,
            move_id: row.move_id,
            reverses_id: row.reverses_id,
            reversed_by_id: row.reversed_by_id,
            posted_at: row.posted_at,
            created_at: row.created_at,
            lines: line_views,
        })
    }
}

/// The effective warehouse for a serial-reversal line: locate the delivery
/// line by its movement line number to find its order line. Cheap over the
/// small in-memory `lines` slice.
fn find_order_line_for(lines: &[delivery_line::Model], line_no: i32) -> Uuid {
    lines
        .iter()
        .find(|l| l.line_no == line_no)
        .map(|l| l.order_line_id)
        .unwrap_or_default()
}

fn clean(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// Draft-time validation: the order can still ship, every line points at
/// one of its lines, quantities within the undelivered balance (re-checked
/// at post under the order lock).
async fn validate_delivery<C: ConnectionTrait>(conn: &C, new: &NewDelivery) -> Result<()> {
    if new.lines.is_empty() {
        return Err(Error::Validation("a delivery needs at least one line".into()));
    }
    let order_row = load_order(conn, new.order_id).await?;
    let status = OrderStatus::parse(&order_row.status)?;
    if !status.deliverable() {
        return Err(Error::Validation(format!(
            "sales order {} is {} and cannot ship goods",
            order_row.number.as_deref().unwrap_or("?"),
            status.as_str()
        )));
    }
    let order_lines: HashMap<Uuid, order_line::Model> = load_order_lines(conn, new.order_id)
        .await?
        .into_iter()
        .map(|l| (l.id, l))
        .collect();
    let mut delivering: HashMap<Uuid, Decimal> = HashMap::new();
    for (i, l) in new.lines.iter().enumerate() {
        let line_no = i + 1;
        let Some(ol) = order_lines.get(&l.order_line_id) else {
            return Err(Error::Validation(format!(
                "line {line_no} does not belong to this order"
            )));
        };
        if l.qty <= Decimal::ZERO {
            return Err(Error::Validation(format!(
                "line {line_no}: quantity must be positive"
            )));
        }
        let already = delivering.entry(ol.id).or_default();
        *already += l.qty;
        if ol.delivered_qty + *already > ol.qty {
            return Err(Error::Validation(format!(
                "line {line_no}: delivering {} exceeds the {} still open on {}",
                l.qty,
                ol.qty - ol.delivered_qty,
                order_row.number.as_deref().unwrap_or("the order")
            )));
        }
    }
    Ok(())
}

async fn insert_lines<C: ConnectionTrait>(
    conn: &C,
    delivery_id: Uuid,
    lines: &[DeliveryLineInput],
) -> Result<()> {
    let now = chrono::Utc::now();
    for (i, l) in lines.iter().enumerate() {
        delivery_line::ActiveModel {
            id: Set(Uuid::new_v4()),
            delivery_id: Set(delivery_id),
            order_line_id: Set(l.order_line_id),
            line_no: Set((i + 1) as i32),
            qty: Set(l.qty),
            batch_no: Set(l.batch_no.clone().filter(|b| !b.trim().is_empty())),
            batch_id: Set(None),
            serial_nos: Set(serials_to_json(l.serial_nos.as_deref())),
            memo: Set(l.memo.clone().filter(|m| !m.trim().is_empty())),
            created_at: Set(now),
        }
        .insert(conn)
        .await?;
    }
    Ok(())
}

/// Draft serial names into their stored JSON form (`None` when empty).
fn serials_to_json(names: Option<&[String]>) -> Option<sea_orm::JsonValue> {
    let names: Vec<String> = names
        .unwrap_or_default()
        .iter()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(serde_json::json!(names))
    }
}

/// The serial names stored on a delivery line (empty when none).
fn line_serial_names(l: &delivery_line::Model) -> Result<Vec<String>> {
    match &l.serial_nos {
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|e| Error::internal(format!("unreadable serial list on a line: {e}"))),
        None => Ok(Vec::new()),
    }
}

async fn load_delivery<C: ConnectionTrait>(conn: &C, id: Uuid) -> Result<delivery::Model> {
    delivery::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("delivery {id}")))
}

async fn load_delivery_locked(txn: &DatabaseTransaction, id: Uuid) -> Result<delivery::Model> {
    delivery::Entity::find_by_id(id)
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("delivery {id}")))
}

pub(crate) async fn load_delivery_lines<C: ConnectionTrait>(
    conn: &C,
    delivery_id: Uuid,
) -> Result<Vec<delivery_line::Model>> {
    delivery_line::Entity::find()
        .filter(delivery_line::Column::DeliveryId.eq(delivery_id))
        .order_by_asc(delivery_line::Column::LineNo)
        .all(conn)
        .await
        .map_err(Error::from)
}

/// The items behind a set of order lines, plus their stock UoMs.
async fn load_items_for<'a, C, I>(
    conn: &C,
    order_lines: I,
) -> Result<(HashMap<Uuid, item::Model>, HashMap<Uuid, uom::Model>)>
where
    C: ConnectionTrait,
    I: Iterator<Item = &'a order_line::Model>,
{
    let ids: Vec<Uuid> = order_lines.map(|l| l.item_id).collect();
    let items: HashMap<Uuid, item::Model> = item::Entity::find()
        .filter(item::Column::Id.is_in(ids))
        .all(conn)
        .await?
        .into_iter()
        .map(|i| (i.id, i))
        .collect();
    let uom_ids: Vec<Uuid> = items.values().map(|i| i.uom_id).collect();
    let uoms: HashMap<Uuid, uom::Model> = uom::Entity::find()
        .filter(uom::Column::Id.is_in(uom_ids))
        .all(conn)
        .await?
        .into_iter()
        .map(|u| (u.id, u))
        .collect();
    Ok((items, uoms))
}

// ---------------------------------------------------------------------------
// Views (API DTOs)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DeliveryLineView {
    pub id: Uuid,
    pub line_no: i32,
    pub order_line_id: Uuid,
    pub item_id: Option<Uuid>,
    pub sku: String,
    pub item_name: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    pub batch_no: Option<String>,
    pub serial_nos: Vec<String>,
    pub memo: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DeliveryView {
    pub id: Uuid,
    pub number: Option<String>,
    pub order_id: Uuid,
    pub order_number: Option<String>,
    pub customer_id: Uuid,
    pub customer_name: String,
    #[schema(value_type = String, format = Date)]
    pub delivery_date: chrono::NaiveDate,
    pub carrier: Option<String>,
    pub tracking_no: Option<String>,
    pub vehicle_reg: Option<String>,
    pub driver_name: Option<String>,
    pub received_by_name: Option<String>,
    pub shipping_address: Option<String>,
    pub memo: Option<String>,
    pub status: DeliveryStatus,
    /// The outbound stock movement this delivery produced at post.
    pub move_id: Option<Uuid>,
    pub reverses_id: Option<Uuid>,
    pub reversed_by_id: Option<Uuid>,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub posted_at: Option<chrono::DateTime<chrono::Utc>>,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub lines: Vec<DeliveryLineView>,
}

/// A row of the delivery register.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DeliveryHeader {
    pub id: Uuid,
    pub number: Option<String>,
    pub order_id: Uuid,
    pub order_number: Option<String>,
    #[schema(value_type = String, format = Date)]
    pub delivery_date: chrono::NaiveDate,
    pub carrier: Option<String>,
    pub status: DeliveryStatus,
}

pub struct DeliveryFilter {
    pub order_id: Option<Uuid>,
    pub status: Option<DeliveryStatus>,
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
pub struct DeliveryLineRequest {
    pub order_line_id: Uuid,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    /// Required when the item tracks batches (the lot being shipped).
    pub batch_no: Option<String>,
    /// Required when the item tracks serials (the exact units shipped).
    pub serial_nos: Option<Vec<String>>,
    pub memo: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateDeliveryRequest {
    pub order_id: Uuid,
    #[schema(value_type = String, format = Date)]
    pub delivery_date: chrono::NaiveDate,
    pub carrier: Option<String>,
    pub tracking_no: Option<String>,
    pub vehicle_reg: Option<String>,
    pub driver_name: Option<String>,
    /// Who signed for the goods on the customer's side.
    pub received_by_name: Option<String>,
    pub shipping_address: Option<String>,
    pub memo: Option<String>,
    pub lines: Vec<DeliveryLineRequest>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ReverseDeliveryRequest {
    #[serde(default)]
    pub reason: String,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListDeliveriesQuery {
    pub order_id: Option<Uuid>,
    pub status: Option<DeliveryStatus>,
    /// Delivery date range, inclusive.
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

fn new_delivery(req: CreateDeliveryRequest, created_by: Option<Uuid>) -> NewDelivery {
    NewDelivery {
        order_id: req.order_id,
        delivery_date: req.delivery_date,
        carrier: req.carrier,
        tracking_no: req.tracking_no,
        vehicle_reg: req.vehicle_reg,
        driver_name: req.driver_name,
        received_by_name: req.received_by_name,
        shipping_address: req.shipping_address,
        memo: req.memo,
        lines: req
            .lines
            .into_iter()
            .map(|l| DeliveryLineInput {
                order_line_id: l.order_line_id,
                qty: l.qty,
                batch_no: l.batch_no,
                serial_nos: l.serial_nos,
                memo: l.memo,
            })
            .collect(),
        created_by,
    }
}

pub(crate) fn routes() -> Router {
    Router::new()
        .route(
            "/sales/deliveries",
            get(list_deliveries).post(create_delivery),
        )
        .route(
            "/sales/deliveries/{id}",
            get(get_delivery)
                .put(update_delivery)
                .delete(delete_delivery),
        )
        .route("/sales/deliveries/{id}/post", post(post_delivery))
        .route("/sales/deliveries/{id}/reverse", post(reverse_delivery))
        .route("/sales/orders/{id}/deliveries", get(order_deliveries))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    list_deliveries,
    get_delivery,
    create_delivery,
    update_delivery,
    delete_delivery,
    post_delivery,
    reverse_delivery,
    order_deliveries
))]
struct ApiDoc;

#[utoipa::path(get, path = "/sales/deliveries", tag = "sales",
    params(ListDeliveriesQuery),
    responses((status = 200, body = Vec<DeliveryHeader>)))]
async fn list_deliveries(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<ListDeliveriesQuery>,
) -> Result<Json<Vec<DeliveryHeader>>> {
    authz.require(names::DELIVERIES_VIEW).await?;
    DeliveryService::new(db)
        .list(DeliveryFilter {
            order_id: q.order_id,
            status: q.status,
            from: q.from,
            to: q.to,
        })
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/sales/orders/{id}/deliveries", tag = "sales",
    params(("id" = Uuid, Path, description = "Order id")),
    responses((status = 200, body = Vec<DeliveryHeader>)))]
async fn order_deliveries(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<DeliveryHeader>>> {
    authz.require(names::DELIVERIES_VIEW).await?;
    DeliveryService::new(db)
        .list(DeliveryFilter {
            order_id: Some(id),
            status: None,
            from: None,
            to: None,
        })
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/sales/deliveries/{id}", tag = "sales",
    params(("id" = Uuid, Path, description = "Delivery id")),
    responses((status = 200, body = DeliveryView)))]
async fn get_delivery(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<DeliveryView>> {
    authz.require(names::DELIVERIES_VIEW).await?;
    DeliveryService::new(db).view(id).await.map(Json)
}

#[utoipa::path(post, path = "/sales/deliveries", tag = "sales",
    request_body = CreateDeliveryRequest,
    responses((status = 200, body = DeliveryView)))]
async fn create_delivery(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(req): Json<CreateDeliveryRequest>,
) -> Result<Json<DeliveryView>> {
    authz.require(names::DELIVERIES_CREATE).await?;
    let view = DeliveryService::new(db)
        .create_draft(new_delivery(req, Some(authz.user.id)))
        .await?;
    audit.0.created("scm.sales_delivery", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(put, path = "/sales/deliveries/{id}", tag = "sales",
    params(("id" = Uuid, Path, description = "Delivery id")),
    request_body = CreateDeliveryRequest,
    responses((status = 200, body = DeliveryView)))]
async fn update_delivery(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<CreateDeliveryRequest>,
) -> Result<Json<DeliveryView>> {
    authz.require(names::DELIVERIES_CREATE).await?;
    let service = DeliveryService::new(db);
    let before = service.view(id).await?;
    let after = service.update_draft(id, new_delivery(req, None)).await?;
    audit
        .0
        .updated("scm.sales_delivery", id, &before, &after)
        .await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/sales/deliveries/{id}", tag = "sales",
    params(("id" = Uuid, Path, description = "Delivery id")),
    responses((status = 200, body = DeliveryView)))]
async fn delete_delivery(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<DeliveryView>> {
    authz.require(names::DELIVERIES_CREATE).await?;
    let view = DeliveryService::new(db).delete_draft(id).await?;
    audit.0.deleted("scm.sales_delivery", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/sales/deliveries/{id}/post", tag = "sales",
    params(("id" = Uuid, Path, description = "Delivery id")),
    responses((status = 200, body = DeliveryView)))]
async fn post_delivery(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    CurrentTenant(tenant): CurrentTenant,
    Extension(numbering): Extension<Numbering>,
    Extension(events): Extension<Events>,
    Path(id): Path<Uuid>,
) -> Result<Json<DeliveryView>> {
    authz.require(names::DELIVERIES_POST).await?;
    let gl = gl::Gl::new(events, tenant.map(|t| t.id));
    let view = DeliveryService::new(db).post(id, &numbering, &gl).await?;
    audit
        .0
        .event(format!(
            "posted delivery {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/sales/deliveries/{id}/reverse", tag = "sales",
    params(("id" = Uuid, Path, description = "Delivery id")),
    request_body = ReverseDeliveryRequest,
    responses((status = 200, body = DeliveryView)))]
async fn reverse_delivery(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    CurrentTenant(tenant): CurrentTenant,
    Extension(numbering): Extension<Numbering>,
    Extension(events): Extension<Events>,
    Path(id): Path<Uuid>,
    Json(req): Json<ReverseDeliveryRequest>,
) -> Result<Json<DeliveryView>> {
    authz.require(names::DELIVERIES_REVERSE).await?;
    let gl = gl::Gl::new(events, tenant.map(|t| t.id));
    let view = DeliveryService::new(db)
        .reverse(id, &req.reason, &numbering, Some(authz.user.id), &gl)
        .await?;
    audit
        .0
        .event(format!(
            "reversed delivery with {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}
