//! Goods receipts: where purchased goods become stock.
//!
//! Posting is the transactional heart of the module — one database
//! transaction locks the receipt, then the purchase order (the
//! serialization point against sibling receipts and invoices), validates
//! every line against the PO's remaining balance, writes a posted stock
//! movement through the inventory engine (`source =
//! "procurement.receipt:{id}"`, sharing the receipt's GRN number), bumps
//! `received_qty`, releases `on_order`, recomputes the order status and
//! maintains the item-supplier catalog's last-price memory. Stock is
//! costed at the PO line's effective price × the exchange rate, in base
//! currency.
//!
//! Locks follow the global order: document rows first (receipt → order),
//! then every touched stock level ascending by `(item_id, warehouse_id)`,
//! then the numbering series row last. Reversal mirrors the stock movement
//! back out (blocked if the goods have since been issued), restores the PO
//! counters, and links the pair — billed quantities must be cancelled off
//! their invoices first.

use crate::scm::inventory::item::{item, uom};
use crate::scm::inventory::moves::{MoveStatus, MoveType, doc as move_doc, line as move_line};
use crate::scm::inventory::stock::{self, Movement, StockService, ledger};
use crate::scm::procurement::order::{
    self, OrderStatus, effective_price, load_lines as load_order_lines, load_order,
    load_order_locked, order_line, recompute_status,
};
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

/// Where a goods receipt is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptStatus {
    Draft,
    Posted,
    Reversed,
}

impl ReceiptStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ReceiptStatus::Draft => "draft",
            ReceiptStatus::Posted => "posted",
            ReceiptStatus::Reversed => "reversed",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "draft" => Ok(ReceiptStatus::Draft),
            "posted" => Ok(ReceiptStatus::Posted),
            "reversed" => Ok(ReceiptStatus::Reversed),
            other => Err(Error::internal(format!("unknown receipt status {other:?}"))),
        }
    }
}

/// The goods receipt header.
pub mod receipt {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "procurement_receipts")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub number: Option<String>,
        pub order_id: Uuid,
        pub receipt_date: Date,
        pub reference: Option<String>,
        pub carrier: Option<String>,
        pub tracking_no: Option<String>,
        pub vehicle_reg: Option<String>,
        pub delivered_by: Option<String>,
        pub received_by: Option<Uuid>,
        #[sea_orm(column_type = "Decimal(Some((20, 8)))", nullable)]
        pub exchange_rate: Option<Decimal>,
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

/// One goods receipt line, always against an order line.
pub mod receipt_line {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "procurement_receipt_lines")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub receipt_id: Uuid,
        pub order_line_id: Uuid,
        pub line_no: i32,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub qty: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub rejected_qty: Decimal,
        pub reject_reason: Option<String>,
        pub batch_id: Option<Uuid>,
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

/// A receipt line as supplied by a caller. `qty` is what goes into stock;
/// rejected goods stay off the ledger and off `received_qty`.
pub struct ReceiptLineInput {
    pub order_line_id: Uuid,
    pub qty: Decimal,
    pub rejected_qty: Option<Decimal>,
    pub reject_reason: Option<String>,
    pub memo: Option<String>,
}

/// A new draft goods receipt as supplied by a caller.
pub struct NewReceipt {
    pub order_id: Uuid,
    pub receipt_date: chrono::NaiveDate,
    pub reference: Option<String>,
    pub carrier: Option<String>,
    pub tracking_no: Option<String>,
    pub vehicle_reg: Option<String>,
    pub delivered_by: Option<String>,
    pub exchange_rate: Option<Decimal>,
    pub memo: Option<String>,
    pub lines: Vec<ReceiptLineInput>,
    pub created_by: Option<Uuid>,
}

/// The goods receipt service over one (tenant) connection.
pub struct ReceiptService {
    db: DatabaseConnection,
}

