//! Purchase returns: previously received goods go back to the supplier.
//!
//! A return references the purchase order (like a receipt, in reverse):
//! posting locks the return, then the order, validates every line against
//! the **unbilled received balance** (`received_qty − billed_qty` — billed
//! goods must have their invoice cancelled first; the AP debit-note flow
//! belongs to accounting's payment phase), writes an outbound stock
//! movement through the engine (`source = "procurement.return:{id}"`,
//! sharing the RTS number), reopens `received_qty` and the order status,
//! and relieves GRNI on the GL — any gap between the PO-price relief and
//! the moving-average stock value books to purchase price variance.
//!
//! Tracked items name their lot (with enough in the lot at the delivery
//! warehouse) and the exact serial units going back. Reversal brings the
//! goods, the counters and the serials home, and books the mirror entry.

use crate::scm::gl;
use crate::scm::inventory::batch;
use crate::scm::inventory::item::{item, uom};
use crate::scm::inventory::moves::{MoveStatus, MoveType, doc as move_doc, line as move_line};
use crate::scm::inventory::stock::{self, Movement, StockService, ledger};
use crate::scm::procurement::order::{
    self, OrderStatus, effective_price, load_lines as load_order_lines, load_order,
    load_order_locked, order_line, recompute_status,
};
use crate::scm::procurement::permissions::names;
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

/// Where a purchase return is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReturnStatus {
    Draft,
    Posted,
    Reversed,
}

impl ReturnStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ReturnStatus::Draft => "draft",
            ReturnStatus::Posted => "posted",
            ReturnStatus::Reversed => "reversed",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "draft" => Ok(ReturnStatus::Draft),
            "posted" => Ok(ReturnStatus::Posted),
            "reversed" => Ok(ReturnStatus::Reversed),
            other => Err(Error::internal(format!("unknown return status {other:?}"))),
        }
    }
}

/// The purchase return header.
pub mod preturn {
    use nebula::sea_orm;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "procurement_returns")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub number: Option<String>,
        pub order_id: Uuid,
        pub return_date: Date,
        pub reason: Option<String>,
        pub reference: Option<String>,
        pub carrier: Option<String>,
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

/// One return line, always against an order line.
pub mod return_line {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "procurement_return_lines")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub return_id: Uuid,
        pub order_line_id: Uuid,
        pub line_no: i32,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub qty: Decimal,
        pub batch_no: Option<String>,
        pub batch_id: Option<Uuid>,
        #[sea_orm(column_type = "JsonBinary", nullable)]
        pub serial_nos: Option<Json>,
        pub reason: Option<String>,
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

/// A return line as supplied by a caller.
pub struct ReturnLineInput {
    pub order_line_id: Uuid,
    pub qty: Decimal,
    pub batch_no: Option<String>,
    pub serial_nos: Option<Vec<String>>,
    pub reason: Option<String>,
    pub memo: Option<String>,
}

/// A new draft return as supplied by a caller.
pub struct NewReturn {
    pub order_id: Uuid,
    pub return_date: chrono::NaiveDate,
    pub reason: Option<String>,
    pub reference: Option<String>,
    pub carrier: Option<String>,
    pub memo: Option<String>,
    pub lines: Vec<ReturnLineInput>,
    pub created_by: Option<Uuid>,
}

/// The purchase return service over one (tenant) connection.
pub struct ReturnService {
    db: DatabaseConnection,
}

