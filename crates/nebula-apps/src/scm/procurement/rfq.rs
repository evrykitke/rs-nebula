//! Requests for quotation: several suppliers price the same lines, one
//! wins the order.
//!
//! Lifecycle: draft → sent (numbered from the RFQ series, invitations
//! frozen) → closed (no more quotes) → awarded, cancellable until
//! awarded. Quotes are recorded per supplier per line while the RFQ is
//! sent — one row each, re-recording replaces it — so the view is the
//! comparison table. Awarding a supplier requires a quote on every line
//! and produces a *draft* purchase order at the quoted prices in the
//! supplier's currency; the buyer still reviews and submits it through
//! the normal approval gate. An RFQ can source an approved requisition
//! (lines copied on create, the requisition marked converted on award).
//! No stock or GL effects at any point.

use crate::scm::inventory::item::{item, uom};
use crate::scm::procurement::order::{NewOrder, OrderLineInput, OrderService};
use crate::scm::procurement::permissions::names;
use crate::scm::procurement::requisition::{
    self, RequisitionStatus, load_requisition_locked, requisition as requisition_entity,
};
use crate::scm::procurement::supplier::supplier;
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
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Where an RFQ is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RfqStatus {
    Draft,
    Sent,
    Closed,
    Awarded,
    Cancelled,
}

impl RfqStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RfqStatus::Draft => "draft",
            RfqStatus::Sent => "sent",
            RfqStatus::Closed => "closed",
            RfqStatus::Awarded => "awarded",
            RfqStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "draft" => Ok(RfqStatus::Draft),
            "sent" => Ok(RfqStatus::Sent),
            "closed" => Ok(RfqStatus::Closed),
            "awarded" => Ok(RfqStatus::Awarded),
            "cancelled" => Ok(RfqStatus::Cancelled),
            other => Err(Error::internal(format!("unknown rfq status {other:?}"))),
        }
    }
}

/// The RFQ header.
pub mod rfq {
    use nebula::sea_orm;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "procurement_rfqs")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub number: Option<String>,
        pub title: String,
        pub due_date: Option<Date>,
        pub memo: Option<String>,
        pub status: String,
        pub requisition_id: Option<Uuid>,
        pub awarded_supplier_id: Option<Uuid>,
        pub order_id: Option<Uuid>,
        pub sent_at: Option<DateTimeUtc>,
        pub created_at: DateTimeUtc,
        pub created_by: Option<Uuid>,
        pub updated_at: DateTimeUtc,
        pub updated_by: Option<Uuid>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// One line suppliers are asked to price.
pub mod rfq_line {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "procurement_rfq_lines")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub rfq_id: Uuid,
        pub line_no: i32,
        pub item_id: Uuid,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub qty: Decimal,
        pub memo: Option<String>,
        pub created_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// Which suppliers were asked.
pub mod rfq_supplier {
    use nebula::sea_orm;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "procurement_rfq_suppliers")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub rfq_id: Uuid,
        #[sea_orm(primary_key, auto_increment = false)]
        pub supplier_id: Uuid,
        pub invited_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// One supplier's price for one line; re-recording replaces it.
pub mod rfq_quote {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "procurement_rfq_quotes")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub rfq_id: Uuid,
        pub rfq_line_id: Uuid,
        pub supplier_id: Uuid,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))")]
        pub unit_price: Decimal,
        pub lead_time_days: Option<i32>,
        #[sea_orm(column_type = "Decimal(Some((20, 6)))", nullable)]
        pub min_qty: Option<Decimal>,
        pub notes: Option<String>,
        pub quoted_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// An RFQ line as supplied by a caller.
pub struct RfqLineInput {
    pub item_id: Uuid,
    pub qty: Decimal,
    pub memo: Option<String>,
}

/// A new draft RFQ as supplied by a caller. When `requisition_id` is set
/// and `lines` is empty, the requisition's lines are copied in.
pub struct NewRfq {
    pub title: String,
    pub due_date: Option<chrono::NaiveDate>,
    pub memo: Option<String>,
    pub requisition_id: Option<Uuid>,
    pub supplier_ids: Vec<Uuid>,
    pub lines: Vec<RfqLineInput>,
    pub created_by: Option<Uuid>,
}

/// One supplier's quote for one line, as recorded by a buyer.
pub struct QuoteInput {
    pub rfq_line_id: Uuid,
    pub unit_price: Decimal,
    pub lead_time_days: Option<i32>,
    pub min_qty: Option<Decimal>,
    pub notes: Option<String>,
}