impl ReceiptService {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Create a draft receipt against an order that can still receive.
    pub async fn create_draft(&self, new: NewReceipt) -> Result<ReceiptView> {
        validate_receipt(&self.db, &new).await?;
        let txn = self.db.begin().await?;
        let receipt_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        receipt::ActiveModel {
            id: Set(receipt_id),
            number: Set(None),
            order_id: Set(new.order_id),
            receipt_date: Set(new.receipt_date),
            reference: Set(clean(new.reference)),
            carrier: Set(clean(new.carrier)),
            tracking_no: Set(clean(new.tracking_no)),
            vehicle_reg: Set(clean(new.vehicle_reg)),
            delivered_by: Set(clean(new.delivered_by)),
            received_by: Set(new.created_by),
            exchange_rate: Set(new.exchange_rate),
            memo: Set(clean(new.memo)),
            status: Set(ReceiptStatus::Draft.as_str().to_string()),
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
        insert_lines(&txn, receipt_id, &new.lines).await?;
        txn.commit().await?;
        self.view(receipt_id).await
    }

    /// Replace a draft's header and lines wholesale. The order it receives
    /// against is fixed at creation.
    pub async fn update_draft(&self, id: Uuid, new: NewReceipt) -> Result<ReceiptView> {
        let txn = self.db.begin().await?;
        let existing = load_receipt_locked(&txn, id).await?;
        if ReceiptStatus::parse(&existing.status)? != ReceiptStatus::Draft {
            return Err(Error::Validation("only a draft receipt can be edited".into()));
        }
        if existing.order_id != new.order_id {
            return Err(Error::Validation(
                "a receipt's order cannot change; delete the draft and create a new one".into(),
            ));
        }
        validate_receipt(&txn, &new).await?;
        receipt_line::Entity::delete_many()
            .filter(receipt_line::Column::ReceiptId.eq(id))
            .exec(&txn)
            .await?;
        insert_lines(&txn, id, &new.lines).await?;
        let mut active: receipt::ActiveModel = existing.into();
        active.receipt_date = Set(new.receipt_date);
        active.reference = Set(clean(new.reference));
        active.carrier = Set(clean(new.carrier));
        active.tracking_no = Set(clean(new.tracking_no));
        active.vehicle_reg = Set(clean(new.vehicle_reg));
        active.delivered_by = Set(clean(new.delivered_by));
        active.exchange_rate = Set(new.exchange_rate);
        active.memo = Set(clean(new.memo));
        active.updated_at = Set(chrono::Utc::now());
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Delete a draft (lines cascade).
    pub async fn delete_draft(&self, id: Uuid) -> Result<ReceiptView> {
        let view = self.view(id).await?;
        let txn = self.db.begin().await?;
        let existing = load_receipt_locked(&txn, id).await?;
        if ReceiptStatus::parse(&existing.status)? != ReceiptStatus::Draft {
            return Err(Error::Validation("only a draft receipt can be deleted".into()));
        }
        receipt::Entity::delete_by_id(id).exec(&txn).await?;
        txn.commit().await?;
        Ok(view)
    }

    /// Post a draft receipt: stock in, PO counters up, one transaction.
    pub async fn post(&self, id: Uuid, numbering: &Numbering) -> Result<ReceiptView> {
        let txn = self.db.begin().await?;
        let receipt_row = load_receipt_locked(&txn, id).await?;
        if ReceiptStatus::parse(&receipt_row.status)? != ReceiptStatus::Draft {
            return Err(Error::Validation("only a draft receipt can be posted".into()));
        }
        let order_row = load_order_locked(&txn, receipt_row.order_id).await?;
        let order_status = OrderStatus::parse(&order_row.status)?;
        if !order_status.receivable() {
            return Err(Error::Validation(format!(
                "purchase order {} is {} and cannot receive goods",
                order_row.number.as_deref().unwrap_or("?"),
                order_status.as_str()
            )));
        }

        let lines = load_receipt_lines(&txn, id).await?;
        if lines.is_empty() {
            return Err(Error::Validation("a receipt needs at least one line".into()));
        }
        let order_lines: HashMap<Uuid, order_line::Model> =
            load_order_lines(&txn, order_row.id)
                .await?
                .into_iter()
                .map(|l| (l.id, l))
                .collect();

        // 1. Validate quantities against the PO's remaining balance,
        //    accumulating per order line so two receipt lines against the
        //    same PO line cannot slip past together.
        let mut receiving: HashMap<Uuid, Decimal> = HashMap::new();
        for line in &lines {
            let ol = order_lines.get(&line.order_line_id).ok_or_else(|| {
                Error::Validation(format!(
                    "line {} does not belong to this order",
                    line.line_no
                ))
            })?;
            let already = receiving.entry(ol.id).or_default();
            *already += line.qty;
            if ol.received_qty + *already > ol.qty {
                return Err(Error::Validation(format!(
                    "line {}: receiving {} exceeds the {} remaining on the order",
                    line.line_no,
                    line.qty,
                    ol.qty - ol.received_qty
                )));
            }
        }

        let (items, uoms) = load_items_for(&txn, order_lines.values()).await?;
        for item in items.values() {
            if !item.is_active {
                return Err(Error::Validation(format!(
                    "item {} is inactive and cannot be received",
                    item.sku
                )));
            }
        }

        let rate = receipt_row.exchange_rate.unwrap_or(order_row.exchange_rate);
        let warehouse_id = order_row.deliver_to_warehouse_id;

        // 2. The stock movement, in this same transaction. Level rows are
        //    pre-locked ascending; the number comes last per the global
        //    lock order and is stamped on both documents.
        let move_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        move_doc::ActiveModel {
            id: Set(move_id),
            number: Set(None),
            move_type: Set(MoveType::Receipt.as_str().to_string()),
            entry_date: Set(receipt_row.receipt_date),
            memo: Set(format!(
                "Goods receipt against {}",
                order_row.number.as_deref().unwrap_or("purchase order")
            )),
            reference: Set(receipt_row.reference.clone()),
            from_warehouse_id: Set(None),
            to_warehouse_id: Set(Some(warehouse_id)),
            status: Set(MoveStatus::Posted.as_str().to_string()),
            source: Set(Some(format!("procurement.receipt:{id}"))),
            reverses_id: Set(None),
            reversed_by_id: Set(None),
            posted_at: Set(Some(now)),
            created_at: Set(now),
            created_by: Set(receipt_row.created_by),
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

        for line in &lines {
            let ol = &order_lines[&line.order_line_id];
            let item = &items[&ol.item_id];
            let stock_uom = uoms.get(&item.uom_id).ok_or_else(|| {
                Error::internal(format!("stock uom missing for item {}", item.sku))
            })?;
            let unit_cost = stock::round_cost(
                effective_price(ol.unit_price, ol.discount_pct) * rate,
            );
            let ml = move_line::ActiveModel {
                id: Set(Uuid::new_v4()),
                move_id: Set(move_id),
                line_no: Set(line.line_no),
                item_id: Set(ol.item_id),
                qty: Set(line.qty),
                entered_uom_id: Set(None),
                unit_cost: Set(Some(unit_cost)),
                batch_id: Set(line.batch_id),
                memo: Set(line.memo.clone()),
                created_at: Set(now),
            }
            .insert(&txn)
            .await?;
            StockService::apply(
                &txn,
                move_id,
                ml.id,
                receipt_row.receipt_date,
                item,
                stock_uom,
                warehouse_id,
                Movement::Receipt {
                    qty: line.qty,
                    unit_cost,
                },
            )
            .await?;
        }

        // 3. PO counters: received up, open demand down, status refreshed.
        for (ol_id, received) in &receiving {
            let ol = order_lines[ol_id].clone();
            let item_id = ol.item_id;
            let base = ol.received_qty;
            let mut active: order_line::ActiveModel = ol.into();
            active.received_qty = Set(base + received);
            active.update(&txn).await?;
            StockService::adjust_on_order(&txn, item_id, warehouse_id, -*received).await?;
        }
        recompute_status(&txn, load_order_locked(&txn, order_row.id).await?).await?;

        // 4. Last-price memory: the catalog row and the item master learn
        //    what we actually paid.
        maintain_price_memory(&txn, &order_row, &order_lines, &receiving, rate, receipt_row.receipt_date).await?;

        // 5. Freeze the receipt with the shared GRN number.
        let number = numbering.next(&txn, crate::scm::RECEIPT_SERIES).await?;
        let mut mv: move_doc::ActiveModel = move_doc::Entity::find_by_id(move_id)
            .one(&txn)
            .await?
            .ok_or_else(|| Error::internal("movement vanished inside its transaction"))?
            .into();
        mv.number = Set(Some(number.formatted.clone()));
        mv.update(&txn).await?;

        let mut active: receipt::ActiveModel = receipt_row.into();
        active.status = Set(ReceiptStatus::Posted.as_str().to_string());
        active.number = Set(Some(number.formatted));
        active.move_id = Set(Some(move_id));
        active.posted_at = Set(Some(now));
        active.updated_at = Set(now);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Reverse a posted receipt: the stock goes back out (blocked when it
    /// has since been issued), the PO counters reopen, and a linked
    /// reversal receipt documents it. Billed quantities block the reversal
    /// until their invoices are cancelled.
    pub async fn reverse(
        &self,
        id: Uuid,
        reason: &str,
        numbering: &Numbering,
        by: Option<Uuid>,
    ) -> Result<ReceiptView> {
        let txn = self.db.begin().await?;
        let original = load_receipt_locked(&txn, id).await?;
        match ReceiptStatus::parse(&original.status)? {
            ReceiptStatus::Posted => {}
            ReceiptStatus::Draft => {
                return Err(Error::Validation(
                    "a draft receipt has not been posted and cannot be reversed".into(),
                ));
            }
            ReceiptStatus::Reversed => {
                return Err(Error::Validation("receipt is already reversed".into()));
            }
        }
        let order_row = load_order_locked(&txn, original.order_id).await?;
        let lines = load_receipt_lines(&txn, id).await?;
        let order_lines: HashMap<Uuid, order_line::Model> =
            load_order_lines(&txn, order_row.id)
                .await?
                .into_iter()
                .map(|l| (l.id, l))
                .collect();

        // Billing is an outer commitment: what has been billed cannot be
        // silently un-received.
        let mut reversing: HashMap<Uuid, Decimal> = HashMap::new();
        for line in &lines {
            *reversing.entry(line.order_line_id).or_default() += line.qty;
        }
        for (ol_id, qty) in &reversing {
            let ol = order_lines.get(ol_id).ok_or_else(|| {
                Error::internal("receipt line lost its order line")
            })?;
            if ol.billed_qty > ol.received_qty - qty {
                return Err(Error::Validation(format!(
                    "order line {} has been billed for {}; cancel the invoice before reversing",
                    ol.line_no, ol.billed_qty
                )));
            }
        }

        let original_move_id = original
            .move_id
            .ok_or_else(|| Error::internal("posted receipt without a stock movement"))?;
        let original_move = move_doc::Entity::find_by_id(original_move_id)
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or_else(|| Error::internal("receipt's stock movement is missing"))?;
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
                original.number.as_deref().unwrap_or("goods receipt")
            )
        } else {
            format!(
                "Reversal of {}: {}",
                original.number.as_deref().unwrap_or("goods receipt"),
                reason.trim()
            )
        };

        receipt::ActiveModel {
            id: Set(reversal_id),
            number: Set(None),
            order_id: Set(original.order_id),
            receipt_date: Set(now.date_naive()),
            reference: Set(original.number.clone()),
            carrier: Set(None),
            tracking_no: Set(None),
            vehicle_reg: Set(None),
            delivered_by: Set(None),
            received_by: Set(by),
            exchange_rate: Set(original.exchange_rate),
            memo: Set(Some(memo.clone())),
            status: Set(ReceiptStatus::Posted.as_str().to_string()),
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
            from_warehouse_id: Set(original_move.from_warehouse_id),
            to_warehouse_id: Set(original_move.to_warehouse_id),
            status: Set(MoveStatus::Posted.as_str().to_string()),
            source: Set(Some(format!("procurement.receipt:{reversal_id}"))),
            reverses_id: Set(Some(original_move.id)),
            reversed_by_id: Set(None),
            posted_at: Set(Some(now)),
            created_at: Set(now),
            created_by: Set(by),
        }
        .insert(&txn)
        .await?;

        // Copy the receipt lines and mirror each ledger row back out.
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
                batch_id: Set(ml.batch_id),
                memo: Set(ml.memo.clone()),
                created_at: Set(now),
            }
            .insert(&txn)
            .await?;
        }
        for line in &lines {
            receipt_line::ActiveModel {
                id: Set(Uuid::new_v4()),
                receipt_id: Set(reversal_id),
                order_line_id: Set(line.order_line_id),
                line_no: Set(line.line_no),
                qty: Set(line.qty),
                rejected_qty: Set(Decimal::ZERO),
                reject_reason: Set(None),
                batch_id: Set(line.batch_id),
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
            let mv = if row.qty_delta > Decimal::ZERO {
                Movement::Issue { qty: row.qty_delta }
            } else {
                Movement::Receipt {
                    qty: -row.qty_delta,
                    unit_cost: row.unit_cost,
                }
            };
            StockService::apply(
                &txn,
                reversal_move_id,
                mirror_line,
                now.date_naive(),
                item,
                stock_uom,
                row.warehouse_id,
                mv,
            )
            .await?;
        }

        // Reopen the PO: received back down, demand back up (unless the
        // order was since closed or cancelled), status refreshed.
        let order_status = OrderStatus::parse(&order_row.status)?;
        for (ol_id, qty) in &reversing {
            let ol = order_lines[ol_id].clone();
            let mut active: order_line::ActiveModel = ol.into();
            active.received_qty = Set(order_lines[ol_id].received_qty - qty);
            active.update(&txn).await?;
            if matches!(
                order_status,
                OrderStatus::Approved | OrderStatus::PartiallyReceived | OrderStatus::Received
            ) {
                StockService::adjust_on_order(
                    &txn,
                    order_lines[ol_id].item_id,
                    order_row.deliver_to_warehouse_id,
                    *qty,
                )
                .await?;
            }
        }
        recompute_status(&txn, load_order_locked(&txn, order_row.id).await?).await?;

        // Number and link everything.
        let number = numbering.next(&txn, crate::scm::RECEIPT_SERIES).await?;
        let mut mv: move_doc::ActiveModel = move_doc::Entity::find_by_id(reversal_move_id)
            .one(&txn)
            .await?
            .ok_or_else(|| Error::internal("movement vanished inside its transaction"))?
            .into();
        mv.number = Set(Some(number.formatted.clone()));
        mv.update(&txn).await?;
        let mut rev: receipt::ActiveModel = receipt::Entity::find_by_id(reversal_id)
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

        let mut active: receipt::ActiveModel = original.into();
        active.status = Set(ReceiptStatus::Reversed.as_str().to_string());
        active.reversed_by_id = Set(Some(reversal_id));
        active.updated_at = Set(now);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(reversal_id).await
    }

    pub async fn list(&self, filter: ReceiptFilter) -> Result<Vec<ReceiptHeader>> {
        let mut query = receipt::Entity::find();
        if let Some(order_id) = filter.order_id {
            query = query.filter(receipt::Column::OrderId.eq(order_id));
        }
        if let Some(s) = filter.status {
            query = query.filter(receipt::Column::Status.eq(s.as_str()));
        }
        if let Some(from) = filter.from {
            query = query.filter(receipt::Column::ReceiptDate.gte(from));
        }
        if let Some(to) = filter.to {
            query = query.filter(receipt::Column::ReceiptDate.lte(to));
        }
        let rows = query
            .order_by_desc(receipt::Column::ReceiptDate)
            .order_by_desc(receipt::Column::CreatedAt)
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
                Ok(ReceiptHeader {
                    id: r.id,
                    number: r.number.clone(),
                    order_id: r.order_id,
                    order_number: orders.get(&r.order_id).and_then(|o| o.number.clone()),
                    receipt_date: r.receipt_date,
                    reference: r.reference.clone(),
                    status: ReceiptStatus::parse(&r.status)?,
                })
            })
            .collect()
    }