impl ReturnService {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Create a draft return against an order with unbilled received stock.
    pub async fn create_draft(&self, new: NewReturn) -> Result<ReturnView> {
        validate_return(&self.db, &new).await?;
        let txn = self.db.begin().await?;
        let return_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        preturn::ActiveModel {
            id: Set(return_id),
            number: Set(None),
            order_id: Set(new.order_id),
            return_date: Set(new.return_date),
            reason: Set(clean(new.reason)),
            reference: Set(clean(new.reference)),
            carrier: Set(clean(new.carrier)),
            memo: Set(clean(new.memo)),
            status: Set(ReturnStatus::Draft.as_str().to_string()),
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
        insert_lines(&txn, return_id, &new.lines).await?;
        txn.commit().await?;
        self.view(return_id).await
    }

    /// Replace a draft's header and lines wholesale. The order is fixed.
    pub async fn update_draft(&self, id: Uuid, new: NewReturn) -> Result<ReturnView> {
        let txn = self.db.begin().await?;
        let existing = load_return_locked(&txn, id).await?;
        if ReturnStatus::parse(&existing.status)? != ReturnStatus::Draft {
            return Err(Error::Validation("only a draft return can be edited".into()));
        }
        if existing.order_id != new.order_id {
            return Err(Error::Validation(
                "a return's order cannot change; delete the draft and create a new one".into(),
            ));
        }
        validate_return(&txn, &new).await?;
        return_line::Entity::delete_many()
            .filter(return_line::Column::ReturnId.eq(id))
            .exec(&txn)
            .await?;
        insert_lines(&txn, id, &new.lines).await?;
        let mut active: preturn::ActiveModel = existing.into();
        active.return_date = Set(new.return_date);
        active.reason = Set(clean(new.reason));
        active.reference = Set(clean(new.reference));
        active.carrier = Set(clean(new.carrier));
        active.memo = Set(clean(new.memo));
        active.updated_at = Set(chrono::Utc::now());
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Delete a draft (lines cascade).
    pub async fn delete_draft(&self, id: Uuid) -> Result<ReturnView> {
        let view = self.view(id).await?;
        let txn = self.db.begin().await?;
        let existing = load_return_locked(&txn, id).await?;
        if ReturnStatus::parse(&existing.status)? != ReturnStatus::Draft {
            return Err(Error::Validation("only a draft return can be deleted".into()));
        }
        preturn::Entity::delete_by_id(id).exec(&txn).await?;
        txn.commit().await?;
        Ok(view)
    }

    /// Post a draft return: stock out, order counters reopened, GRNI
    /// relieved — one transaction, locks in the global order (documents,
    /// then levels ascending, then the number last).
    pub async fn post(&self, id: Uuid, numbering: &Numbering, gl: &gl::Gl) -> Result<ReturnView> {
        let txn = self.db.begin().await?;
        let return_row = load_return_locked(&txn, id).await?;
        if ReturnStatus::parse(&return_row.status)? != ReturnStatus::Draft {
            return Err(Error::Validation("only a draft return can be posted".into()));
        }
        let order_row = load_order_locked(&txn, return_row.order_id).await?;
        let order_status = OrderStatus::parse(&order_row.status)?;
        if matches!(
            order_status,
            OrderStatus::Draft | OrderStatus::Submitted | OrderStatus::Cancelled
        ) {
            return Err(Error::Validation(format!(
                "purchase order {} is {} and has nothing to return",
                order_row.number.as_deref().unwrap_or("?"),
                order_status.as_str()
            )));
        }

        let lines = load_return_lines(&txn, id).await?;
        if lines.is_empty() {
            return Err(Error::Validation("a return needs at least one line".into()));
        }
        let order_lines: HashMap<Uuid, order_line::Model> =
            load_order_lines(&txn, order_row.id)
                .await?
                .into_iter()
                .map(|l| (l.id, l))
                .collect();

        // Only the unbilled received balance can go back, accumulated per
        // order line so two return lines cannot slip past together.
        let mut returning: HashMap<Uuid, Decimal> = HashMap::new();
        for line in &lines {
            let ol = order_lines.get(&line.order_line_id).ok_or_else(|| {
                Error::Validation(format!(
                    "line {} does not belong to this order",
                    line.line_no
                ))
            })?;
            let already = returning.entry(ol.id).or_default();
            *already += line.qty;
            if *already > ol.received_qty - ol.billed_qty {
                return Err(Error::Validation(format!(
                    "line {}: returning {} exceeds the {} received and not billed — \
                     cancel the invoice before returning billed goods",
                    line.line_no,
                    line.qty,
                    ol.received_qty - ol.billed_qty
                )));
            }
        }

        let (items, uoms) = load_items_for(&txn, order_lines.values()).await?;
        let warehouse_id = order_row.deliver_to_warehouse_id;
        let rate = order_row.exchange_rate;

        // The outbound stock movement, in this same transaction.
        let move_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        move_doc::ActiveModel {
            id: Set(move_id),
            number: Set(None),
            move_type: Set(MoveType::Issue.as_str().to_string()),
            entry_date: Set(return_row.return_date),
            memo: Set(format!(
                "Return to supplier against {}",
                order_row.number.as_deref().unwrap_or("purchase order")
            )),
            reference: Set(return_row.reference.clone()),
            from_warehouse_id: Set(Some(warehouse_id)),
            to_warehouse_id: Set(None),
            status: Set(MoveStatus::Posted.as_str().to_string()),
            source: Set(Some(format!("procurement.return:{id}"))),
            reverses_id: Set(None),
            reversed_by_id: Set(None),
            posted_at: Set(Some(now)),
            created_at: Set(now),
            created_by: Set(return_row.created_by),
        }
        .insert(&txn)
        .await?;

        let mut gates: Vec<(Uuid, Uuid)> = lines
            .iter()
            .filter_map(|l| order_lines.get(&l.order_line_id))
            .map(|ol| (ol.item_id, warehouse_id))
            .collect();
        gates.sort();
        gates.dedup();
        for (item_id, wh) in &gates {
            stock::lock_or_init_level(&txn, *item_id, *wh).await?;
        }

        // The GRNI relief: the same per-unit value the receipt accrued.
        let mut grni_relief = Decimal::ZERO;

        for line in &lines {
            let ol = &order_lines[&line.order_line_id];
            let item = &items[&ol.item_id];
            let stock_uom = uoms.get(&item.uom_id).ok_or_else(|| {
                Error::internal(format!("stock uom missing for item {}", item.sku))
            })?;

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
                    let in_lot =
                        batch::batch_on_hand(&txn, item.id, warehouse_id, b.id).await?;
                    if in_lot < line.qty {
                        return Err(Error::Validation(format!(
                            "line {}: batch {} of {} holds {} here, cannot return {}",
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
                        "line {}: item {} tracks batches; name the lot going back",
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
                return_row.return_date,
                item,
                stock_uom,
                warehouse_id,
                batch_id,
                Movement::Issue {
                    qty: line.qty,
                    covered_by_reservation: Decimal::ZERO,
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
                let mut active: return_line::ActiveModel = line.clone().into();
                active.batch_id = Set(batch_id);
                active.update(&txn).await?;
            }

            grni_relief += stock::round_money(
                line.qty * stock::round_cost(effective_price(ol.unit_price, ol.discount_pct) * rate),
            );
        }

        // Reopen the order: received back down, open demand back up while
        // the order still expects deliveries, status refreshed.
        for (ol_id, returned) in &returning {
            let ol = order_lines[ol_id].clone();
            let item_id = ol.item_id;
            let base = ol.received_qty;
            let mut active: order_line::ActiveModel = ol.into();
            active.received_qty = Set(base - returned);
            active.update(&txn).await?;
            if matches!(
                order_status,
                OrderStatus::Approved | OrderStatus::PartiallyReceived | OrderStatus::Received
            ) {
                StockService::adjust_on_order(&txn, item_id, warehouse_id, *returned).await?;
            }
        }
        recompute_status(&txn, load_order_locked(&txn, order_row.id).await?).await?;

        // Number both papers from the RTS series, freeze the return.
        let number = numbering.next(&txn, crate::scm::RETURN_SERIES).await?;
        let mut mv: move_doc::ActiveModel = move_doc::Entity::find_by_id(move_id)
            .one(&txn)
            .await?
            .ok_or_else(|| Error::internal("movement vanished inside its transaction"))?
            .into();
        mv.number = Set(Some(number.formatted.clone()));
        mv.update(&txn).await?;
        let return_date = return_row.return_date;
        let mut active: preturn::ActiveModel = return_row.into();
        active.status = Set(ReturnStatus::Posted.as_str().to_string());
        active.number = Set(Some(number.formatted));
        active.move_id = Set(Some(move_id));
        active.posted_at = Set(Some(now));
        active.updated_at = Set(now);
        active.update(&txn).await?;

        let request = gl::purchase_return_request(
            &txn,
            id,
            move_id,
            order_row.number.as_deref(),
            return_date,
            grni_relief,
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

    /// Reverse a posted return: the goods, the counters and the serial
    /// units come home, and the GL books the mirror.
    pub async fn reverse(
        &self,
        id: Uuid,
        reason: &str,
        numbering: &Numbering,
        by: Option<Uuid>,
        gl: &gl::Gl,
    ) -> Result<ReturnView> {
        let txn = self.db.begin().await?;
        let original = load_return_locked(&txn, id).await?;
        match ReturnStatus::parse(&original.status)? {
            ReturnStatus::Posted => {}
            ReturnStatus::Draft => {
                return Err(Error::Validation(
                    "a draft return has not been posted and cannot be reversed".into(),
                ));
            }
            ReturnStatus::Reversed => {
                return Err(Error::Validation("return is already reversed".into()));
            }
        }
        let order_row = load_order_locked(&txn, original.order_id).await?;
        let lines = load_return_lines(&txn, id).await?;
        let order_lines: HashMap<Uuid, order_line::Model> =
            load_order_lines(&txn, order_row.id)
                .await?
                .into_iter()
                .map(|l| (l.id, l))
                .collect();
        let mut returning: HashMap<Uuid, Decimal> = HashMap::new();
        for line in &lines {
            *returning.entry(line.order_line_id).or_default() += line.qty;
        }

        let original_move_id = original
            .move_id
            .ok_or_else(|| Error::internal("posted return without a stock movement"))?;
        let original_move = move_doc::Entity::find_by_id(original_move_id)
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or_else(|| Error::internal("return's stock movement is missing"))?;
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
                original.number.as_deref().unwrap_or("purchase return")
            )
        } else {
            format!(
                "Reversal of {}: {}",
                original.number.as_deref().unwrap_or("purchase return"),
                reason.trim()
            )
        };

        preturn::ActiveModel {
            id: Set(reversal_id),
            number: Set(None),
            order_id: Set(original.order_id),
            return_date: Set(now.date_naive()),
            reason: Set(Some(memo.clone())),
            reference: Set(original.number.clone()),
            carrier: Set(None),
            memo: Set(None),
            status: Set(ReturnStatus::Posted.as_str().to_string()),
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
            move_type: Set(MoveType::Issue.as_str().to_string()),
            entry_date: Set(now.date_naive()),
            memo: Set(memo),
            reference: Set(original_move.number.clone()),
            from_warehouse_id: Set(original_move.from_warehouse_id),
            to_warehouse_id: Set(None),
            status: Set(MoveStatus::Posted.as_str().to_string()),
            source: Set(Some(format!("procurement.return:{reversal_id}"))),
            reverses_id: Set(Some(original_move.id)),
            reversed_by_id: Set(None),
            posted_at: Set(Some(now)),
            created_at: Set(now),
            created_by: Set(by),
        }
        .insert(&txn)
        .await?;

        // Copy the lines and mirror each ledger row back in.
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
            return_line::ActiveModel {
                id: Set(Uuid::new_v4()),
                return_id: Set(reversal_id),
                order_line_id: Set(line.order_line_id),
                line_no: Set(line.line_no),
                qty: Set(line.qty),
                batch_no: Set(line.batch_no.clone()),
                batch_id: Set(line.batch_id),
                serial_nos: Set(line.serial_nos.clone()),
                reason: Set(None),
                memo: Set(line.memo.clone()),
                created_at: Set(now),
            }
            .insert(&txn)
            .await?;
        }

        let mut grni_reaccrual = Decimal::ZERO;
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
        // Serial units the return sent away come back in.
        for ml in &original_move_lines {
            let names = batch::serial_names_of_line(&txn, ml.id).await?;
            if names.is_empty() {
                continue;
            }
            let item = items.get(&ml.item_id).ok_or_else(|| {
                Error::internal(format!("item {} missing for reversal", ml.item_id))
            })?;
            batch::serials_in(
                &txn,
                item,
                mirror_line_ids[&ml.id],
                order_row.deliver_to_warehouse_id,
                ml.batch_id,
                &names,
                now.date_naive(),
                by,
            )
            .await?;
        }

        // The counters walk back: received up, open demand back down.
        let order_status = OrderStatus::parse(&order_row.status)?;
        let rate = order_row.exchange_rate;
        for (ol_id, qty) in &returning {
            let ol = order_lines[ol_id].clone();
            grni_reaccrual += stock::round_money(
                *qty * stock::round_cost(effective_price(ol.unit_price, ol.discount_pct) * rate),
            );
            let base = ol.received_qty;
            let item_id = ol.item_id;
            let mut active: order_line::ActiveModel = ol.into();
            active.received_qty = Set(base + qty);
            active.update(&txn).await?;
            if matches!(
                order_status,
                OrderStatus::Approved | OrderStatus::PartiallyReceived | OrderStatus::Received
            ) {
                StockService::adjust_on_order(
                    &txn,
                    item_id,
                    order_row.deliver_to_warehouse_id,
                    -*qty,
                )
                .await?;
            }
        }
        recompute_status(&txn, load_order_locked(&txn, order_row.id).await?).await?;

        // Number and link everything.
        let number = numbering.next(&txn, crate::scm::RETURN_SERIES).await?;
        let mut mv: move_doc::ActiveModel = move_doc::Entity::find_by_id(reversal_move_id)
            .one(&txn)
            .await?
            .ok_or_else(|| Error::internal("movement vanished inside its transaction"))?
            .into();
        mv.number = Set(Some(number.formatted.clone()));
        mv.update(&txn).await?;
        let mut rev: preturn::ActiveModel = preturn::Entity::find_by_id(reversal_id)
            .one(&txn)
            .await?
            .ok_or_else(|| Error::internal("reversal vanished inside its transaction"))?
            .into();
        rev.number = Set(Some(number.formatted));
        rev.update(&txn).await?;

        let mut orig_mv: move_doc::ActiveModel = original_move.into();
        orig_mv.status = Set(MoveStatus::Reversed.as_str().to_string());
        orig_mv.reversed_by_id = Set(Some(reversal_move_id));
        orig_mv.update(&txn).await?;

        let mut active: preturn::ActiveModel = original.into();
        active.status = Set(ReturnStatus::Reversed.as_str().to_string());
        active.reversed_by_id = Set(Some(reversal_id));
        active.updated_at = Set(now);
        active.update(&txn).await?;

        let request = gl::purchase_return_request(
            &txn,
            reversal_id,
            reversal_move_id,
            order_row.number.as_deref(),
            now.date_naive(),
            grni_reaccrual,
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

    pub async fn list(&self, filter: ReturnFilter) -> Result<Vec<ReturnHeader>> {
        let mut query = preturn::Entity::find();
        if let Some(order_id) = filter.order_id {
            query = query.filter(preturn::Column::OrderId.eq(order_id));
        }
        if let Some(s) = filter.status {
            query = query.filter(preturn::Column::Status.eq(s.as_str()));
        }
        if let Some(from) = filter.from {
            query = query.filter(preturn::Column::ReturnDate.gte(from));
        }
        if let Some(to) = filter.to {
            query = query.filter(preturn::Column::ReturnDate.lte(to));
        }
        let rows = query
            .order_by_desc(preturn::Column::ReturnDate)
            .order_by_desc(preturn::Column::CreatedAt)
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
                Ok(ReturnHeader {
                    id: r.id,
                    number: r.number.clone(),
                    order_id: r.order_id,
                    order_number: orders.get(&r.order_id).and_then(|o| o.number.clone()),
                    return_date: r.return_date,
                    reason: r.reason.clone(),
                    status: ReturnStatus::parse(&r.status)?,
                })
            })
            .collect()
    }

    /// Load a full return with lines and labels.
    pub async fn view(&self, id: Uuid) -> Result<ReturnView> {
        let row = load_return(&self.db, id).await?;
        let lines = load_return_lines(&self.db, id).await?;
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

        let line_views = lines
            .into_iter()
            .map(|l| {
                let ol = order_lines.get(&l.order_line_id);
                let item = ol.and_then(|ol| items.get(&ol.item_id));
                let serial_nos = line_serial_names(&l).unwrap_or_default();
                ReturnLineView {
                    id: l.id,
                    line_no: l.line_no,
                    order_line_id: l.order_line_id,
                    item_id: ol.map(|ol| ol.item_id),
                    sku: item.map(|i| i.sku.clone()).unwrap_or_default(),
                    item_name: item.map(|i| i.name.clone()).unwrap_or_default(),
                    qty: l.qty,
                    batch_no: l.batch_no,
                    serial_nos,
                    reason: l.reason,
                    memo: l.memo,
                }
            })
            .collect();

        Ok(ReturnView {
            id: row.id,
            number: row.number,
            order_id: row.order_id,
            order_number: order_row.number,
            return_date: row.return_date,
            reason: row.reason,
            reference: row.reference,
            carrier: row.carrier,
            memo: row.memo,
            status: ReturnStatus::parse(&row.status)?,
            move_id: row.move_id,
            reverses_id: row.reverses_id,
            reversed_by_id: row.reversed_by_id,
            posted_at: row.posted_at,
            created_at: row.created_at,
            lines: line_views,
        })
    }
}

fn clean(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// Draft-time validation: the order is real, every line points at one of
/// its lines, quantities within the unbilled received balance (re-checked
/// at post under the order lock).
async fn validate_return<C: ConnectionTrait>(conn: &C, new: &NewReturn) -> Result<()> {
    if new.lines.is_empty() {
        return Err(Error::Validation("a return needs at least one line".into()));
    }
    let order_row = load_order(conn, new.order_id).await?;
    let order_lines: HashMap<Uuid, order_line::Model> = load_order_lines(conn, new.order_id)
        .await?
        .into_iter()
        .map(|l| (l.id, l))
        .collect();
    let mut returning: HashMap<Uuid, Decimal> = HashMap::new();
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
        let already = returning.entry(ol.id).or_default();
        *already += l.qty;
        if *already > ol.received_qty - ol.billed_qty {
            return Err(Error::Validation(format!(
                "line {line_no}: returning {} exceeds the {} received and not billed on {}",
                l.qty,
                ol.received_qty - ol.billed_qty,
                order_row.number.as_deref().unwrap_or("the order")
            )));
        }
    }
    Ok(())
}

async fn insert_lines<C: ConnectionTrait>(
    conn: &C,
    return_id: Uuid,
    lines: &[ReturnLineInput],
) -> Result<()> {
    let now = chrono::Utc::now();
    for (i, l) in lines.iter().enumerate() {
        return_line::ActiveModel {
            id: Set(Uuid::new_v4()),
            return_id: Set(return_id),
            order_line_id: Set(l.order_line_id),
            line_no: Set((i + 1) as i32),
            qty: Set(l.qty),
            batch_no: Set(l.batch_no.clone().filter(|b| !b.trim().is_empty())),
            batch_id: Set(None),
            serial_nos: Set(serials_to_json(l.serial_nos.as_deref())),
            reason: Set(l.reason.clone().filter(|r| !r.trim().is_empty())),
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

/// The serial names stored on a return line (empty when none).
fn line_serial_names(l: &return_line::Model) -> Result<Vec<String>> {
    match &l.serial_nos {
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|e| Error::internal(format!("unreadable serial list on a line: {e}"))),
        None => Ok(Vec::new()),
    }
}

async fn load_return<C: ConnectionTrait>(conn: &C, id: Uuid) -> Result<preturn::Model> {
    preturn::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("purchase return {id}")))
}

async fn load_return_locked(txn: &DatabaseTransaction, id: Uuid) -> Result<preturn::Model> {
    preturn::Entity::find_by_id(id)
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("purchase return {id}")))
}

async fn load_return_lines<C: ConnectionTrait>(
    conn: &C,
    return_id: Uuid,
) -> Result<Vec<return_line::Model>> {
    return_line::Entity::find()
        .filter(return_line::Column::ReturnId.eq(return_id))
        .order_by_asc(return_line::Column::LineNo)
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

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ReturnLineView {
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
    pub reason: Option<String>,
    pub memo: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ReturnView {
    pub id: Uuid,
    pub number: Option<String>,
    pub order_id: Uuid,
    pub order_number: Option<String>,
    #[schema(value_type = String, format = Date)]
    pub return_date: chrono::NaiveDate,
    pub reason: Option<String>,
    /// Supplier RMA / collection note.
    pub reference: Option<String>,
    pub carrier: Option<String>,
    pub memo: Option<String>,
    pub status: ReturnStatus,
    /// The outbound stock movement this return produced at post.
    pub move_id: Option<Uuid>,
    pub reverses_id: Option<Uuid>,
    pub reversed_by_id: Option<Uuid>,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub posted_at: Option<chrono::DateTime<chrono::Utc>>,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub lines: Vec<ReturnLineView>,
}

/// A row of the returns register.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ReturnHeader {
    pub id: Uuid,
    pub number: Option<String>,
    pub order_id: Uuid,
    pub order_number: Option<String>,
    #[schema(value_type = String, format = Date)]
    pub return_date: chrono::NaiveDate,
    pub reason: Option<String>,
    pub status: ReturnStatus,
}

pub struct ReturnFilter {
    pub order_id: Option<Uuid>,
    pub status: Option<ReturnStatus>,
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ReturnLineRequest {
    pub order_line_id: Uuid,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    /// Required when the item tracks batches (the lot going back).
    pub batch_no: Option<String>,
    /// Required when the item tracks serials (the exact units going back).
    pub serial_nos: Option<Vec<String>>,
    pub reason: Option<String>,
    pub memo: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateReturnRequest {
    pub order_id: Uuid,
    #[schema(value_type = String, format = Date)]
    pub return_date: chrono::NaiveDate,
    pub reason: Option<String>,
    /// Supplier RMA / collection note.
    pub reference: Option<String>,
    pub carrier: Option<String>,
    pub memo: Option<String>,
    pub lines: Vec<ReturnLineRequest>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ReverseReturnRequest {
    #[serde(default)]
    pub reason: String,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListReturnsQuery {
    pub order_id: Option<Uuid>,
    pub status: Option<ReturnStatus>,
    /// Return date range, inclusive.
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

fn new_return(req: CreateReturnRequest, created_by: Option<Uuid>) -> NewReturn {
    NewReturn {
        order_id: req.order_id,
        return_date: req.return_date,
        reason: req.reason,
        reference: req.reference,
        carrier: req.carrier,
        memo: req.memo,
        lines: req
            .lines
            .into_iter()
            .map(|l| ReturnLineInput {
                order_line_id: l.order_line_id,
                qty: l.qty,
                batch_no: l.batch_no,
                serial_nos: l.serial_nos,
                reason: l.reason,
                memo: l.memo,
            })
            .collect(),
        created_by,
    }
}

pub(crate) fn routes() -> Router {
    Router::new()
        .route(
            "/procurement/returns",
            get(list_returns).post(create_return),
        )
        .route(
            "/procurement/returns/{id}",
            get(get_return).put(update_return).delete(delete_return),
        )
        .route("/procurement/returns/{id}/post", post(post_return))
        .route("/procurement/returns/{id}/reverse", post(reverse_return))
        .route("/procurement/orders/{id}/returns", get(order_returns))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    list_returns,
    get_return,
    create_return,
    update_return,
    delete_return,
    post_return,
    reverse_return,
    order_returns
))]
struct ApiDoc;

#[utoipa::path(get, path = "/procurement/returns", tag = "procurement",
    params(ListReturnsQuery),
    responses((status = 200, body = Vec<ReturnHeader>)))]
async fn list_returns(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<ListReturnsQuery>,
) -> Result<Json<Vec<ReturnHeader>>> {
    authz.require(names::RETURNS_VIEW).await?;
    ReturnService::new(db)
        .list(ReturnFilter {
            order_id: q.order_id,
            status: q.status,
            from: q.from,
            to: q.to,
        })
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/procurement/orders/{id}/returns", tag = "procurement",
    params(("id" = Uuid, Path, description = "Order id")),
    responses((status = 200, body = Vec<ReturnHeader>)))]
async fn order_returns(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<ReturnHeader>>> {
    authz.require(names::RETURNS_VIEW).await?;
    ReturnService::new(db)
        .list(ReturnFilter {
            order_id: Some(id),
            status: None,
            from: None,
            to: None,
        })
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/procurement/returns/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "Return id")),
    responses((status = 200, body = ReturnView)))]
async fn get_return(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<ReturnView>> {
    authz.require(names::RETURNS_VIEW).await?;
    ReturnService::new(db).view(id).await.map(Json)
}

#[utoipa::path(post, path = "/procurement/returns", tag = "procurement",
    request_body = CreateReturnRequest,
    responses((status = 200, body = ReturnView)))]
async fn create_return(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(req): Json<CreateReturnRequest>,
) -> Result<Json<ReturnView>> {
    authz.require(names::RETURNS_CREATE).await?;
    let view = ReturnService::new(db)
        .create_draft(new_return(req, Some(authz.user.id)))
        .await?;
    audit.0.created("scm.return", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(put, path = "/procurement/returns/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "Return id")),
    request_body = CreateReturnRequest,
    responses((status = 200, body = ReturnView)))]
async fn update_return(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<CreateReturnRequest>,
) -> Result<Json<ReturnView>> {
    authz.require(names::RETURNS_CREATE).await?;
    let service = ReturnService::new(db);
    let before = service.view(id).await?;
    let after = service.update_draft(id, new_return(req, None)).await?;
    audit.0.updated("scm.return", id, &before, &after).await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/procurement/returns/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "Return id")),
    responses((status = 200, body = ReturnView)))]