/// The RFQ service over one (tenant) connection.
pub struct RfqService {
    db: DatabaseConnection,
}

impl RfqService {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Create a draft RFQ.
    pub async fn create_draft(&self, mut new: NewRfq) -> Result<RfqView> {
        if let (Some(requisition_id), true) = (new.requisition_id, new.lines.is_empty()) {
            new.lines = requisition::load_lines(&self.db, requisition_id)
                .await?
                .into_iter()
                .map(|l| RfqLineInput {
                    item_id: l.item_id,
                    qty: l.qty,
                    memo: l.memo,
                })
                .collect();
        }
        validate_rfq(&self.db, &new).await?;
        let txn = self.db.begin().await?;
        let rfq_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        rfq::ActiveModel {
            id: Set(rfq_id),
            number: Set(None),
            title: Set(new.title.trim().to_string()),
            due_date: Set(new.due_date),
            memo: Set(clean(new.memo)),
            status: Set(RfqStatus::Draft.as_str().to_string()),
            requisition_id: Set(new.requisition_id),
            awarded_supplier_id: Set(None),
            order_id: Set(None),
            sent_at: Set(None),
            created_at: Set(now),
            created_by: Set(new.created_by),
            updated_at: Set(now),
            updated_by: Set(None),
        }
        .insert(&txn)
        .await?;
        insert_lines(&txn, rfq_id, &new.lines).await?;
        insert_suppliers(&txn, rfq_id, &new.supplier_ids).await?;
        txn.commit().await?;
        self.view(rfq_id).await
    }