    /// Load a full receipt with lines and labels.
    pub async fn view(&self, id: Uuid) -> Result<ReceiptView> {
        let row = load_receipt(&self.db, id).await?;
        let lines = load_receipt_lines(&self.db, id).await?;
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
                ReceiptLineView {
                    id: l.id,
                    line_no: l.line_no,
                    order_line_id: l.order_line_id,
                    item_id: ol.map(|ol| ol.item_id),
                    sku: item.map(|i| i.sku.clone()).unwrap_or_default(),
                    item_name: item.map(|i| i.name.clone()).unwrap_or_default(),
                    qty: l.qty,
                    rejected_qty: l.rejected_qty,
                    reject_reason: l.reject_reason,
                    memo: l.memo,
                }
            })
            .collect();

        Ok(ReceiptView {
            id: row.id,
            number: row.number,
            order_id: row.order_id,
            order_number: order_row.number,
            receipt_date: row.receipt_date,
            reference: row.reference,
            carrier: row.carrier,
            tracking_no: row.tracking_no,
            vehicle_reg: row.vehicle_reg,
            delivered_by: row.delivered_by,
            received_by: row.received_by,
            exchange_rate: row.exchange_rate,
            memo: row.memo,
            status: ReceiptStatus::parse(&row.status)?,
            move_id: row.move_id,
            reverses_id: row.reverses_id,
            reversed_by_id: row.reversed_by_id,
            posted_at: row.posted_at,
            created_at: row.created_at,
            lines: line_views,
        })
    }
}

