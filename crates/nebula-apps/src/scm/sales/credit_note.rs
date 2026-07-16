//! Credit notes: a customer credit against a posted sales invoice.
//!
//! Posting validates each line against the invoice under its row lock —
//! no more can be credited than was billed (per line, across all the
//! invoice's credit notes) — and books the mirror of the invoice's AR
//! entry: **Dr Sales + Dr VAT output / Cr AR**. Lines flagged `restock`
//! bring goods physically back into stock at the **original issue cost**
//! (the cost the delivery took them out at, read from the stock ledger,
//! not today's moving average), on a receipt-type movement whose own value
//! books **Dr Inventory / Cr COGS**. status: draft | posted | cancelled.
//! Cancelling reverses both the AR mirror and any restock.

use crate::scm::gl;
use crate::scm::inventory::batch;
use crate::scm::inventory::item::{item, uom};
use crate::scm::inventory::moves::{MoveStatus, MoveType, doc as move_doc, line as move_line};
use crate::scm::inventory::stock::{self, Movement, StockService, ledger};
use crate::scm::sales::customer::customer;
use crate::scm::sales::delivery::delivery;
use crate::scm::sales::invoice::{
    self, InvoiceStatus, TaxLine, Totals, compute_totals, invoice_line, load_invoice_lines,
    load_invoice_locked, tax_rates,
};
use crate::scm::sales::order::{effective_price, load_lines as load_order_lines, order_line};
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
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use crate::scm::inventory::stock::round_money;

/// Where a credit note is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CreditNoteStatus {
    Draft,
    Posted,
    Cancelled,
}

impl CreditNoteStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CreditNoteStatus::Draft => "draft",
            CreditNoteStatus::Posted => "posted",
            CreditNoteStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "draft" => Ok(CreditNoteStatus::Draft),
            "posted" => Ok(CreditNoteStatus::Posted),
            "cancelled" => Ok(CreditNoteStatus::Cancelled),
            other => Err(Error::internal(format!(
                "unknown credit note status {other:?}"
            ))),
        }
    }
}

/// The credit note header.
pub mod credit_note {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "sales_credit_notes")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub number: Option<String>,
        pub customer_id: Uuid,
        pub invoice_id: Uuid,
        pub credit_date: Date,
        pub reason: String,
        pub currency: String,
        #[sea_orm(column_type = "Decimal(Some((20, 8)))")]
        pub exchange_rate: Decimal,
        pub tax_inclusive: bool,
        pub memo: Option<String>,
        pub status: String,
        pub move_id: Option<Uuid>,
        pub posted_at: Option<DateTimeUtc>,
        pub posted_by: Option<Uuid>,
        pub cancelled_at: Option<DateTimeUtc>,
        pub cancelled_by: Option<Uuid>,
        pub cancel_reason: Option<String>,
        pub created_at: DateTimeUtc,
        pub created_by: Option<Uuid>,
        pub updated_at: DateTimeUtc,
        pub updated_by: Option<Uuid>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// One credit note line.
pub mod credit_note_line {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "sales_credit_note_lines")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub credit_note_id: Uuid,
        pub invoice_line_id: Option<Uuid>,
        pub line_no: i32,
        pub description: String,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub qty: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub unit_price: Decimal,
        #[sea_orm(column_type = "Decimal(Some((7, 4)))", nullable)]
        pub discount_pct: Option<Decimal>,
        pub tax_code_id: Option<Uuid>,
        pub restock: bool,
        pub restock_warehouse_id: Option<Uuid>,
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

/// Does a posted (or draft) credit note exist against this invoice? Blocks
/// invoice cancellation.
pub(crate) async fn has_credit_notes<C: ConnectionTrait>(
    conn: &C,
    invoice_id: Uuid,
) -> Result<bool> {
    Ok(credit_note::Entity::find()
        .filter(credit_note::Column::InvoiceId.eq(invoice_id))
        .filter(credit_note::Column::Status.ne(CreditNoteStatus::Cancelled.as_str()))
        .one(conn)
        .await?
        .is_some())
}