async fn delete_return(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<ReturnView>> {
    authz.require(names::RETURNS_CREATE).await?;
    let view = ReturnService::new(db).delete_draft(id).await?;
    audit.0.deleted("scm.return", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/returns/{id}/post", tag = "procurement",
    params(("id" = Uuid, Path, description = "Return id")),
    responses((status = 200, body = ReturnView)))]
async fn post_return(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    CurrentTenant(tenant): CurrentTenant,
    Extension(numbering): Extension<Numbering>,
    Extension(events): Extension<Events>,
    Path(id): Path<Uuid>,
) -> Result<Json<ReturnView>> {
    authz.require(names::RETURNS_POST).await?;
    let gl = gl::Gl::new(events, tenant.map(|t| t.id));
    let view = ReturnService::new(db).post(id, &numbering, &gl).await?;
    audit
        .0
        .event(format!(
            "posted purchase return {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/returns/{id}/reverse", tag = "procurement",
    params(("id" = Uuid, Path, description = "Return id")),
    request_body = ReverseReturnRequest,
    responses((status = 200, body = ReturnView)))]
async fn reverse_return(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    CurrentTenant(tenant): CurrentTenant,
    Extension(numbering): Extension<Numbering>,
    Extension(events): Extension<Events>,
    Path(id): Path<Uuid>,
    Json(req): Json<ReverseReturnRequest>,
) -> Result<Json<ReturnView>> {
    authz.require(names::RETURNS_REVERSE).await?;
    let gl = gl::Gl::new(events, tenant.map(|t| t.id));
    let view = ReturnService::new(db)
        .reverse(id, &req.reason, &numbering, Some(authz.user.id), &gl)
        .await?;
    audit
        .0
        .event(format!(
            "reversed purchase return with {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}