/// After a posting, the catalog row for (item, supplier) and the item
/// master remember the price actually paid — receipts maintain this from
/// day one so later phases (price prefill, auto-reorder) have history.
async fn maintain_price_memory(
    txn: &DatabaseTransaction,
    order_row: &order::order::Model,
    order_lines: &HashMap<Uuid, order_line::Model>,
    receiving: &HashMap<Uuid, Decimal>,
    rate: Decimal,
    receipt_date: chrono::NaiveDate,
) -> Result<()> {
    let now = chrono::Utc::now();
    for ol_id in receiving.keys() {
        let ol = &order_lines[ol_id];
        let price = effective_price(ol.unit_price, ol.discount_pct);
        let base_price = stock::round_cost(price * rate);

        let existing = item_supplier::Entity::find()
            .filter(item_supplier::Column::SupplierId.eq(order_row.supplier_id))
            .filter(item_supplier::Column::ItemId.eq(ol.item_id))
            .one(txn)
            .await?;
        match existing {
            Some(row) => {
                let mut active: item_supplier::ActiveModel = row.into();
                active.last_price = Set(Some(price));
                active.last_purchased_on = Set(Some(receipt_date));
                active.updated_at = Set(now);
                active.update(txn).await?;
            }
            None => {
                item_supplier::ActiveModel {
                    id: Set(Uuid::new_v4()),
                    item_id: Set(ol.item_id),
                    supplier_id: Set(order_row.supplier_id),
                    supplier_sku: Set(None),
                    supplier_item_name: Set(None),
                    purchase_uom_id: Set(None),
                    pack_qty: Set(None),
                    last_price: Set(Some(price)),
                    last_purchased_on: Set(Some(receipt_date)),
                    lead_time_days: Set(None),
                    min_order_qty: Set(None),
                    is_preferred: Set(false),
                    is_active: Set(true),
                    notes: Set(None),
                    created_at: Set(now),
                    updated_at: Set(now),
                }
                .insert(txn)
                .await?;
            }
        }

        if let Some(item_row) = item::Entity::find_by_id(ol.item_id).one(txn).await? {
            let mut active: item::ActiveModel = item_row.into();
            active.last_purchase_price = Set(Some(base_price));
            active.updated_at = Set(now);
            active.update(txn).await?;
        }
    }
    Ok(())
}