    /// Replace a draft's header, lines and invitations wholesale.
    pub async fn update_draft(&self, id: Uuid, new: NewRfq) -> Result<RfqView> {
        validate_rfq(&self.db, &new).await?;
        let txn = self.db.begin().await?;
        let existing = load_rfq_locked(&txn, id).await?;
        if RfqStatus::parse(&existing.status)? != RfqStatus::Draft {
            return Err(Error::Validation("only a draft RFQ can be edited".into()));
        }
        rfq_line::Entity::delete_many()
            .filter(rfq_line::Column::RfqId.eq(id))
            .exec(&txn)
            .await?;
        rfq_supplier::Entity::delete_many()
            .filter(rfq_supplier::Column::RfqId.eq(id))
            .exec(&txn)
            .await?;
        insert_lines(&txn, id, &new.lines).await?;
        insert_suppliers(&txn, id, &new.supplier_ids).await?;
        let mut active: rfq::ActiveModel = existing.into();
        active.title = Set(new.title.trim().to_string());
        active.due_date = Set(new.due_date);
        active.memo = Set(clean(new.memo));
        active.requisition_id = Set(new.requisition_id);
        active.updated_at = Set(chrono::Utc::now());
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Delete a draft (lines, invitations and quotes cascade).
    pub async fn delete_draft(&self, id: Uuid) -> Result<RfqView> {
        let view = self.view(id).await?;
        let txn = self.db.begin().await?;
        let existing = load_rfq_locked(&txn, id).await?;
        if RfqStatus::parse(&existing.status)? != RfqStatus::Draft {
            return Err(Error::Validation("only a draft RFQ can be deleted".into()));
        }
        rfq::Entity::delete_by_id(id).exec(&txn).await?;
        txn.commit().await?;
        Ok(view)
    }

    /// Send the RFQ to its invited suppliers: the RFQ number is allocated
    /// and quotes can be recorded from here.
    pub async fn send(&self, id: Uuid, numbering: &Numbering, by: Option<Uuid>) -> Result<RfqView> {
        let txn = self.db.begin().await?;
        let existing = load_rfq_locked(&txn, id).await?;
        if RfqStatus::parse(&existing.status)? != RfqStatus::Draft {
            return Err(Error::Validation("only a draft RFQ can be sent".into()));
        }
        let lines = load_lines(&txn, id).await?;
        if lines.is_empty() {
            return Err(Error::Validation("an RFQ needs at least one line".into()));
        }
        let invited = load_suppliers(&txn, id).await?;
        if invited.is_empty() {
            return Err(Error::Validation(
                "an RFQ needs at least one invited supplier".into(),
            ));
        }
        let number = numbering.next(&txn, crate::scm::RFQ_SERIES).await?;
        let now = chrono::Utc::now();
        let mut active: rfq::ActiveModel = existing.into();
        active.number = Set(Some(number.formatted));
        active.status = Set(RfqStatus::Sent.as_str().to_string());
        active.sent_at = Set(Some(now));
        active.updated_at = Set(now);
        active.updated_by = Set(by);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Record (or replace) one invited supplier's quotes while the RFQ is
    /// out.
    pub async fn record_quotes(
        &self,
        id: Uuid,
        supplier_id: Uuid,
        quotes: Vec<QuoteInput>,
    ) -> Result<RfqView> {
        if quotes.is_empty() {
            return Err(Error::Validation("no quotes to record".into()));
        }
        let txn = self.db.begin().await?;
        let existing = load_rfq_locked(&txn, id).await?;
        if RfqStatus::parse(&existing.status)? != RfqStatus::Sent {
            return Err(Error::Validation(
                "quotes can only be recorded on a sent RFQ".into(),
            ));
        }
        let invited: HashSet<Uuid> = load_suppliers(&txn, id)
            .await?
            .into_iter()
            .map(|s| s.supplier_id)
            .collect();
        if !invited.contains(&supplier_id) {
            return Err(Error::Validation(
                "the supplier was not invited to this RFQ".into(),
            ));
        }
        let line_ids: HashSet<Uuid> = load_lines(&txn, id)
            .await?
            .into_iter()
            .map(|l| l.id)
            .collect();
        let now = chrono::Utc::now();
        for q in &quotes {
            if !line_ids.contains(&q.rfq_line_id) {
                return Err(Error::Validation(format!(
                    "line {} does not belong to this RFQ",
                    q.rfq_line_id
                )));
            }
            if q.unit_price < Decimal::ZERO {
                return Err(Error::Validation(
                    "a quoted price must not be negative".into(),
                ));
            }
            let existing_quote = rfq_quote::Entity::find()
                .filter(rfq_quote::Column::RfqLineId.eq(q.rfq_line_id))
                .filter(rfq_quote::Column::SupplierId.eq(supplier_id))
                .one(&txn)
                .await?;
            match existing_quote {
                Some(row) => {
                    let mut active: rfq_quote::ActiveModel = row.into();
                    active.unit_price = Set(q.unit_price);
                    active.lead_time_days = Set(q.lead_time_days);
                    active.min_qty = Set(q.min_qty);
                    active.notes = Set(q.notes.clone().filter(|n| !n.trim().is_empty()));
                    active.quoted_at = Set(now);
                    active.update(&txn).await?;
                }
                None => {
                    rfq_quote::ActiveModel {
                        id: Set(Uuid::new_v4()),
                        rfq_id: Set(id),
                        rfq_line_id: Set(q.rfq_line_id),
                        supplier_id: Set(supplier_id),
                        unit_price: Set(q.unit_price),
                        lead_time_days: Set(q.lead_time_days),
                        min_qty: Set(q.min_qty),
                        notes: Set(q.notes.clone().filter(|n| !n.trim().is_empty())),
                        quoted_at: Set(now),
                    }
                    .insert(&txn)
                    .await?;
                }
            }
        }
        txn.commit().await?;
        self.view(id).await
    }

    /// Stop accepting quotes; awarding stays possible.
    pub async fn close(&self, id: Uuid, by: Option<Uuid>) -> Result<RfqView> {
        let txn = self.db.begin().await?;
        let existing = load_rfq_locked(&txn, id).await?;
        if RfqStatus::parse(&existing.status)? != RfqStatus::Sent {
            return Err(Error::Validation("only a sent RFQ can be closed".into()));
        }
        let mut active: rfq::ActiveModel = existing.into();
        active.status = Set(RfqStatus::Closed.as_str().to_string());
        active.updated_at = Set(chrono::Utc::now());
        active.updated_by = Set(by);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Cancel an RFQ that has not been awarded.
    pub async fn cancel(&self, id: Uuid, by: Option<Uuid>) -> Result<RfqView> {
        let txn = self.db.begin().await?;
        let existing = load_rfq_locked(&txn, id).await?;
        let status = RfqStatus::parse(&existing.status)?;
        if !matches!(
            status,
            RfqStatus::Draft | RfqStatus::Sent | RfqStatus::Closed
        ) {
            return Err(Error::Validation(format!(
                "a {} RFQ cannot be cancelled",
                status.as_str()
            )));
        }
        let mut active: rfq::ActiveModel = existing.into();
        active.status = Set(RfqStatus::Cancelled.as_str().to_string());
        active.updated_at = Set(chrono::Utc::now());
        active.updated_by = Set(by);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Award the RFQ to one supplier: they must have quoted every line,
    /// and the result is a draft purchase order at the quoted prices in
    /// the supplier's currency. The delivery warehouse comes from the
    /// request or the linked requisition; an approved linked requisition
    /// is marked converted by the same order.
    pub async fn award(
        &self,
        id: Uuid,
        supplier_id: Uuid,
        warehouse_id: Option<Uuid>,
        by: Option<Uuid>,
    ) -> Result<RfqView> {
        let existing = load_rfq(&self.db, id).await?;
        let status = RfqStatus::parse(&existing.status)?;
        if !matches!(status, RfqStatus::Sent | RfqStatus::Closed) {
            return Err(Error::Validation(format!(
                "a {} RFQ cannot be awarded",
                status.as_str()
            )));
        }
        let invited: HashSet<Uuid> = load_suppliers(&self.db, id)
            .await?
            .into_iter()
            .map(|s| s.supplier_id)
            .collect();
        if !invited.contains(&supplier_id) {
            return Err(Error::Validation(
                "the supplier was not invited to this RFQ".into(),
            ));
        }
        let lines = load_lines(&self.db, id).await?;
        let quotes: HashMap<Uuid, rfq_quote::Model> = rfq_quote::Entity::find()
            .filter(rfq_quote::Column::RfqId.eq(id))
            .filter(rfq_quote::Column::SupplierId.eq(supplier_id))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|q| (q.rfq_line_id, q))
            .collect();

        let linked_requisition = match existing.requisition_id {
            Some(rid) => {
                requisition_entity::Entity::find_by_id(rid)
                    .one(&self.db)
                    .await?
            }
            None => None,
        };
        let deliver_to = warehouse_id
            .or(linked_requisition.as_ref().map(|r| r.warehouse_id))
            .ok_or_else(|| {
                Error::Validation(
                    "name the delivery warehouse — the RFQ has no linked requisition to take it from"
                        .into(),
                )
            })?;

        let mut order_lines = Vec::with_capacity(lines.len());
        for line in &lines {
            let quote = quotes.get(&line.id).ok_or_else(|| {
                Error::Validation(format!(
                    "line {} has no quote from this supplier — record it or award another",
                    line.line_no
                ))
            })?;
            order_lines.push(OrderLineInput {
                item_id: line.item_id,
                description: None,
                qty: line.qty,
                unit_price: quote.unit_price,
                discount_pct: None,
                tax_code_id: None,
                expected_date: quote
                    .lead_time_days
                    .map(|d| chrono::Utc::now().date_naive() + chrono::Duration::days(d as i64)),
                memo: line.memo.clone(),
            });
        }

        let order_service = OrderService::new(self.db.clone());
        let order = order_service
            .create_draft(NewOrder {
                supplier_id,
                order_date: chrono::Utc::now().date_naive(),
                expected_date: existing.due_date,
                deliver_to_warehouse_id: deliver_to,
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
                memo: existing
                    .number
                    .as_deref()
                    .map(|n| format!("Awarded from {n}")),
                reference: existing.number.clone(),
                terms_and_conditions: None,
                lines: order_lines,
                created_by: by,
            })
            .await?;

        // The order commits in its own transaction; re-check under the row
        // lock so two awards cannot both win — the loser's draft order is
        // removed again.
        let txn = self.db.begin().await?;
        let current = load_rfq_locked(&txn, id).await?;
        if !matches!(
            RfqStatus::parse(&current.status)?,
            RfqStatus::Sent | RfqStatus::Closed
        ) {
            txn.rollback().await?;
            order_service.delete_draft(order.id).await.ok();
            return Err(Error::Validation(
                "the RFQ was awarded or cancelled by someone else".into(),
            ));
        }
        let now = chrono::Utc::now();
        if let Some(req_row) = &current.requisition_id {
            let req_row = load_requisition_locked(&txn, *req_row).await?;
            if RequisitionStatus::parse(&req_row.status)? == RequisitionStatus::Approved {
                let mut active: requisition_entity::ActiveModel = req_row.into();
                active.status = Set(RequisitionStatus::Converted.as_str().to_string());
                active.order_id = Set(Some(order.id));
                active.updated_at = Set(now);
                active.updated_by = Set(by);
                active.update(&txn).await?;
            }
        }
        let mut active: rfq::ActiveModel = current.into();
        active.status = Set(RfqStatus::Awarded.as_str().to_string());
        active.awarded_supplier_id = Set(Some(supplier_id));
        active.order_id = Set(Some(order.id));
        active.updated_at = Set(now);
        active.updated_by = Set(by);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    pub async fn list(&self, filter: RfqFilter) -> Result<Vec<RfqHeader>> {
        let mut query = rfq::Entity::find();
        if let Some(s) = filter.status {
            query = query.filter(rfq::Column::Status.eq(s.as_str()));
        }
        let rows = query
            .order_by_desc(rfq::Column::CreatedAt)
            .all(&self.db)
            .await?;
        rows.into_iter()
            .map(|r| {
                Ok(RfqHeader {
                    id: r.id,
                    number: r.number.clone(),
                    title: r.title.clone(),
                    due_date: r.due_date,
                    status: RfqStatus::parse(&r.status)?,
                })
            })
            .collect()
    }

    /// Load a full RFQ: lines with their quotes (the comparison table),
    /// invited suppliers, labels.
    pub async fn view(&self, id: Uuid) -> Result<RfqView> {
        let row = load_rfq(&self.db, id).await?;
        let lines = load_lines(&self.db, id).await?;
        let invited = load_suppliers(&self.db, id).await?;
        let quotes = rfq_quote::Entity::find()
            .filter(rfq_quote::Column::RfqId.eq(id))
            .order_by_asc(rfq_quote::Column::QuotedAt)
            .all(&self.db)
            .await?;

        let supplier_ids: Vec<Uuid> = invited.iter().map(|s| s.supplier_id).collect();
        let suppliers: HashMap<Uuid, supplier::Model> = supplier::Entity::find()
            .filter(supplier::Column::Id.is_in(supplier_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|s| (s.id, s))
            .collect();
        let item_ids: Vec<Uuid> = lines.iter().map(|l| l.item_id).collect();
        let items: HashMap<Uuid, item::Model> = item::Entity::find()
            .filter(item::Column::Id.is_in(item_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|i| (i.id, i))
            .collect();
        let uom_ids: Vec<Uuid> = items.values().map(|i| i.uom_id).collect();
        let uoms: HashMap<Uuid, uom::Model> = uom::Entity::find()
            .filter(uom::Column::Id.is_in(uom_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|u| (u.id, u))
            .collect();
        let requisition_number = match row.requisition_id {
            Some(rid) => requisition_entity::Entity::find_by_id(rid)
                .one(&self.db)
                .await?
                .and_then(|r| r.number),
            None => None,
        };
        let order_number = match row.order_id {
            Some(order_id) => crate::scm::procurement::order::order::Entity::find_by_id(order_id)
                .one(&self.db)
                .await?
                .and_then(|o| o.number),
            None => None,
        };

        let mut quotes_by_line: HashMap<Uuid, Vec<RfqQuoteView>> = HashMap::new();
        for q in quotes {
            quotes_by_line
                .entry(q.rfq_line_id)
                .or_default()
                .push(RfqQuoteView {
                    supplier_id: q.supplier_id,
                    supplier_name: suppliers
                        .get(&q.supplier_id)
                        .map(|s| s.name.clone())
                        .unwrap_or_default(),
                    unit_price: q.unit_price,
                    lead_time_days: q.lead_time_days,
                    min_qty: q.min_qty,
                    notes: q.notes,
                    quoted_at: q.quoted_at,
                });
        }

        let line_views = lines
            .into_iter()
            .map(|l| {
                let item = items.get(&l.item_id);
                let uom_code = item
                    .and_then(|i| uoms.get(&i.uom_id))
                    .map(|u| u.code.clone())
                    .unwrap_or_default();
                RfqLineView {
                    id: l.id,
                    line_no: l.line_no,
                    item_id: l.item_id,
                    sku: item.map(|i| i.sku.clone()).unwrap_or_default(),
                    item_name: item.map(|i| i.name.clone()).unwrap_or_default(),
                    uom_code,
                    qty: l.qty,
                    memo: l.memo,
                    quotes: quotes_by_line.remove(&l.id).unwrap_or_default(),
                }
            })
            .collect();
        let supplier_views = invited
            .into_iter()
            .map(|s| {
                let label = suppliers.get(&s.supplier_id);
                RfqSupplierView {
                    supplier_id: s.supplier_id,
                    code: label.map(|l| l.code.clone()).unwrap_or_default(),
                    name: label.map(|l| l.name.clone()).unwrap_or_default(),
                    invited_at: s.invited_at,
                }
            })
            .collect();

        Ok(RfqView {
            id: row.id,
            number: row.number,
            title: row.title,
            due_date: row.due_date,
            memo: row.memo,
            status: RfqStatus::parse(&row.status)?,
            requisition_id: row.requisition_id,
            requisition_number,
            awarded_supplier_id: row.awarded_supplier_id,
            order_id: row.order_id,
            order_number,
            sent_at: row.sent_at,
            created_at: row.created_at,
            suppliers: supplier_views,
            lines: line_views,
        })
    }
}

fn clean(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// Draft-time validation: a title, at least one valid line, invited
/// suppliers active, the linked requisition (if any) real.
async fn validate_rfq<C: ConnectionTrait>(conn: &C, new: &NewRfq) -> Result<()> {
    if new.title.trim().is_empty() {
        return Err(Error::Validation("an RFQ needs a title".into()));
    }
    if new.lines.is_empty() {
        return Err(Error::Validation("an RFQ needs at least one line".into()));
    }
    if let Some(rid) = new.requisition_id {
        requisition_entity::Entity::find_by_id(rid)
            .one(conn)
            .await?
            .ok_or_else(|| Error::NotFound(format!("purchase requisition {rid}")))?;
    }
    let mut seen = HashSet::new();
    for supplier_id in &new.supplier_ids {
        if !seen.insert(*supplier_id) {
            return Err(Error::Validation("a supplier is invited twice".into()));
        }
        let found = supplier::Entity::find_by_id(*supplier_id).one(conn).await?;
        let Some(found) = found else {
            return Err(Error::NotFound(format!("supplier {supplier_id}")));
        };
        if !found.is_active {
            return Err(Error::Validation(format!(
                "supplier {} is inactive",
                found.code
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
    rfq_id: Uuid,
    lines: &[RfqLineInput],
) -> Result<()> {
    let now = chrono::Utc::now();
    for (i, l) in lines.iter().enumerate() {
        rfq_line::ActiveModel {
            id: Set(Uuid::new_v4()),
            rfq_id: Set(rfq_id),
            line_no: Set((i + 1) as i32),
            item_id: Set(l.item_id),
            qty: Set(l.qty),
            memo: Set(l.memo.clone().filter(|m| !m.trim().is_empty())),
            created_at: Set(now),
        }
        .insert(conn)
        .await?;
    }
    Ok(())
}

async fn insert_suppliers<C: ConnectionTrait>(
    conn: &C,
    rfq_id: Uuid,
    supplier_ids: &[Uuid],
) -> Result<()> {
    let now = chrono::Utc::now();
    for supplier_id in supplier_ids {
        rfq_supplier::ActiveModel {
            rfq_id: Set(rfq_id),
            supplier_id: Set(*supplier_id),
            invited_at: Set(now),
        }
        .insert(conn)
        .await?;
    }
    Ok(())
}

async fn load_rfq<C: ConnectionTrait>(conn: &C, id: Uuid) -> Result<rfq::Model> {
    rfq::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("RFQ {id}")))
}

async fn load_rfq_locked(txn: &DatabaseTransaction, id: Uuid) -> Result<rfq::Model> {
    rfq::Entity::find_by_id(id)
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("RFQ {id}")))
}

async fn load_lines<C: ConnectionTrait>(conn: &C, rfq_id: Uuid) -> Result<Vec<rfq_line::Model>> {
    rfq_line::Entity::find()
        .filter(rfq_line::Column::RfqId.eq(rfq_id))
        .order_by_asc(rfq_line::Column::LineNo)
        .all(conn)
        .await
        .map_err(Error::from)
}

async fn load_suppliers<C: ConnectionTrait>(
    conn: &C,
    rfq_id: Uuid,
) -> Result<Vec<rfq_supplier::Model>> {
    rfq_supplier::Entity::find()
        .filter(rfq_supplier::Column::RfqId.eq(rfq_id))
        .all(conn)
        .await
        .map_err(Error::from)
}

// ---------------------------------------------------------------------------
// Views (API DTOs)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RfqQuoteView {
    pub supplier_id: Uuid,
    pub supplier_name: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub unit_price: Decimal,
    pub lead_time_days: Option<i32>,
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub min_qty: Option<Decimal>,
    pub notes: Option<String>,
    #[schema(value_type = String, format = DateTime)]
    pub quoted_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RfqLineView {
    pub id: Uuid,
    pub line_no: i32,
    pub item_id: Uuid,
    pub sku: String,
    pub item_name: String,
    /// The item's stocking unit of measure (code), for display.
    pub uom_code: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    pub memo: Option<String>,
    pub quotes: Vec<RfqQuoteView>,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RfqSupplierView {
    pub supplier_id: Uuid,
    pub code: String,
    pub name: String,
    #[schema(value_type = String, format = DateTime)]
    pub invited_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RfqView {
    pub id: Uuid,
    pub number: Option<String>,
    pub title: String,
    #[schema(value_type = Option<String>, format = Date)]
    pub due_date: Option<chrono::NaiveDate>,
    pub memo: Option<String>,
    pub status: RfqStatus,
    pub requisition_id: Option<Uuid>,
    pub requisition_number: Option<String>,
    pub awarded_supplier_id: Option<Uuid>,
    /// The draft purchase order the award produced.
    pub order_id: Option<Uuid>,
    pub order_number: Option<String>,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub sent_at: Option<chrono::DateTime<chrono::Utc>>,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub suppliers: Vec<RfqSupplierView>,
    pub lines: Vec<RfqLineView>,
}

/// A row of the RFQ register.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RfqHeader {
    pub id: Uuid,
    pub number: Option<String>,
    pub title: String,
    #[schema(value_type = Option<String>, format = Date)]
    pub due_date: Option<chrono::NaiveDate>,
    pub status: RfqStatus,
}

pub struct RfqFilter {
    pub status: Option<RfqStatus>,
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
pub struct RfqLineRequest {
    pub item_id: Uuid,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    pub memo: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateRfqRequest {
    pub title: String,
    #[schema(value_type = Option<String>, format = Date)]
    pub due_date: Option<chrono::NaiveDate>,
    pub memo: Option<String>,
    /// Copy this requisition's lines when `lines` is empty.
    pub requisition_id: Option<Uuid>,
    #[serde(default)]
    pub supplier_ids: Vec<Uuid>,
    #[serde(default)]
    pub lines: Vec<RfqLineRequest>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct QuoteRequest {
    pub rfq_line_id: Uuid,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub unit_price: Decimal,
    pub lead_time_days: Option<i32>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub min_qty: Option<Decimal>,
    pub notes: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct RecordQuotesRequest {
    pub supplier_id: Uuid,
    pub quotes: Vec<QuoteRequest>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct AwardRfqRequest {
    pub supplier_id: Uuid,
    /// Defaults from the linked requisition's warehouse.
    pub warehouse_id: Option<Uuid>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListRfqsQuery {
    pub status: Option<RfqStatus>,
}

fn new_rfq(req: CreateRfqRequest, created_by: Option<Uuid>) -> NewRfq {
    NewRfq {
        title: req.title,
        due_date: req.due_date,
        memo: req.memo,
        requisition_id: req.requisition_id,
        supplier_ids: req.supplier_ids,
        lines: req
            .lines
            .into_iter()
            .map(|l| RfqLineInput {
                item_id: l.item_id,
                qty: l.qty,
                memo: l.memo,
            })
            .collect(),
        created_by,
    }
}

pub(crate) fn routes() -> Router {
    Router::new()
        .route("/procurement/rfqs", get(list_rfqs).post(create_rfq))
        .route(
            "/procurement/rfqs/{id}",
            get(get_rfq).put(update_rfq).delete(delete_rfq),
        )
        .route("/procurement/rfqs/{id}/send", post(send_rfq))
        .route("/procurement/rfqs/{id}/quotes", post(record_quotes))
        .route("/procurement/rfqs/{id}/close", post(close_rfq))
        .route("/procurement/rfqs/{id}/cancel", post(cancel_rfq))
        .route("/procurement/rfqs/{id}/award", post(award_rfq))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    list_rfqs,
    get_rfq,
    create_rfq,
    update_rfq,
    delete_rfq,
    send_rfq,
    record_quotes,
    close_rfq,
    cancel_rfq,
    award_rfq
))]
struct ApiDoc;

#[utoipa::path(get, path = "/procurement/rfqs", tag = "procurement",
    params(ListRfqsQuery),
    responses((status = 200, body = Vec<RfqHeader>)))]
async fn list_rfqs(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<ListRfqsQuery>,
) -> Result<Json<Vec<RfqHeader>>> {
    authz.require(names::RFQS_VIEW).await?;
    RfqService::new(db)
        .list(RfqFilter { status: q.status })
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/procurement/rfqs/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "RFQ id")),
    responses((status = 200, body = RfqView)))]
async fn get_rfq(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<RfqView>> {
    authz.require(names::RFQS_VIEW).await?;
    RfqService::new(db).view(id).await.map(Json)
}

#[utoipa::path(post, path = "/procurement/rfqs", tag = "procurement",
    request_body = CreateRfqRequest,
    responses((status = 200, body = RfqView)))]
async fn create_rfq(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(req): Json<CreateRfqRequest>,
) -> Result<Json<RfqView>> {
    authz.require(names::RFQS_CREATE).await?;
    let view = RfqService::new(db)
        .create_draft(new_rfq(req, Some(authz.user.id)))
        .await?;
    audit.0.created("scm.rfq", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(put, path = "/procurement/rfqs/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "RFQ id")),
    request_body = CreateRfqRequest,
    responses((status = 200, body = RfqView)))]
async fn update_rfq(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<CreateRfqRequest>,
) -> Result<Json<RfqView>> {
    authz.require(names::RFQS_CREATE).await?;
    let service = RfqService::new(db);
    let before = service.view(id).await?;
    let after = service.update_draft(id, new_rfq(req, None)).await?;
    audit.0.updated("scm.rfq", id, &before, &after).await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/procurement/rfqs/{id}", tag = "procurement",
    params(("id" = Uuid, Path, description = "RFQ id")),
    responses((status = 200, body = RfqView)))]
async fn delete_rfq(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<RfqView>> {
    authz.require(names::RFQS_CREATE).await?;
    let view = RfqService::new(db).delete_draft(id).await?;
    audit.0.deleted("scm.rfq", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/rfqs/{id}/send", tag = "procurement",
    params(("id" = Uuid, Path, description = "RFQ id")),
    responses((status = 200, body = RfqView)))]
async fn send_rfq(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Extension(numbering): Extension<Numbering>,
    Path(id): Path<Uuid>,
) -> Result<Json<RfqView>> {
    authz.require(names::RFQS_SEND).await?;
    let view = RfqService::new(db)
        .send(id, &numbering, Some(authz.user.id))
        .await?;
    audit
        .0
        .event(format!("sent RFQ {}", view.number.as_deref().unwrap_or("")))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/rfqs/{id}/quotes", tag = "procurement",
    params(("id" = Uuid, Path, description = "RFQ id")),
    request_body = RecordQuotesRequest,
    responses((status = 200, body = RfqView)))]
async fn record_quotes(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<RecordQuotesRequest>,
) -> Result<Json<RfqView>> {
    authz.require(names::RFQS_RECORD_QUOTES).await?;
    let supplier_id = req.supplier_id;
    let view = RfqService::new(db)
        .record_quotes(
            id,
            supplier_id,
            req.quotes
                .into_iter()
                .map(|q| QuoteInput {
                    rfq_line_id: q.rfq_line_id,
                    unit_price: q.unit_price,
                    lead_time_days: q.lead_time_days,
                    min_qty: q.min_qty,
                    notes: q.notes,
                })
                .collect(),
        )
        .await?;
    audit
        .0
        .event(format!(
            "recorded quotes on RFQ {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/rfqs/{id}/close", tag = "procurement",
    params(("id" = Uuid, Path, description = "RFQ id")),
    responses((status = 200, body = RfqView)))]
async fn close_rfq(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<RfqView>> {
    authz.require(names::RFQS_SEND).await?;
    let view = RfqService::new(db).close(id, Some(authz.user.id)).await?;
    audit
        .0
        .event(format!(
            "closed RFQ {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/rfqs/{id}/cancel", tag = "procurement",
    params(("id" = Uuid, Path, description = "RFQ id")),
    responses((status = 200, body = RfqView)))]
async fn cancel_rfq(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<RfqView>> {
    authz.require(names::RFQS_CREATE).await?;
    let view = RfqService::new(db).cancel(id, Some(authz.user.id)).await?;
    audit
        .0
        .event(format!(
            "cancelled RFQ {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/procurement/rfqs/{id}/award", tag = "procurement",
    params(("id" = Uuid, Path, description = "RFQ id")),
    request_body = AwardRfqRequest,
    responses((status = 200, body = RfqView)))]
async fn award_rfq(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<AwardRfqRequest>,
) -> Result<Json<RfqView>> {
    authz.require(names::RFQS_AWARD).await?;
    let view = RfqService::new(db)
        .award(id, req.supplier_id, req.warehouse_id, Some(authz.user.id))
        .await?;
    audit
        .0
        .event(format!(
            "awarded RFQ {} to supplier, purchase order {}",
            view.number.as_deref().unwrap_or(""),
            view.order_number.as_deref().unwrap_or("(draft)")
        ))
        .await;
    Ok(Json(view))
}