/// Quantity already credited per invoice line by *posted* credit notes.
async fn credited_qty<C: ConnectionTrait>(
    conn: &C,
    invoice_id: Uuid,
    exclude_note: Option<Uuid>,
) -> Result<HashMap<Uuid, Decimal>> {
    let notes: Vec<credit_note::Model> = credit_note::Entity::find()
        .filter(credit_note::Column::InvoiceId.eq(invoice_id))
        .filter(credit_note::Column::Status.eq(CreditNoteStatus::Posted.as_str()))
        .all(conn)
        .await?;
    let ids: Vec<Uuid> = notes
        .into_iter()
        .filter(|n| exclude_note != Some(n.id))
        .map(|n| n.id)
        .collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let lines = credit_note_line::Entity::find()
        .filter(credit_note_line::Column::CreditNoteId.is_in(ids))
        .all(conn)
        .await?;
    let mut map: HashMap<Uuid, Decimal> = HashMap::new();
    for l in lines {
        if let Some(il) = l.invoice_line_id {
            *map.entry(il).or_default() += l.qty;
        }
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// A credit note line as supplied by a caller.
pub struct CreditNoteLineInput {
    pub invoice_line_id: Uuid,
    pub description: Option<String>,
    pub qty: Decimal,
    pub unit_price: Option<Decimal>,
    pub discount_pct: Option<Decimal>,
    pub tax_code_id: Option<Uuid>,
    pub restock: bool,
    pub restock_warehouse_id: Option<Uuid>,
    pub batch_no: Option<String>,
    pub serial_nos: Option<Vec<String>>,
    pub memo: Option<String>,
}

/// A new draft credit note.
pub struct NewCreditNote {
    pub invoice_id: Uuid,
    pub credit_date: chrono::NaiveDate,
    pub reason: String,
    pub memo: Option<String>,
    pub lines: Vec<CreditNoteLineInput>,
    pub created_by: Option<Uuid>,
}

/// The credit note service over one (tenant) connection.
pub struct CreditNoteService {
    db: DatabaseConnection,
}

impl CreditNoteService {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn create_draft(&self, new: NewCreditNote) -> Result<CreditNoteView> {
        let inv = validate_note(&self.db, &new, None).await?;
        let txn = self.db.begin().await?;
        let note_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        credit_note::ActiveModel {
            id: Set(note_id),
            number: Set(None),
            customer_id: Set(inv.customer_id),
            invoice_id: Set(new.invoice_id),
            credit_date: Set(new.credit_date),
            reason: Set(new.reason.trim().to_string()),
            currency: Set(inv.currency.clone()),
            exchange_rate: Set(inv.exchange_rate),
            tax_inclusive: Set(inv.tax_inclusive),
            memo: Set(clean(new.memo)),
            status: Set(CreditNoteStatus::Draft.as_str().to_string()),
            move_id: Set(None),
            posted_at: Set(None),
            posted_by: Set(None),
            cancelled_at: Set(None),
            cancelled_by: Set(None),
            cancel_reason: Set(None),
            created_at: Set(now),
            created_by: Set(new.created_by),
            updated_at: Set(now),
            updated_by: Set(None),
        }
        .insert(&txn)
        .await?;
        insert_lines(&txn, note_id, &new.lines, &inv).await?;
        txn.commit().await?;
        self.view(note_id).await
    }

    pub async fn update_draft(&self, id: Uuid, new: NewCreditNote) -> Result<CreditNoteView> {
        let txn = self.db.begin().await?;
        let existing = load_note_locked(&txn, id).await?;
        if CreditNoteStatus::parse(&existing.status)? != CreditNoteStatus::Draft {
            return Err(Error::Validation(
                "only a draft credit note can be edited".into(),
            ));
        }
        if existing.invoice_id != new.invoice_id {
            return Err(Error::Validation(
                "a credit note's invoice cannot change; delete the draft and create a new one"
                    .into(),
            ));
        }
        let inv = validate_note(&txn, &new, Some(id)).await?;
        credit_note_line::Entity::delete_many()
            .filter(credit_note_line::Column::CreditNoteId.eq(id))
            .exec(&txn)
            .await?;
        insert_lines(&txn, id, &new.lines, &inv).await?;
        let mut active: credit_note::ActiveModel = existing.into();
        active.credit_date = Set(new.credit_date);
        active.reason = Set(new.reason.trim().to_string());
        active.memo = Set(clean(new.memo));
        active.updated_at = Set(chrono::Utc::now());
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    pub async fn delete_draft(&self, id: Uuid) -> Result<CreditNoteView> {
        let view = self.view(id).await?;
        let txn = self.db.begin().await?;
        let existing = load_note_locked(&txn, id).await?;
        if CreditNoteStatus::parse(&existing.status)? != CreditNoteStatus::Draft {
            return Err(Error::Validation(
                "only a draft credit note can be deleted".into(),
            ));
        }
        credit_note::Entity::delete_by_id(id).exec(&txn).await?;
        txn.commit().await?;
        Ok(view)
    }

    /// Post a draft credit note: validate credited quantities under the
    /// invoice lock, restock any returned goods at their issue cost, and
    /// book Dr Sales + Dr VAT output / Cr AR (plus the restock's own Dr
    /// Inventory / Cr COGS) — one transaction.
    pub async fn post(
        &self,
        id: Uuid,
        numbering: &Numbering,
        by: Option<Uuid>,
        gl: &gl::Gl,
    ) -> Result<CreditNoteView> {
        let txn = self.db.begin().await?;
        let note = load_note_locked(&txn, id).await?;
        if CreditNoteStatus::parse(&note.status)? != CreditNoteStatus::Draft {
            return Err(Error::Validation(
                "only a draft credit note can be posted".into(),
            ));
        }
        let inv = load_invoice_locked(&txn, note.invoice_id).await?;
        if InvoiceStatus::parse(&inv.status)? != InvoiceStatus::Posted {
            return Err(Error::Validation(
                "the invoice is not posted; nothing to credit".into(),
            ));
        }
        let lines = load_note_lines(&txn, id).await?;
        if lines.is_empty() {
            return Err(Error::Validation(
                "a credit note needs at least one line".into(),
            ));
        }
        let inv_lines: HashMap<Uuid, invoice_line::Model> = load_invoice_lines(&txn, inv.id)
            .await?
            .into_iter()
            .map(|l| (l.id, l))
            .collect();
        let already = credited_qty(&txn, inv.id, Some(id)).await?;

        // No more than was billed can be credited, accumulated per invoice
        // line.
        let mut crediting: HashMap<Uuid, Decimal> = HashMap::new();
        for line in &lines {
            let il_id = line
                .invoice_line_id
                .ok_or_else(|| Error::internal("credit line without an invoice line"))?;
            let il = inv_lines.get(&il_id).ok_or_else(|| {
                Error::Validation(format!(
                    "line {} does not belong to this invoice",
                    line.line_no
                ))
            })?;
            let taken = crediting.entry(il_id).or_default();
            *taken += line.qty;
            let prior = already.get(&il_id).copied().unwrap_or(Decimal::ZERO);
            if prior + *taken > il.qty {
                return Err(Error::Validation(format!(
                    "line {}: crediting {} exceeds the {} still creditable on the invoice line",
                    line.line_no,
                    line.qty,
                    il.qty - prior
                )));
            }
        }

        // Restock, if any line returns goods. Built like a delivery reversal
        // (a receipt at the issue cost), then COGS rides its ledger value.
        let restock_lines: Vec<&credit_note_line::Model> =
            lines.iter().filter(|l| l.restock).collect();
        let mut move_id: Option<Uuid> = None;
        if !restock_lines.is_empty() {
            move_id = Some(
                self.post_restock(&txn, &note, &inv, &restock_lines, &inv_lines)
                    .await?,
            );
        }

        // The AR mirror.
        let customer = load_customer(&txn, note.customer_id).await?;
        let totals = totals_for(&txn, &note, &lines, customer.tax_exempt).await?;
        let rate = note.exchange_rate;
        let net_base = round_money((totals.total - totals.tax) * rate);
        let tax_base = round_money(totals.tax * rate);
        let gross_base = round_money(totals.total * rate);

        let number = numbering
            .next(&txn, crate::scm::SALES_CREDIT_NOTE_SERIES)
            .await?;
        let now = chrono::Utc::now();
        let ar_request = gl::ar_invoice_request(
            format!("sales.credit_note:{id}:post"),
            format!(
                "Credit note {} against {}",
                number.formatted,
                inv.number.as_deref().unwrap_or("sales invoice")
            ),
            note.credit_date,
            net_base,
            tax_base,
            gross_base,
            true, // mirror: Dr Sales + Dr VAT / Cr AR
            gl.tenant_id(),
        )?;
        if let Some(req) = &ar_request {
            gl::stage(&txn, req).await?;
        }
        // COGS on the restock, if any.
        let cogs_request = match move_id {
            Some(mid) => {
                let req = gl::cogs_move_request(
                    &txn,
                    format!("sales.credit_note:{id}:restock"),
                    mid,
                    format!("Restock on credit note {}", number.formatted),
                    note.credit_date,
                    gl.tenant_id(),
                )
                .await?;
                if let Some(r) = &req {
                    gl::stage(&txn, r).await?;
                }
                req
            }
            None => None,
        };

        let mut active: credit_note::ActiveModel = note.into();
        active.status = Set(CreditNoteStatus::Posted.as_str().to_string());
        active.number = Set(Some(number.formatted));
        active.move_id = Set(move_id);
        active.posted_at = Set(Some(now));
        active.posted_by = Set(by);
        active.updated_at = Set(now);
        active.update(&txn).await?;
        txn.commit().await?;
        if let Some(req) = ar_request {
            gl.publish(req).await;
        }
        if let Some(req) = cogs_request {
            gl.publish(req).await;
        }
        self.view(id).await
    }

    /// The restock receipt movement: each restocked line re-enters stock at
    /// the cost its delivery took it out at. Returns the movement id.
    async fn post_restock(
        &self,
        txn: &DatabaseTransaction,
        note: &credit_note::Model,
        inv: &invoice::invoice::Model,
        restock_lines: &[&credit_note_line::Model],
        inv_lines: &HashMap<Uuid, invoice_line::Model>,
    ) -> Result<Uuid> {
        let order_id = inv.order_id.ok_or_else(|| {
            Error::Validation("cannot restock a credit note on a direct invoice".into())
        })?;
        let order_lines: HashMap<Uuid, order_line::Model> = load_order_lines(txn, order_id)
            .await?
            .into_iter()
            .map(|l| (l.id, l))
            .collect();
        let item_ids: Vec<Uuid> = restock_lines
            .iter()
            .filter_map(|l| l.invoice_line_id)
            .filter_map(|il| inv_lines.get(&il))
            .filter_map(|il| il.order_line_id)
            .filter_map(|ol| order_lines.get(&ol))
            .map(|ol| ol.item_id)
            .collect();
        let items: HashMap<Uuid, item::Model> = item::Entity::find()
            .filter(item::Column::Id.is_in(item_ids.clone()))
            .all(txn)
            .await?
            .into_iter()
            .map(|i| (i.id, i))
            .collect();
        let uom_ids: Vec<Uuid> = items.values().map(|i| i.uom_id).collect();
        let uoms: HashMap<Uuid, uom::Model> = uom::Entity::find()
            .filter(uom::Column::Id.is_in(uom_ids))
            .all(txn)
            .await?
            .into_iter()
            .map(|u| (u.id, u))
            .collect();

        // The issue cost per item, from the order's posted delivery ledger.
        let issue_costs = issue_costs_for(txn, order_id, &item_ids).await?;

        let move_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        move_doc::ActiveModel {
            id: Set(move_id),
            number: Set(None),
            move_type: Set(MoveType::Receipt.as_str().to_string()),
            entry_date: Set(note.credit_date),
            memo: Set(format!(
                "Restock on credit note against {}",
                inv.number.as_deref().unwrap_or("sales invoice")
            )),
            reference: Set(inv.number.clone()),
            from_warehouse_id: Set(None),
            to_warehouse_id: Set(None),
            status: Set(MoveStatus::Posted.as_str().to_string()),
            source: Set(Some(format!("sales.credit_note:{}", note.id))),
            reverses_id: Set(None),
            reversed_by_id: Set(None),
            posted_at: Set(Some(now)),
            created_at: Set(now),
            created_by: Set(note.created_by),
        }
        .insert(txn)
        .await?;

        // Pre-lock the levels ascending.
        let mut gates: Vec<(Uuid, Uuid)> = Vec::new();
        for line in restock_lines {
            let (item_id, wh) = self.restock_target(line, inv_lines, &order_lines)?;
            gates.push((item_id, wh));
        }
        gates.sort();
        gates.dedup();
        for (item_id, wh) in &gates {
            stock::lock_or_init_level(txn, *item_id, *wh).await?;
        }

        for (i, line) in restock_lines.iter().enumerate() {
            let (item_id, warehouse_id) = self.restock_target(line, inv_lines, &order_lines)?;
            let item = items
                .get(&item_id)
                .ok_or_else(|| Error::internal("restock line lost its item"))?;
            let stock_uom = uoms.get(&item.uom_id).ok_or_else(|| {
                Error::internal(format!("stock uom missing for item {}", item.sku))
            })?;
            let unit_cost = issue_costs
                .get(&item_id)
                .copied()
                .unwrap_or_else(|| Decimal::ZERO);

            let serial_names = line_serial_names(line)?;
            if !item.track_serials && !serial_names.is_empty() {
                return Err(Error::Validation(format!(
                    "restock line {}: item {} does not track serial numbers",
                    line.line_no, item.sku
                )));
            }
            let batch_id = match (&line.batch_no, item.track_batches) {
                (Some(no), _) => Some(
                    batch::find_or_create_batch(
                        txn,
                        item,
                        no,
                        note.credit_date,
                        None,
                        note.created_by,
                    )
                    .await?
                    .id,
                ),
                (None, true) => {
                    return Err(Error::Validation(format!(
                        "restock line {}: item {} tracks batches; name the lot returning",
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
                line_no: Set((i + 1) as i32),
                item_id: Set(item_id),
                qty: Set(line.qty),
                entered_uom_id: Set(None),
                unit_cost: Set(Some(unit_cost)),
                batch_no: Set(line.batch_no.clone()),
                batch_id: Set(batch_id),
                serial_nos: Set(line.serial_nos.clone()),
                memo: Set(line.memo.clone()),
                created_at: Set(now),
            }
            .insert(txn)
            .await?;
            StockService::apply(
                txn,
                move_id,
                ml.id,
                note.credit_date,
                item,
                stock_uom,
                warehouse_id,
                batch_id,
                Movement::Receipt {
                    qty: line.qty,
                    unit_cost,
                },
            )
            .await?;
            if !names.is_empty() {
                batch::serials_in(
                    txn,
                    item,
                    ml.id,
                    warehouse_id,
                    batch_id,
                    &names,
                    note.credit_date,
                    note.created_by,
                )
                .await?;
            }
            if batch_id != line.batch_id {
                let mut active: credit_note_line::ActiveModel = (*line).clone().into();
                active.batch_id = Set(batch_id);
                active.update(txn).await?;
            }
        }
        Ok(move_id)
    }

    /// The (item, warehouse) a restock line returns into.
    fn restock_target(
        &self,
        line: &credit_note_line::Model,
        inv_lines: &HashMap<Uuid, invoice_line::Model>,
        order_lines: &HashMap<Uuid, order_line::Model>,
    ) -> Result<(Uuid, Uuid)> {
        let il = line
            .invoice_line_id
            .and_then(|id| inv_lines.get(&id))
            .ok_or_else(|| Error::internal("restock line lost its invoice line"))?;
        let ol = il
            .order_line_id
            .and_then(|id| order_lines.get(&id))
            .ok_or_else(|| Error::Validation("cannot restock a line with no order line".into()))?;
        let warehouse_id = line
            .restock_warehouse_id
            .or(ol.warehouse_id)
            .ok_or_else(|| Error::Validation("a restock line needs a warehouse".into()))?;
        Ok((ol.item_id, warehouse_id))
    }

    /// Cancel a posted credit note: reverse the AR mirror and, if it
    /// restocked, issue the goods back out at their restock cost.
    pub async fn cancel(
        &self,
        id: Uuid,
        reason: &str,
        by: Option<Uuid>,
        gl: &gl::Gl,
    ) -> Result<CreditNoteView> {
        let txn = self.db.begin().await?;
        let note = load_note_locked(&txn, id).await?;
        if CreditNoteStatus::parse(&note.status)? != CreditNoteStatus::Posted {
            return Err(Error::Validation(
                "only a posted credit note can be cancelled".into(),
            ));
        }
        let lines = load_note_lines(&txn, id).await?;
        let customer = load_customer(&txn, note.customer_id).await?;
        let totals = totals_for(&txn, &note, &lines, customer.tax_exempt).await?;
        let rate = note.exchange_rate;
        let net_base = round_money((totals.total - totals.tax) * rate);
        let tax_base = round_money(totals.tax * rate);
        let gross_base = round_money(totals.total * rate);
        let now = chrono::Utc::now();

        // Reverse the AR mirror: back to Dr AR / Cr Sales / Cr VAT.
        let ar_request = gl::ar_invoice_request(
            format!("sales.credit_note:{id}:cancel"),
            format!(
                "Cancellation of credit note {}",
                note.number.as_deref().unwrap_or("?")
            ),
            now.date_naive(),
            net_base,
            tax_base,
            gross_base,
            false,
            gl.tenant_id(),
        )?;
        if let Some(req) = &ar_request {
            gl::stage(&txn, req).await?;
        }

        // Undo the restock: issue the restocked goods back out at the cost
        // they came in at, and book Dr COGS / Cr Inventory.
        let cogs_request = match note.move_id {
            Some(original_move_id) => {
                let mid = self
                    .reverse_restock(&txn, &note, original_move_id, by)
                    .await?;
                let req = gl::cogs_move_request(
                    &txn,
                    format!("sales.credit_note:{id}:restock_cancel"),
                    mid,
                    format!(
                        "Reversal of restock on credit note {}",
                        note.number.as_deref().unwrap_or("?")
                    ),
                    now.date_naive(),
                    gl.tenant_id(),
                )
                .await?;
                if let Some(r) = &req {
                    gl::stage(&txn, r).await?;
                }
                req
            }
            None => None,
        };

        let mut active: credit_note::ActiveModel = note.into();
        active.status = Set(CreditNoteStatus::Cancelled.as_str().to_string());
        active.cancelled_at = Set(Some(now));
        active.cancelled_by = Set(by);
        active.cancel_reason = Set(Some(reason.trim().to_string()).filter(|r| !r.is_empty()));
        active.updated_at = Set(now);
        active.update(&txn).await?;
        txn.commit().await?;
        if let Some(req) = ar_request {
            gl.publish(req).await;
        }
        if let Some(req) = cogs_request {
            gl.publish(req).await;
        }
        self.view(id).await
    }

    /// Issue the restocked goods back out at their restock cost (the mirror
    /// of `post_restock`). Returns the reversal movement id.
    async fn reverse_restock(
        &self,
        txn: &DatabaseTransaction,
        note: &credit_note::Model,
        original_move_id: Uuid,
        by: Option<Uuid>,
    ) -> Result<Uuid> {
        let original_move = move_doc::Entity::find_by_id(original_move_id)
            .lock_exclusive()
            .one(txn)
            .await?
            .ok_or_else(|| Error::internal("credit note's restock movement is missing"))?;
        let rows = ledger::Entity::find()
            .filter(ledger::Column::MoveId.eq(original_move_id))
            .order_by_asc(ledger::Column::Seq)
            .all(txn)
            .await?;
        let item_ids: Vec<Uuid> = rows.iter().map(|r| r.item_id).collect();
        let items: HashMap<Uuid, item::Model> = item::Entity::find()
            .filter(item::Column::Id.is_in(item_ids))
            .all(txn)
            .await?
            .into_iter()
            .map(|i| (i.id, i))
            .collect();
        let uom_ids: Vec<Uuid> = items.values().map(|i| i.uom_id).collect();
        let uoms: HashMap<Uuid, uom::Model> = uom::Entity::find()
            .filter(uom::Column::Id.is_in(uom_ids))
            .all(txn)
            .await?
            .into_iter()
            .map(|u| (u.id, u))
            .collect();

        let mut gates: Vec<(Uuid, Uuid)> =
            rows.iter().map(|r| (r.item_id, r.warehouse_id)).collect();
        gates.sort();
        gates.dedup();
        for (item_id, wh) in &gates {
            stock::lock_or_init_level(txn, *item_id, *wh).await?;
        }

        let now = chrono::Utc::now();
        let reversal_move_id = Uuid::new_v4();
        move_doc::ActiveModel {
            id: Set(reversal_move_id),
            number: Set(None),
            move_type: Set(MoveType::Issue.as_str().to_string()),
            entry_date: Set(now.date_naive()),
            memo: Set(format!(
                "Reversal of restock on credit note {}",
                note.number.as_deref().unwrap_or("?")
            )),
            reference: Set(original_move.number.clone()),
            from_warehouse_id: Set(None),
            to_warehouse_id: Set(None),
            status: Set(MoveStatus::Posted.as_str().to_string()),
            source: Set(Some(format!("sales.credit_note:{}:reversal", note.id))),
            reverses_id: Set(Some(original_move.id)),
            reversed_by_id: Set(None),
            posted_at: Set(Some(now)),
            created_at: Set(now),
            created_by: Set(by),
        }
        .insert(txn)
        .await?;

        let original_move_lines = move_line::Entity::find()
            .filter(move_line::Column::MoveId.eq(original_move_id))
            .order_by_asc(move_line::Column::LineNo)
            .all(txn)
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
            .insert(txn)
            .await?;
        }
        for row in &rows {
            let item = items
                .get(&row.item_id)
                .ok_or_else(|| Error::internal("restock ledger row lost its item"))?;
            let stock_uom = uoms
                .get(&item.uom_id)
                .ok_or_else(|| Error::internal(format!("stock uom missing for {}", item.sku)))?;
            let mirror_line = *mirror_line_ids
                .get(&row.move_line_id)
                .ok_or_else(|| Error::internal("ledger row without a document line"))?;
            // The restock was a receipt (positive qty_delta); issue it back.
            StockService::apply(
                txn,
                reversal_move_id,
                mirror_line,
                now.date_naive(),
                item,
                stock_uom,
                row.warehouse_id,
                row.batch_id,
                Movement::Issue {
                    qty: row.qty_delta,
                    covered_by_reservation: Decimal::ZERO,
                },
            )
            .await?;
        }
        // Serial units the restock brought back leave again.
        for ml in &original_move_lines {
            let names = batch::serial_names_of_line(txn, ml.id).await?;
            if names.is_empty() {
                continue;
            }
            let item = items
                .get(&ml.item_id)
                .ok_or_else(|| Error::internal("restock line lost its item"))?;
            let warehouse_id = rows
                .iter()
                .find(|r| r.move_line_id == ml.id)
                .map(|r| r.warehouse_id)
                .ok_or_else(|| Error::internal("restock line without a ledger row"))?;
            batch::serials_out(
                txn,
                item,
                mirror_line_ids[&ml.id],
                warehouse_id,
                &names,
                batch::SerialStatus::Issued,
            )
            .await?;
        }
        Ok(reversal_move_id)
    }

    pub async fn list(&self, filter: CreditNoteFilter) -> Result<Vec<CreditNoteHeader>> {
        let mut query = credit_note::Entity::find();
        if let Some(customer_id) = filter.customer_id {
            query = query.filter(credit_note::Column::CustomerId.eq(customer_id));
        }
        if let Some(invoice_id) = filter.invoice_id {
            query = query.filter(credit_note::Column::InvoiceId.eq(invoice_id));
        }
        if let Some(s) = filter.status {
            query = query.filter(credit_note::Column::Status.eq(s.as_str()));
        }
        let rows = query
            .order_by_desc(credit_note::Column::CreditDate)
            .order_by_desc(credit_note::Column::CreatedAt)
            .all(&self.db)
            .await?;
        let customer_ids: Vec<Uuid> = rows.iter().map(|r| r.customer_id).collect();
        let customers: HashMap<Uuid, customer::Model> = customer::Entity::find()
            .filter(customer::Column::Id.is_in(customer_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|c| (c.id, c))
            .collect();
        let invoice_ids: Vec<Uuid> = rows.iter().map(|r| r.invoice_id).collect();
        let invoices: HashMap<Uuid, invoice::invoice::Model> = invoice::invoice::Entity::find()
            .filter(invoice::invoice::Column::Id.is_in(invoice_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|i| (i.id, i))
            .collect();
        let mut headers = Vec::with_capacity(rows.len());
        for r in &rows {
            let lines = load_note_lines(&self.db, r.id).await?;
            let cust = customers.get(&r.customer_id);
            let totals = totals_for(
                &self.db,
                r,
                &lines,
                cust.map(|c| c.tax_exempt).unwrap_or(false),
            )
            .await?;
            headers.push(CreditNoteHeader {
                id: r.id,
                number: r.number.clone(),
                customer_id: r.customer_id,
                customer_name: cust.map(|c| c.name.clone()).unwrap_or_default(),
                invoice_id: r.invoice_id,
                invoice_number: invoices.get(&r.invoice_id).and_then(|i| i.number.clone()),
                credit_date: r.credit_date,
                currency: r.currency.clone(),
                total: totals.total,
                status: CreditNoteStatus::parse(&r.status)?,
            });
        }
        Ok(headers)
    }

    pub async fn view(&self, id: Uuid) -> Result<CreditNoteView> {
        let row = load_note(&self.db, id).await?;
        let lines = load_note_lines(&self.db, id).await?;
        let customer = customer::Entity::find_by_id(row.customer_id)
            .one(&self.db)
            .await?;
        let inv = invoice::invoice::Entity::find_by_id(row.invoice_id)
            .one(&self.db)
            .await?;
        let tax_exempt = customer.as_ref().map(|c| c.tax_exempt).unwrap_or(false);
        let rate_ids: Vec<Uuid> = lines.iter().filter_map(|l| l.tax_code_id).collect();
        let rates = tax_rates(&self.db, &rate_ids).await?;

        let mut subtotal = Decimal::ZERO;
        let mut tax_total = Decimal::ZERO;
        let line_views: Vec<CreditNoteLineView> = lines
            .iter()
            .map(|l| {
                let rate = if tax_exempt {
                    Decimal::ZERO
                } else {
                    l.tax_code_id
                        .and_then(|id| rates.get(&id).copied())
                        .unwrap_or(Decimal::ZERO)
                };
                let line_amt = round_money(l.qty * effective_price(l.unit_price, l.discount_pct));
                let (net, tax) = if row.tax_inclusive {
                    let n = round_money(line_amt / (Decimal::ONE + rate / Decimal::ONE_HUNDRED));
                    (n, line_amt - n)
                } else {
                    (
                        line_amt,
                        round_money(line_amt * rate / Decimal::ONE_HUNDRED),
                    )
                };
                subtotal += net;
                tax_total += tax;
                CreditNoteLineView {
                    id: l.id,
                    line_no: l.line_no,
                    invoice_line_id: l.invoice_line_id,
                    description: l.description.clone(),
                    qty: l.qty,
                    unit_price: l.unit_price,
                    discount_pct: l.discount_pct,
                    tax_code_id: l.tax_code_id,
                    net,
                    tax,
                    restock: l.restock,
                    restock_warehouse_id: l.restock_warehouse_id,
                    batch_no: l.batch_no.clone(),
                    serial_nos: line_serial_names(l).unwrap_or_default(),
                    memo: l.memo.clone(),
                }
            })
            .collect();
        let total = round_money(subtotal + tax_total);

        Ok(CreditNoteView {
            id: row.id,
            number: row.number,
            customer_id: row.customer_id,
            customer_name: customer.map(|c| c.name).unwrap_or_default(),
            invoice_id: row.invoice_id,
            invoice_number: inv.and_then(|i| i.number),
            credit_date: row.credit_date,
            reason: row.reason,
            currency: row.currency,
            exchange_rate: row.exchange_rate,
            tax_inclusive: row.tax_inclusive,
            memo: row.memo,
            status: CreditNoteStatus::parse(&row.status)?,
            move_id: row.move_id,
            cancel_reason: row.cancel_reason,
            subtotal,
            tax: tax_total,
            total,
            posted_at: row.posted_at,
            created_at: row.created_at,
            lines: line_views,
        })
    }
}

/// The moving-average cost each item last went out at on this order's
/// posted deliveries — the "issue cost" a restock re-enters at.
async fn issue_costs_for<C: ConnectionTrait>(
    conn: &C,
    order_id: Uuid,
    item_ids: &[Uuid],
) -> Result<HashMap<Uuid, Decimal>> {
    let deliveries = delivery::Entity::find()
        .filter(delivery::Column::OrderId.eq(order_id))
        .all(conn)
        .await?;
    let move_ids: Vec<Uuid> = deliveries.into_iter().filter_map(|d| d.move_id).collect();
    if move_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let wanted: HashSet<Uuid> = item_ids.iter().copied().collect();
    // Ascending seq, so the last write for an item wins — the cost it most
    // recently left at.
    let rows = ledger::Entity::find()
        .filter(ledger::Column::MoveId.is_in(move_ids))
        .order_by_asc(ledger::Column::Seq)
        .all(conn)
        .await?;
    let mut costs: HashMap<Uuid, Decimal> = HashMap::new();
    for r in rows {
        if r.qty_delta < Decimal::ZERO && wanted.contains(&r.item_id) {
            costs.insert(r.item_id, r.unit_cost);
        }
    }
    Ok(costs)
}

fn clean(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// Draft-time validation: the invoice is posted, lines point at its lines,
/// quantities within the still-creditable balance, restock lines name a
/// warehouse. Returns the invoice for the header snapshot.
async fn validate_note<C: ConnectionTrait>(
    conn: &C,
    new: &NewCreditNote,
    exclude: Option<Uuid>,
) -> Result<invoice::invoice::Model> {
    if new.reason.trim().is_empty() {
        return Err(Error::Validation("a credit note needs a reason".into()));
    }
    if new.lines.is_empty() {
        return Err(Error::Validation(
            "a credit note needs at least one line".into(),
        ));
    }
    let inv = invoice::invoice::Entity::find_by_id(new.invoice_id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("sales invoice {}", new.invoice_id)))?;
    if InvoiceStatus::parse(&inv.status)? != InvoiceStatus::Posted {
        return Err(Error::Validation(
            "only a posted invoice can be credited".into(),
        ));
    }
    let inv_lines: HashMap<Uuid, invoice_line::Model> = load_invoice_lines(conn, inv.id)
        .await?
        .into_iter()
        .map(|l| (l.id, l))
        .collect();
    let already = credited_qty(conn, inv.id, exclude).await?;
    let mut crediting: HashMap<Uuid, Decimal> = HashMap::new();
    for (i, l) in new.lines.iter().enumerate() {
        let line_no = i + 1;
        let Some(il) = inv_lines.get(&l.invoice_line_id) else {
            return Err(Error::Validation(format!(
                "line {line_no} does not belong to this invoice"
            )));
        };
        if l.qty <= Decimal::ZERO {
            return Err(Error::Validation(format!(
                "line {line_no}: quantity must be positive"
            )));
        }
        let taken = crediting.entry(l.invoice_line_id).or_default();
        *taken += l.qty;
        let prior = already
            .get(&l.invoice_line_id)
            .copied()
            .unwrap_or(Decimal::ZERO);
        if prior + *taken > il.qty {
            return Err(Error::Validation(format!(
                "line {line_no}: crediting {} exceeds the {} still creditable",
                l.qty,
                il.qty - prior
            )));
        }
    }
    Ok(inv)
}

async fn insert_lines(
    txn: &DatabaseTransaction,
    note_id: Uuid,
    lines: &[CreditNoteLineInput],
    inv: &invoice::invoice::Model,
) -> Result<()> {
    let inv_lines: HashMap<Uuid, invoice_line::Model> = load_invoice_lines(txn, inv.id)
        .await?
        .into_iter()
        .map(|l| (l.id, l))
        .collect();
    let now = chrono::Utc::now();
    for (i, l) in lines.iter().enumerate() {
        let il = inv_lines.get(&l.invoice_line_id);
        let description = match l.description.clone().filter(|d| !d.trim().is_empty()) {
            Some(d) => d,
            None => il
                .map(|il| il.description.clone())
                .unwrap_or_else(|| "Line".to_string()),
        };
        let unit_price = l
            .unit_price
            .or_else(|| il.map(|il| il.unit_price))
            .unwrap_or(Decimal::ZERO);
        credit_note_line::ActiveModel {
            id: Set(Uuid::new_v4()),
            credit_note_id: Set(note_id),
            invoice_line_id: Set(Some(l.invoice_line_id)),
            line_no: Set((i + 1) as i32),
            description: Set(description),
            qty: Set(l.qty),
            unit_price: Set(unit_price),
            discount_pct: Set(l.discount_pct.or_else(|| il.and_then(|il| il.discount_pct))),
            tax_code_id: Set(l.tax_code_id.or_else(|| il.and_then(|il| il.tax_code_id))),
            restock: Set(l.restock),
            restock_warehouse_id: Set(l.restock_warehouse_id),
            batch_no: Set(l.batch_no.clone().filter(|b| !b.trim().is_empty())),
            batch_id: Set(None),
            serial_nos: Set(serials_to_json(l.serial_nos.as_deref())),
            memo: Set(l.memo.clone().filter(|m| !m.trim().is_empty())),
            created_at: Set(now),
        }
        .insert(txn)
        .await?;
    }
    Ok(())
}

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

fn line_serial_names(l: &credit_note_line::Model) -> Result<Vec<String>> {
    match &l.serial_nos {
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|e| Error::internal(format!("unreadable serial list on a line: {e}"))),
        None => Ok(Vec::new()),
    }
}

/// The credit note's totals, resolving tax rates over the seam.
async fn totals_for<C: ConnectionTrait>(
    conn: &C,
    note: &credit_note::Model,
    lines: &[credit_note_line::Model],
    tax_exempt: bool,
) -> Result<Totals> {
    let rate_ids: Vec<Uuid> = lines.iter().filter_map(|l| l.tax_code_id).collect();
    let rates = tax_rates(conn, &rate_ids).await?;
    let tax_lines: Vec<TaxLine> = lines
        .iter()
        .map(|l| TaxLine {
            qty: l.qty,
            unit_price: l.unit_price,
            discount_pct: l.discount_pct,
            tax_code_id: l.tax_code_id,
        })
        .collect();
    Ok(compute_totals(
        &tax_lines,
        &rates,
        note.tax_inclusive,
        tax_exempt,
        None,
        None,
        None,
    ))
}

/// The credit note's gross total in its own currency — shared with the
/// payment module so allocations use the figure the view shows.
pub(crate) async fn credit_note_total<C: ConnectionTrait>(conn: &C, id: Uuid) -> Result<Decimal> {
    let note = load_note(conn, id).await?;
    let lines = load_note_lines(conn, id).await?;
    let customer = customer::Entity::find_by_id(note.customer_id)
        .one(conn)
        .await?;
    let totals = totals_for(
        conn,
        &note,
        &lines,
        customer.map(|c| c.tax_exempt).unwrap_or(false),
    )
    .await?;
    Ok(totals.total)
}

async fn load_customer<C: ConnectionTrait>(conn: &C, id: Uuid) -> Result<customer::Model> {
    customer::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("customer {id}")))
}

async fn load_note<C: ConnectionTrait>(conn: &C, id: Uuid) -> Result<credit_note::Model> {
    credit_note::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("credit note {id}")))
}

pub(crate) async fn load_note_locked(
    txn: &DatabaseTransaction,
    id: Uuid,
) -> Result<credit_note::Model> {
    credit_note::Entity::find_by_id(id)
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("credit note {id}")))
}

async fn load_note_lines<C: ConnectionTrait>(
    conn: &C,
    note_id: Uuid,
) -> Result<Vec<credit_note_line::Model>> {
    credit_note_line::Entity::find()
        .filter(credit_note_line::Column::CreditNoteId.eq(note_id))
        .order_by_asc(credit_note_line::Column::LineNo)
        .all(conn)
        .await
        .map_err(Error::from)
}

// ---------------------------------------------------------------------------
// Views (API DTOs)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CreditNoteLineView {
    pub id: Uuid,
    pub line_no: i32,
    pub invoice_line_id: Option<Uuid>,
    pub description: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub unit_price: Decimal,
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub discount_pct: Option<Decimal>,
    pub tax_code_id: Option<Uuid>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub net: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub tax: Decimal,
    pub restock: bool,
    pub restock_warehouse_id: Option<Uuid>,
    pub batch_no: Option<String>,
    pub serial_nos: Vec<String>,
    pub memo: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CreditNoteView {
    pub id: Uuid,
    pub number: Option<String>,
    pub customer_id: Uuid,
    pub customer_name: String,
    pub invoice_id: Uuid,
    pub invoice_number: Option<String>,
    #[schema(value_type = String, format = Date)]
    pub credit_date: chrono::NaiveDate,
    pub reason: String,
    pub currency: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub exchange_rate: Decimal,
    pub tax_inclusive: bool,
    pub memo: Option<String>,
    pub status: CreditNoteStatus,
    pub move_id: Option<Uuid>,
    pub cancel_reason: Option<String>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub subtotal: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub tax: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total: Decimal,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub posted_at: Option<chrono::DateTime<chrono::Utc>>,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub lines: Vec<CreditNoteLineView>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CreditNoteHeader {
    pub id: Uuid,
    pub number: Option<String>,
    pub customer_id: Uuid,
    pub customer_name: String,
    pub invoice_id: Uuid,
    pub invoice_number: Option<String>,
    #[schema(value_type = String, format = Date)]
    pub credit_date: chrono::NaiveDate,
    pub currency: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total: Decimal,
    pub status: CreditNoteStatus,
}

pub struct CreditNoteFilter {
    pub customer_id: Option<Uuid>,
    pub invoice_id: Option<Uuid>,
    pub status: Option<CreditNoteStatus>,
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreditNoteLineRequest {
    pub invoice_line_id: Uuid,
    pub description: Option<String>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    /// Defaults to the invoiced price.
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub unit_price: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub discount_pct: Option<Decimal>,
    pub tax_code_id: Option<Uuid>,
    /// Bring the goods back into stock (at the original issue cost).
    #[serde(default)]
    pub restock: bool,
    /// Required when restocking, unless the order line names a warehouse.
    pub restock_warehouse_id: Option<Uuid>,
    pub batch_no: Option<String>,
    pub serial_nos: Option<Vec<String>>,
    pub memo: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateCreditNoteRequest {
    pub invoice_id: Uuid,
    #[schema(value_type = String, format = Date)]
    pub credit_date: chrono::NaiveDate,
    pub reason: String,
    pub memo: Option<String>,
    pub lines: Vec<CreditNoteLineRequest>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CancelCreditNoteRequest {
    #[serde(default)]
    pub reason: String,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListCreditNotesQuery {
    pub customer_id: Option<Uuid>,
    pub invoice_id: Option<Uuid>,
    pub status: Option<CreditNoteStatus>,
}

fn new_note(req: CreateCreditNoteRequest, created_by: Option<Uuid>) -> NewCreditNote {
    NewCreditNote {
        invoice_id: req.invoice_id,
        credit_date: req.credit_date,
        reason: req.reason,
        memo: req.memo,
        lines: req
            .lines
            .into_iter()
            .map(|l| CreditNoteLineInput {
                invoice_line_id: l.invoice_line_id,
                description: l.description,
                qty: l.qty,
                unit_price: l.unit_price,
                discount_pct: l.discount_pct,
                tax_code_id: l.tax_code_id,
                restock: l.restock,
                restock_warehouse_id: l.restock_warehouse_id,
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
        .route("/sales/credit-notes", get(list_notes).post(create_note))
        .route(
            "/sales/credit-notes/{id}",
            get(get_note).put(update_note).delete(delete_note),
        )
        .route("/sales/credit-notes/{id}/post", post(post_note))
        .route("/sales/credit-notes/{id}/cancel", post(cancel_note))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    list_notes,
    get_note,
    create_note,
    update_note,
    delete_note,
    post_note,
    cancel_note
))]
struct ApiDoc;

#[utoipa::path(get, path = "/sales/credit-notes", tag = "sales",
    params(ListCreditNotesQuery),
    responses((status = 200, body = Vec<CreditNoteHeader>)))]
async fn list_notes(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<ListCreditNotesQuery>,
) -> Result<Json<Vec<CreditNoteHeader>>> {
    authz.require(names::CREDIT_NOTES_VIEW).await?;
    CreditNoteService::new(db)
        .list(CreditNoteFilter {
            customer_id: q.customer_id,
            invoice_id: q.invoice_id,
            status: q.status,
        })
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/sales/credit-notes/{id}", tag = "sales",
    params(("id" = Uuid, Path, description = "Credit note id")),
    responses((status = 200, body = CreditNoteView)))]
async fn get_note(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<CreditNoteView>> {
    authz.require(names::CREDIT_NOTES_VIEW).await?;
    CreditNoteService::new(db).view(id).await.map(Json)
}

#[utoipa::path(post, path = "/sales/credit-notes", tag = "sales",
    request_body = CreateCreditNoteRequest,
    responses((status = 200, body = CreditNoteView)))]
async fn create_note(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(req): Json<CreateCreditNoteRequest>,
) -> Result<Json<CreditNoteView>> {
    authz.require(names::CREDIT_NOTES_CREATE).await?;
    let view = CreditNoteService::new(db)
        .create_draft(new_note(req, Some(authz.user.id)))
        .await?;
    audit
        .0
        .created("scm.sales_credit_note", view.id, &view)
        .await;
    Ok(Json(view))
}

#[utoipa::path(put, path = "/sales/credit-notes/{id}", tag = "sales",
    params(("id" = Uuid, Path, description = "Credit note id")),
    request_body = CreateCreditNoteRequest,
    responses((status = 200, body = CreditNoteView)))]
async fn update_note(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<CreateCreditNoteRequest>,
) -> Result<Json<CreditNoteView>> {
    authz.require(names::CREDIT_NOTES_CREATE).await?;
    let service = CreditNoteService::new(db);
    let before = service.view(id).await?;
    let after = service.update_draft(id, new_note(req, None)).await?;
    audit
        .0
        .updated("scm.sales_credit_note", id, &before, &after)
        .await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/sales/credit-notes/{id}", tag = "sales",
    params(("id" = Uuid, Path, description = "Credit note id")),
    responses((status = 200, body = CreditNoteView)))]