fn clean(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// Draft-time validation: the order can still receive, every line points
/// at one of its lines, quantities are sane.
async fn validate_receipt<C: ConnectionTrait>(conn: &C, new: &NewReceipt) -> Result<()> {
    if new.lines.is_empty() {
        return Err(Error::Validation("a receipt needs at least one line".into()));
    }
    if new.exchange_rate.is_some_and(|r| r <= Decimal::ZERO) {
        return Err(Error::Validation("exchange rate must be positive".into()));
    }
    let order_row = load_order(conn, new.order_id).await?;
    let status = OrderStatus::parse(&order_row.status)?;
    if !status.receivable() {
        return Err(Error::Validation(format!(
            "purchase order {} is {} and cannot receive goods",
            order_row.number.as_deref().unwrap_or("?"),
            status.as_str()
        )));
    }
    let order_lines: HashMap<Uuid, order_line::Model> = load_order_lines(conn, new.order_id)
        .await?
        .into_iter()
        .map(|l| (l.id, l))
        .collect();
    for (i, l) in new.lines.iter().enumerate() {
        let line_no = i + 1;
        if !order_lines.contains_key(&l.order_line_id) {
            return Err(Error::Validation(format!(
                "line {line_no} does not belong to this order"
            )));
        }
        if l.qty <= Decimal::ZERO {
            return Err(Error::Validation(format!(
                "line {line_no}: quantity must be positive"
            )));
        }
        if l.rejected_qty.is_some_and(|q| q < Decimal::ZERO) {
            return Err(Error::Validation(format!(
                "line {line_no}: rejected quantity must not be negative"
            )));
        }
    }
    Ok(())
}

async fn insert_lines<C: ConnectionTrait>(
    conn: &C,
    receipt_id: Uuid,
    lines: &[ReceiptLineInput],
) -> Result<()> {
    let now = chrono::Utc::now();
    for (i, l) in lines.iter().enumerate() {
        receipt_line::ActiveModel {
            id: Set(Uuid::new_v4()),
            receipt_id: Set(receipt_id),
            order_line_id: Set(l.order_line_id),
            line_no: Set((i + 1) as i32),
            qty: Set(l.qty),
            rejected_qty: Set(l.rejected_qty.unwrap_or(Decimal::ZERO)),
            reject_reason: Set(l.reject_reason.clone().filter(|r| !r.trim().is_empty())),
            batch_id: Set(None),
            memo: Set(l.memo.clone().filter(|m| !m.trim().is_empty())),
            created_at: Set(now),
        }
        .insert(conn)
        .await?;
    }
    Ok(())
}

async fn load_receipt<C: ConnectionTrait>(conn: &C, id: Uuid) -> Result<receipt::Model> {
    receipt::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("goods receipt {id}")))
}

async fn load_receipt_locked(txn: &DatabaseTransaction, id: Uuid) -> Result<receipt::Model> {
    receipt::Entity::find_by_id(id)
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("goods receipt {id}")))
}

async fn load_receipt_lines<C: ConnectionTrait>(
    conn: &C,
    receipt_id: Uuid,
) -> Result<Vec<receipt_line::Model>> {
    receipt_line::Entity::find()
        .filter(receipt_line::Column::ReceiptId.eq(receipt_id))
        .order_by_asc(receipt_line::Column::LineNo)
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
pub struct ReceiptLineView {
    pub id: Uuid,
    pub line_no: i32,
    pub order_line_id: Uuid,
    pub item_id: Option<Uuid>,
    pub sku: String,
    pub item_name: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub rejected_qty: Decimal,
    pub reject_reason: Option<String>,
    pub memo: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ReceiptView {
    pub id: Uuid,
    pub number: Option<String>,
    pub order_id: Uuid,
    pub order_number: Option<String>,
    #[schema(value_type = String, format = Date)]
    pub receipt_date: chrono::NaiveDate,
    pub reference: Option<String>,
    pub carrier: Option<String>,
    pub tracking_no: Option<String>,
    pub vehicle_reg: Option<String>,
    pub delivered_by: Option<String>,
    pub received_by: Option<Uuid>,
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub exchange_rate: Option<Decimal>,
    pub memo: Option<String>,
    pub status: ReceiptStatus,
    /// The stock movement this receipt produced at post.
    pub move_id: Option<Uuid>,
    pub reverses_id: Option<Uuid>,
    pub reversed_by_id: Option<Uuid>,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub posted_at: Option<chrono::DateTime<chrono::Utc>>,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub lines: Vec<ReceiptLineView>,
}

/// A row of the receipt register.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ReceiptHeader {
    pub id: Uuid,
    pub number: Option<String>,
    pub order_id: Uuid,
    pub order_number: Option<String>,
    #[schema(value_type = String, format = Date)]
    pub receipt_date: chrono::NaiveDate,
    pub reference: Option<String>,
    pub status: ReceiptStatus,
}

pub struct ReceiptFilter {
    pub order_id: Option<Uuid>,
    pub status: Option<ReceiptStatus>,
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ReceiptLineRequest {
    pub order_line_id: Uuid,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub rejected_qty: Option<Decimal>,
    pub reject_reason: Option<String>,
    pub memo: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateReceiptRequest {
    pub order_id: Uuid,
    #[schema(value_type = String, format = Date)]
    pub receipt_date: chrono::NaiveDate,
    /// Supplier delivery note number.
    pub reference: Option<String>,
    pub carrier: Option<String>,
    pub tracking_no: Option<String>,
    pub vehicle_reg: Option<String>,
    pub delivered_by: Option<String>,
    /// Overrides the order's rate for this receipt only.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub exchange_rate: Option<Decimal>,
    pub memo: Option<String>,
    pub lines: Vec<ReceiptLineRequest>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ReverseReceiptRequest {
    #[serde(default)]
    pub reason: String,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct ListReceiptsQuery {
    pub order_id: Option<Uuid>,
    pub status: Option<ReceiptStatus>,
    /// Receipt date range, inclusive.
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

fn new_receipt(req: CreateReceiptRequest, created_by: Option<Uuid>) -> NewReceipt {
    NewReceipt {
        order_id: req.order_id,
        receipt_date: req.receipt_date,
        reference: req.reference,
        carrier: req.carrier,
        tracking_no: req.tracking_no,
        vehicle_reg: req.vehicle_reg,
        delivered_by: req.delivered_by,
        exchange_rate: req.exchange_rate,
        memo: req.memo,
        lines: req
            .lines
            .into_iter()
            .map(|l| ReceiptLineInput {
                order_line_id: l.order_line_id,
                qty: l.qty,
                rejected_qty: l.rejected_qty,
                reject_reason: l.reject_reason,
                memo: l.memo,
            })
            .collect(),
        created_by,
    }
}

pub(crate) fn routes() -> Router {
    Router::new()
        .route(
            "/procurement/receipts",
            get(list_receipts).post(create_receipt),
        )
        .route(
            "/procurement/receipts/{id}",
            get(get_receipt).put(update_receipt).delete(delete_receipt),
        )
        .route("/procurement/receipts/{id}/post", post(post_receipt))
        .route("/procurement/receipts/{id}/reverse", post(reverse_receipt))
        .route("/procurement/orders/{id}/receipts", get(order_receipts))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    list_receipts,
    get_receipt,
    create_receipt,
    update_receipt,
    delete_receipt,
    post_receipt,
    reverse_receipt,
    order_receipts
))]
struct ApiDoc;

#[utoipa::path(get, path = "/procurement/receipts", tag = "procurement",
    params(ListReceiptsQuery),
    responses((status = 200, body = Vec<ReceiptHeader>)))]
async fn list_receipts(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<ListReceiptsQuery>,
) -> Result<Json<Vec<ReceiptHeader>>> {
    authz.require(names::RECEIPTS_VIEW).await?;
    ReceiptService::new(db)
        .list(ReceiptFilter {
            order_id: q.order_id,
            status: q.status,
            from: q.from,
            to: q.to,
        })
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/procurement/orders/{id}/receipts", tag = "procurement",
    params(("id" = Uuid, Path, description = "Order id")),
    responses((status = 200, body = Vec<ReceiptHeader>)))]
async fn order_receipts(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<ReceiptHeader>>> {
    authz.require(names::RECEIPTS_VIEW).await?;
    ReceiptService::new(db)
        .list(ReceiptFilter {
            order_id: Some(id),
            status: None,
            from: None,
            to: None,
        })
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/procurement/receipts/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "Receipt id")),
    responses((status = 200, body = ReceiptView)))]