async fn delete_note(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<CreditNoteView>> {
    authz.require(names::CREDIT_NOTES_CREATE).await?;
    let view = CreditNoteService::new(db).delete_draft(id).await?;
    audit
        .0
        .deleted("scm.sales_credit_note", view.id, &view)
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/sales/credit-notes/{id}/post", tag = "sales",
    params(("id" = Uuid, Path, description = "Credit note id")),
    responses((status = 200, body = CreditNoteView)))]
async fn post_note(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    CurrentTenant(tenant): CurrentTenant,
    Extension(numbering): Extension<Numbering>,
    Extension(events): Extension<Events>,
    Path(id): Path<Uuid>,
) -> Result<Json<CreditNoteView>> {
    authz.require(names::CREDIT_NOTES_POST).await?;
    let gl = gl::Gl::new(events, tenant.map(|t| t.id));
    let view = CreditNoteService::new(db)
        .post(id, &numbering, Some(authz.user.id), &gl)
        .await?;
    audit
        .0
        .event(format!(
            "posted credit note {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/sales/credit-notes/{id}/cancel", tag = "sales",
    params(("id" = Uuid, Path, description = "Credit note id")),
    request_body = CancelCreditNoteRequest,
    responses((status = 200, body = CreditNoteView)))]
async fn cancel_note(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    CurrentTenant(tenant): CurrentTenant,
    Extension(events): Extension<Events>,
    Path(id): Path<Uuid>,
    Json(req): Json<CancelCreditNoteRequest>,
) -> Result<Json<CreditNoteView>> {
    authz.require(names::CREDIT_NOTES_CANCEL).await?;
    let gl = gl::Gl::new(events, tenant.map(|t| t.id));
    let view = CreditNoteService::new(db)
        .cancel(id, &req.reason, Some(authz.user.id), &gl)
        .await?;
    audit
        .0
        .event(format!(
            "cancelled credit note {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}