async fn get_receipt(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<ReceiptView>> {
    authz.require(names::RECEIPTS_VIEW).await?;
    ReceiptService::new(db).view(id).await.map(Json)
}

#[utoipa::path(post, path = "/procurement/receipts", tag = "procurement",
    request_body = CreateReceiptRequest,
    responses((status = 200, body = ReceiptView)))]
async fn create_receipt(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(req): Json<CreateReceiptRequest>,
) -> Result<Json<ReceiptView>> {
    authz.require(names::RECEIPTS_CREATE).await?;
    let view = ReceiptService::new(db)
        .create_draft(new_receipt(req, Some(authz.user.id)))
        .await?;
    audit.0.created("scm.receipt", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(put, path = "/procurement/receipts/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "Receipt id")),
    request_body = CreateReceiptRequest,
    responses((status = 200, body = ReceiptView)))]
async fn update_receipt(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<CreateReceiptRequest>,
) -> Result<Json<ReceiptView>> {
    authz.require(names::RECEIPTS_CREATE).await?;
    let service = ReceiptService::new(db);
    let before = service.view(id).await?;
    let after = service.update_draft(id, new_receipt(req, None)).await?;
    audit.0.updated("scm.receipt", id, &before, &after).await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/procurement/receipts/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "Receipt id")),
    responses((status = 200, body = ReceiptView)))]
async fn delete_receipt(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<ReceiptView>> {
    authz.require(names::RECEIPTS_CREATE).await?;
    let view = ReceiptService::new(db).delete_draft(id).await?;
    audit.0.deleted("scm.receipt", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/receipts/{id}/post", tag = "procurement",
    params(("id" = Uuid, Path, description = "Receipt id")),
    responses((status = 200, body = ReceiptView)))]
async fn post_receipt(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Extension(numbering): Extension<Numbering>,
    Path(id): Path<Uuid>,
) -> Result<Json<ReceiptView>> {
    authz.require(names::RECEIPTS_POST).await?;
    let view = ReceiptService::new(db).post(id, &numbering).await?;
    audit
        .0
        .event(format!(
            "posted goods receipt {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/receipts/{id}/reverse", tag = "procurement",
    params(("id" = Uuid, Path, description = "Receipt id")),
    request_body = ReverseReceiptRequest,
    responses((status = 200, body = ReceiptView)))]
async fn reverse_receipt(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Extension(numbering): Extension<Numbering>,
    Path(id): Path<Uuid>,
    Json(req): Json<ReverseReceiptRequest>,
) -> Result<Json<ReceiptView>> {
    authz.require(names::RECEIPTS_REVERSE).await?;
    let view = ReceiptService::new(db)
        .reverse(id, &req.reason, &numbering, Some(authz.user.id))
        .await?;
    audit
        .0
        .event(format!(
            "reversed goods receipt with {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}
