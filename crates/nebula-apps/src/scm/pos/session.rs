//! Sessions: the unit of POS accountability. One cashier's span at one
//! register — opening float → sales → paid-in/out → closing count →
//! over/short — and nothing sells outside an open one (a partial unique
//! index holds the one-open-session-per-register rule even under race).
//!
//! Closing is where POS meets the books, in two deliberate steps:
//!
//! 1. **Counting** (status → `closing`): refuse while the till still
//!    holds unsynced sales, store expected-vs-counted per tender, demand
//!    a note for any variance.
//! 2. **Consolidation** (status → `closed`): aggregate every captured
//!    line net of refunds per item × batch, post **one** issue movement
//!    through the stock engine (source `pos.session:{id}` — a whole day
//!    of selling is one ledger touch per item), and stage **one** revenue
//!    GL request in the scm outbox: Dr cash (counted effect) / Dr
//!    M-Pesa clearing / Dr card clearing ± cash-over-short / Cr sales /
//!    Cr VAT output. COGS rides on the movement itself, as every issue.
//!
//! A failure in step 2 leaves the session `closing` — sales blocked,
//! retryable, nothing half-posted (the step is one transaction). The Z
//! report reads the stored counts, so it stays true forever.

use crate::scm::gl;
use crate::scm::inventory::batch::batch;
use crate::scm::inventory::item::{ItemType, item, uom};
use crate::scm::inventory::moves::{MoveStatus, MoveType, doc as move_doc, line as move_line};
use crate::scm::inventory::stock::{
    Movement, StockService, level_average, lock_or_init_level, round_money,
};
use crate::scm::pos::permissions::names;
use crate::scm::pos::register;
use crate::scm::pos::sale::{OrderKind, OrderStatus, TENDERS, order, order_line, order_payment};
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
    SqlErr, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Where a session is in its life. `Closing` is a real resting state:
/// counting succeeded but consolidation has not yet — sales stay blocked
/// and the close is retryable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = PosSessionStatus)]
pub enum SessionStatus {
    Open,
    Closing,
    Closed,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Open => "open",
            SessionStatus::Closing => "closing",
            SessionStatus::Closed => "closed",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "open" => Ok(SessionStatus::Open),
            "closing" => Ok(SessionStatus::Closing),
            "closed" => Ok(SessionStatus::Closed),
            other => Err(Error::internal(format!(
                "unknown pos session status {other:?}"
            ))),
        }
    }
}

/// The session row.
pub mod session {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "pos_sessions")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub number: Option<String>,
        pub register_id: Uuid,
        pub cashier_id: Uuid,
        pub status: String,
        pub opened_at: DateTimeUtc,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))")]
        pub opening_float: Decimal,
        pub closed_at: Option<DateTimeUtc>,
        pub closed_by: Option<Uuid>,
        pub closing_note: Option<String>,
        pub move_id: Option<Uuid>,
        pub gl_source: Option<String>,
        pub created_at: DateTimeUtc,
        pub updated_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// Expected vs counted per tender, written once at close.
pub mod session_count {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "pos_session_counts")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub session_id: Uuid,
        pub tender: String,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))")]
        pub expected: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))")]
        pub counted: Decimal,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// A paid-in / paid-out drawer event. kind: paid_in | paid_out.
pub mod cash_movement {
    use nebula::sea_orm;
    use rust_decimal::Decimal;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "pos_cash_movements")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub session_id: Uuid,
        pub kind: String,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))")]
        pub amount: Decimal,
        pub reason: String,
        pub created_at: DateTimeUtc,
        pub created_by: Option<Uuid>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

pub struct CountInput {
    pub tender: String,
    pub counted: Decimal,
}

pub struct SessionService {
    db: DatabaseConnection,
}

impl SessionService {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Open a session on a register. The partial unique index is the
    /// arbiter under race: the second opener gets a conflict, not a
    /// second drawer.
    pub async fn open(
        &self,
        register_id: Uuid,
        opening_float: Decimal,
        cashier_id: Uuid,
        numbering: &Numbering,
    ) -> Result<SessionView> {
        if opening_float < Decimal::ZERO {
            return Err(Error::Validation(
                "the opening float must not be negative".into(),
            ));
        }
        let register_row = register::Entity::find_by_id(register_id)
            .one(&self.db)
            .await?
            .ok_or_else(|| Error::NotFound(format!("register {register_id}")))?;
        if !register_row.is_active {
            return Err(Error::Validation(format!(
                "register {} is not active",
                register_row.code
            )));
        }
        let txn = self.db.begin().await?;
        let id = Uuid::new_v4();
        let now = chrono::Utc::now();
        let number = numbering
            .next(&txn, crate::scm::POS_SESSION_SERIES)
            .await?;
        let inserted = session::ActiveModel {
            id: Set(id),
            number: Set(Some(number.formatted)),
            register_id: Set(register_id),
            cashier_id: Set(cashier_id),
            status: Set(SessionStatus::Open.as_str().to_string()),
            opened_at: Set(now),
            opening_float: Set(round_money(opening_float)),
            closed_at: Set(None),
            closed_by: Set(None),
            closing_note: Set(None),
            move_id: Set(None),
            gl_source: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&txn)
        .await;
        match inserted {
            Ok(_) => {}
            Err(e) if matches!(e.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) => {
                return Err(Error::Conflict(format!(
                    "register {} already has an open session",
                    register_row.code
                )));
            }
            Err(e) => return Err(Error::from(e)),
        }
        txn.commit().await?;
        self.view(id).await
    }

    /// The register's open (or still-closing) session, for resume after
    /// a refresh.
    pub async fn current(&self, register_id: Uuid) -> Result<Option<SessionView>> {
        let row = session::Entity::find()
            .filter(session::Column::RegisterId.eq(register_id))
            .filter(session::Column::Status.is_in([
                SessionStatus::Open.as_str(),
                SessionStatus::Closing.as_str(),
            ]))
            .one(&self.db)
            .await?;
        match row {
            Some(r) => Ok(Some(self.view(r.id).await?)),
            None => Ok(None),
        }
    }

    /// Record a drawer paid-in or paid-out; audit is the caller's duty.
    pub async fn cash_movement(
        &self,
        session_id: Uuid,
        kind: &str,
        amount: Decimal,
        reason: &str,
        by: Option<Uuid>,
    ) -> Result<CashMovementView> {
        if !["paid_in", "paid_out"].contains(&kind) {
            return Err(Error::Validation(
                "kind must be paid_in or paid_out".into(),
            ));
        }
        if amount <= Decimal::ZERO {
            return Err(Error::Validation("the amount must be positive".into()));
        }
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(Error::Validation(
                "a drawer movement needs its reason".into(),
            ));
        }
        let session_row = load_session(&self.db, session_id).await?;
        if SessionStatus::parse(&session_row.status)? != SessionStatus::Open {
            return Err(Error::Validation("the session is not open".into()));
        }
        let row = cash_movement::ActiveModel {
            id: Set(Uuid::new_v4()),
            session_id: Set(session_id),
            kind: Set(kind.to_string()),
            amount: Set(round_money(amount)),
            reason: Set(reason.to_string()),
            created_at: Set(chrono::Utc::now()),
            created_by: Set(by),
        }
        .insert(&self.db)
        .await?;
        Ok(CashMovementView {
            id: row.id,
            session_id: row.session_id,
            kind: row.kind,
            amount: row.amount,
            reason: row.reason,
            created_at: row.created_at,
        })
    }

    /// The X report: the live mid-shift picture, computed fresh.
    pub async fn x_report(&self, session_id: Uuid) -> Result<SessionReport> {
        let session_row = load_session(&self.db, session_id).await?;
        self.report_for(&session_row, false).await
    }

    /// The Z report: the closed session's stored summary — counted and
    /// variance come from the counts written at close, forever.
    pub async fn z_report(&self, session_id: Uuid) -> Result<SessionReport> {
        let session_row = load_session(&self.db, session_id).await?;
        if SessionStatus::parse(&session_row.status)? == SessionStatus::Open {
            return Err(Error::Validation(
                "the session is still open; ask for the X report".into(),
            ));
        }
        self.report_for(&session_row, true).await
    }

    /// Close the session. Step one validates and stores the counts and
    /// parks the session `closing`; step two consolidates stock and
    /// stages the revenue GL request and marks it `closed`. A session
    /// already `closing` (an earlier step-two failure) retries step two
    /// with the counts it stored before.
    pub async fn close(
        &self,
        session_id: Uuid,
        counts: Vec<CountInput>,
        note: Option<String>,
        unsynced: i64,
        by: Option<Uuid>,
        gl: &gl::Gl,
    ) -> Result<SessionView> {
        if unsynced > 0 {
            return Err(Error::Validation(format!(
                "{unsynced} sales are still waiting to sync; drain the queue before closing"
            )));
        }

        // --- Step one: counting -------------------------------------------
        let txn = self.db.begin().await?;
        let session_row = load_session_locked(&txn, session_id).await?;
        let status = SessionStatus::parse(&session_row.status)?;
        match status {
            SessionStatus::Open => {
                let expected = expected_by_tender(&txn, &session_row).await?;
                let mut provided: HashMap<String, Decimal> = HashMap::new();
                for c in &counts {
                    if !TENDERS.contains(&c.tender.as_str()) {
                        return Err(Error::Validation(format!(
                            "unknown tender {:?} in the counts",
                            c.tender
                        )));
                    }
                    if c.counted < Decimal::ZERO {
                        return Err(Error::Validation(
                            "a counted amount must not be negative".into(),
                        ));
                    }
                    if provided.insert(c.tender.clone(), round_money(c.counted)).is_some() {
                        return Err(Error::Validation(format!(
                            "tender {:?} is counted twice",
                            c.tender
                        )));
                    }
                }
                let mut any_variance = false;
                for tender in TENDERS {
                    let exp = expected.get(*tender).copied().unwrap_or(Decimal::ZERO);
                    let counted = provided.get(*tender).copied();
                    if counted.is_none() && !exp.is_zero() {
                        return Err(Error::Validation(format!(
                            "tender {tender:?} has takings; count it before closing"
                        )));
                    }
                    let counted = counted.unwrap_or(Decimal::ZERO);
                    if counted != exp {
                        any_variance = true;
                    }
                }
                let note = note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
                if any_variance && note.is_none() {
                    return Err(Error::Validation(
                        "the drawer does not match; a closing note explaining the difference is required"
                            .into(),
                    ));
                }
                // (Re)store the counts and park the session closing.
                session_count::Entity::delete_many()
                    .filter(session_count::Column::SessionId.eq(session_id))
                    .exec(&txn)
                    .await?;
                for tender in TENDERS {
                    let exp = expected.get(*tender).copied().unwrap_or(Decimal::ZERO);
                    let counted = provided.get(*tender).copied().unwrap_or(Decimal::ZERO);
                    if exp.is_zero() && counted.is_zero() {
                        continue;
                    }
                    session_count::ActiveModel {
                        id: Set(Uuid::new_v4()),
                        session_id: Set(session_id),
                        tender: Set(String::from(*tender)),
                        expected: Set(exp),
                        counted: Set(counted),
                    }
                    .insert(&txn)
                    .await?;
                }
                let mut active: session::ActiveModel = session_row.into();
                active.status = Set(SessionStatus::Closing.as_str().to_string());
                active.closing_note = Set(note);
                active.updated_at = Set(chrono::Utc::now());
                active.update(&txn).await?;
                txn.commit().await?;
            }
            SessionStatus::Closing => {
                // A retry after a failed consolidation: the counts stand.
                drop(txn);
            }
            SessionStatus::Closed => {
                return Err(Error::Validation("the session is already closed".into()));
            }
        }

        // --- Step two: consolidation --------------------------------------
        let (requests, view) = self.consolidate(session_id, by, gl).await?;
        for req in requests {
            gl.publish(req).await;
        }
        Ok(view)
    }

    /// One transaction: the aggregated stock movement, the staged revenue
    /// and COGS requests, and the flip to `closed`. Failing anywhere
    /// rolls the whole step back and the session stays `closing`.
    async fn consolidate(
        &self,
        session_id: Uuid,
        by: Option<Uuid>,
        gl_cx: &gl::Gl,
    ) -> Result<(Vec<nebula::ports::gl::GlPostingRequested>, SessionView)> {
        let txn = self.db.begin().await?;
        let session_row = load_session_locked(&txn, session_id).await?;
        if SessionStatus::parse(&session_row.status)? != SessionStatus::Closing {
            return Err(Error::internal("consolidation outside the closing state"));
        }
        let register_row = register::Entity::find_by_id(session_row.register_id)
            .one(&txn)
            .await?
            .ok_or_else(|| Error::internal("session without a register"))?;
        let warehouse_id = register_row.warehouse_id;
        let now = chrono::Utc::now();
        let close_date = now.date_naive();

        // Aggregate captured lines net of refunds per item × batch.
        let aggregates = aggregate_lines(&txn, session_id).await?;
        let mut requests: Vec<nebula::ports::gl::GlPostingRequested> = Vec::new();
        let mut move_id: Option<Uuid> = None;
        if !aggregates.is_empty() {
            let item_ids: Vec<Uuid> = {
                let mut ids: Vec<Uuid> = aggregates.iter().map(|a| a.item_id).collect();
                ids.sort();
                ids.dedup();
                ids
            };
            let items: HashMap<Uuid, item::Model> = item::Entity::find()
                .filter(item::Column::Id.is_in(item_ids.clone()))
                .all(&txn)
                .await?
                .into_iter()
                .map(|i| (i.id, i))
                .collect();
            let uoms: HashMap<Uuid, uom::Model> = uom::Entity::find()
                .all(&txn)
                .await?
                .into_iter()
                .map(|u| (u.id, u))
                .collect();
            let batch_ids: Vec<Uuid> = aggregates.iter().filter_map(|a| a.batch_id).collect();
            let batch_names: HashMap<Uuid, String> = batch::Entity::find()
                .filter(batch::Column::Id.is_in(batch_ids))
                .all(&txn)
                .await?
                .into_iter()
                .map(|b| (b.id, b.batch_no))
                .collect();

            // Only stockable items move; services sold at the till have
            // no stock side at all.
            let stockable: Vec<&Aggregate> = aggregates
                .iter()
                .filter(|a| {
                    items
                        .get(&a.item_id)
                        .is_some_and(|i| {
                            ItemType::parse(&i.item_type).is_ok_and(|t| t == ItemType::Stockable)
                        })
                })
                .collect();

            if !stockable.is_empty() {
                let mid = Uuid::new_v4();
                move_doc::ActiveModel {
                    id: Set(mid),
                    number: Set(session_row.number.clone()),
                    move_type: Set(MoveType::Issue.as_str().to_string()),
                    entry_date: Set(close_date),
                    memo: Set(format!(
                        "POS session {} consolidation",
                        session_row.number.as_deref().unwrap_or("")
                    )),
                    reference: Set(None),
                    from_warehouse_id: Set(Some(warehouse_id)),
                    to_warehouse_id: Set(None),
                    status: Set(MoveStatus::Posted.as_str().to_string()),
                    source: Set(Some(format!("pos.session:{session_id}"))),
                    reverses_id: Set(None),
                    reversed_by_id: Set(None),
                    posted_at: Set(Some(now)),
                    created_at: Set(now),
                    created_by: Set(by),
                }
                .insert(&txn)
                .await?;

                // Pre-lock the levels in item order so concurrent posting
                // of other documents cannot deadlock against us; keep the
                // running averages for any net-inbound line.
                let mut averages: HashMap<Uuid, Decimal> = HashMap::new();
                for item_id in &item_ids {
                    if stockable.iter().any(|a| a.item_id == *item_id) {
                        let level = lock_or_init_level(&txn, *item_id, warehouse_id).await?;
                        averages.insert(*item_id, level_average(&level));
                    }
                }

                for (i, agg) in stockable.iter().enumerate() {
                    let item_row = &items[&agg.item_id];
                    let stock_uom = uoms.get(&item_row.uom_id).ok_or_else(|| {
                        Error::internal(format!("stock uom missing for {}", item_row.sku))
                    })?;
                    let ml = move_line::ActiveModel {
                        id: Set(Uuid::new_v4()),
                        move_id: Set(mid),
                        line_no: Set((i + 1) as i32),
                        item_id: Set(agg.item_id),
                        qty: Set(agg.net_qty.abs()),
                        entered_uom_id: Set(None),
                        unit_cost: Set(None),
                        batch_no: Set(agg
                            .batch_id
                            .and_then(|b| batch_names.get(&b).cloned())),
                        batch_id: Set(agg.batch_id),
                        serial_nos: Set(None),
                        memo: Set(None),
                        created_at: Set(now),
                    }
                    .insert(&txn)
                    .await?;
                    // Net sold goes out at moving average; a session that
                    // netted *inbound* for an item (refunds beat sales)
                    // restocks it at the running average — the same money
                    // an issue would have taken out.
                    let mv = if agg.net_qty > Decimal::ZERO {
                        Movement::Issue {
                            qty: agg.net_qty,
                            covered_by_reservation: Decimal::ZERO,
                        }
                    } else {
                        Movement::Receipt {
                            qty: -agg.net_qty,
                            unit_cost: averages.get(&agg.item_id).copied().unwrap_or(Decimal::ZERO),
                        }
                    };
                    StockService::apply(
                        &txn,
                        mid,
                        ml.id,
                        close_date,
                        item_row,
                        stock_uom,
                        warehouse_id,
                        agg.batch_id,
                        mv,
                    )
                    .await?;
                }
                move_id = Some(mid);

                // COGS rides on the movement's own ledger value.
                if let Some(req) = gl::cogs_move_request(
                    &txn,
                    format!("pos.session:{session_id}:cogs"),
                    mid,
                    format!(
                        "POS session {} cost of sales",
                        session_row.number.as_deref().unwrap_or("")
                    ),
                    close_date,
                    gl_cx.tenant_id(),
                )
                .await?
                {
                    gl::stage(&txn, &req).await?;
                    requests.push(req);
                }
            }
        }

        // The revenue entry from the money actually taken and counted.
        let money = session_money(&txn, &session_row).await?;
        let counts: HashMap<String, session_count::Model> = session_count::Entity::find()
            .filter(session_count::Column::SessionId.eq(session_id))
            .all(&txn)
            .await?
            .into_iter()
            .map(|c| (c.tender.clone(), c))
            .collect();
        let cash_variance = counts
            .get("cash")
            .map(|c| c.counted - c.expected)
            .unwrap_or(Decimal::ZERO);
        let gl_source = format!("pos.session:{session_id}:close");
        if let Some(req) = gl::pos_session_request(
            gl_source.clone(),
            format!(
                "POS session {} takings",
                session_row.number.as_deref().unwrap_or("")
            ),
            close_date,
            gl::PosTenderTotals {
                cash: money.tender_net("cash") + cash_variance,
                mpesa: money.tender_net("mpesa"),
                card: money.tender_net("card"),
                over_short: cash_variance,
            },
            money.net_sales(),
            money.tax_net(),
            gl_cx.tenant_id(),
        )? {
            gl::stage(&txn, &req).await?;
            requests.push(req);
        }

        let mut active: session::ActiveModel = session_row.into();
        active.status = Set(SessionStatus::Closed.as_str().to_string());
        active.closed_at = Set(Some(now));
        active.closed_by = Set(by);
        active.move_id = Set(move_id);
        active.gl_source = Set(Some(gl_source));
        active.updated_at = Set(now);
        active.update(&txn).await?;
        txn.commit().await?;

        let view = self.view(session_id).await?;
        Ok((requests, view))
    }

    pub async fn list(&self, filter: SessionFilter) -> Result<Vec<SessionView>> {
        let mut query = session::Entity::find();
        if let Some(register_id) = filter.register_id {
            query = query.filter(session::Column::RegisterId.eq(register_id));
        }
        if let Some(status) = filter.status {
            query = query.filter(session::Column::Status.eq(status.as_str()));
        }
        if let Some(from) = filter.from {
            query = query.filter(session::Column::OpenedAt.gte(from));
        }
        if let Some(to) = filter.to {
            query = query.filter(session::Column::OpenedAt.lte(to));
        }
        let rows = query
            .order_by_desc(session::Column::OpenedAt)
            .all(&self.db)
            .await?;
        let register_ids: Vec<Uuid> = rows.iter().map(|r| r.register_id).collect();
        let registers: HashMap<Uuid, register::Model> = register::Entity::find()
            .filter(register::Column::Id.is_in(register_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|r| (r.id, r))
            .collect();
        rows.into_iter()
            .map(|r| view_of(&r, registers.get(&r.register_id)))
            .collect()
    }

    pub async fn view(&self, id: Uuid) -> Result<SessionView> {
        let row = load_session(&self.db, id).await?;
        let register_row = register::Entity::find_by_id(row.register_id)
            .one(&self.db)
            .await?;
        view_of(&row, register_row.as_ref())
    }

    /// The shared X/Z computation; `stored_counts` swaps the live picture
    /// for the counts written at close.
    async fn report_for(
        &self,
        session_row: &session::Model,
        stored_counts: bool,
    ) -> Result<SessionReport> {
        let money = session_money(&self.db, session_row).await?;
        let expected = expected_by_tender(&self.db, session_row).await?;
        let counts: HashMap<String, session_count::Model> = if stored_counts {
            session_count::Entity::find()
                .filter(session_count::Column::SessionId.eq(session_row.id))
                .all(&self.db)
                .await?
                .into_iter()
                .map(|c| (c.tender.clone(), c))
                .collect()
        } else {
            HashMap::new()
        };

        let mut tenders = Vec::new();
        for tender in TENDERS {
            let sales = money.tender_sales.get(*tender).copied().unwrap_or_default();
            let refunds = money
                .tender_refunds
                .get(*tender)
                .copied()
                .unwrap_or_default();
            // The Z report's expected is the one stored at close; the X
            // report's is live.
            let (expected_amt, counted, variance) = match counts.get(*tender) {
                Some(c) => (c.expected, Some(c.counted), Some(c.counted - c.expected)),
                None => (
                    expected.get(*tender).copied().unwrap_or(Decimal::ZERO),
                    None,
                    None,
                ),
            };
            if sales.is_zero()
                && refunds.is_zero()
                && expected_amt.is_zero()
                && counted.is_none()
                && *tender != "cash"
            {
                continue;
            }
            tenders.push(TenderReportLine {
                tender: String::from(*tender),
                sales,
                refunds,
                net: sales - refunds,
                expected: expected_amt,
                counted,
                variance,
            });
        }

        let register_row = register::Entity::find_by_id(session_row.register_id)
            .one(&self.db)
            .await?;
        Ok(SessionReport {
            session: view_of(session_row, register_row.as_ref())?,
            orders: money.sales_count,
            refunds: money.refunds_count,
            voids: money.voids_count,
            price_drift: money.drift_count,
            offline: money.offline_count,
            gross_sales: money.gross_sales,
            refund_total: money.refund_total,
            net_total: money.net_sales() + money.tax_net(),
            tax_total: money.tax_net(),
            paid_in: money.paid_in,
            paid_out: money.paid_out,
            expected_cash: expected.get("cash").copied().unwrap_or(Decimal::ZERO),
            tenders,
        })
    }
}

// ---------------------------------------------------------------------------
// The session's money, computed once and shared by reports and the close
// ---------------------------------------------------------------------------

/// Everything the captured orders of one session add up to.
struct SessionMoney {
    /// Per tender, gross amounts applied on sales / on refunds.
    tender_sales: HashMap<String, Decimal>,
    tender_refunds: HashMap<String, Decimal>,
    gross_sales: Decimal,
    refund_total: Decimal,
    /// Tax inside sales minus tax inside refunds.
    sales_tax: Decimal,
    refund_tax: Decimal,
    paid_in: Decimal,
    paid_out: Decimal,
    sales_count: i64,
    refunds_count: i64,
    voids_count: i64,
    drift_count: i64,
    offline_count: i64,
}

impl SessionMoney {
    fn tender_net(&self, tender: &str) -> Decimal {
        self.tender_sales.get(tender).copied().unwrap_or_default()
            - self.tender_refunds.get(tender).copied().unwrap_or_default()
    }

    /// Net revenue ex VAT, net of refunds.
    fn net_sales(&self) -> Decimal {
        (self.gross_sales - self.sales_tax) - (self.refund_total - self.refund_tax)
    }

    fn tax_net(&self) -> Decimal {
        self.sales_tax - self.refund_tax
    }
}

async fn session_money<C: ConnectionTrait>(
    conn: &C,
    session_row: &session::Model,
) -> Result<SessionMoney> {
    let orders = order::Entity::find()
        .filter(order::Column::SessionId.eq(session_row.id))
        .all(conn)
        .await?;
    let captured: Vec<&order::Model> = orders
        .iter()
        .filter(|o| o.status == OrderStatus::Captured.as_str())
        .collect();
    let order_ids: Vec<Uuid> = captured.iter().map(|o| o.id).collect();
    let payments = if order_ids.is_empty() {
        Vec::new()
    } else {
        order_payment::Entity::find()
            .filter(order_payment::Column::OrderId.is_in(order_ids))
            .all(conn)
            .await?
    };
    let kind_of: HashMap<Uuid, OrderKind> = captured
        .iter()
        .map(|o| Ok((o.id, OrderKind::parse(&o.kind)?)))
        .collect::<Result<_>>()?;

    let mut money = SessionMoney {
        tender_sales: HashMap::new(),
        tender_refunds: HashMap::new(),
        gross_sales: Decimal::ZERO,
        refund_total: Decimal::ZERO,
        sales_tax: Decimal::ZERO,
        refund_tax: Decimal::ZERO,
        paid_in: Decimal::ZERO,
        paid_out: Decimal::ZERO,
        sales_count: 0,
        refunds_count: 0,
        voids_count: orders
            .iter()
            .filter(|o| o.status == OrderStatus::Voided.as_str())
            .count() as i64,
        drift_count: captured.iter().filter(|o| o.price_drift).count() as i64,
        offline_count: captured.iter().filter(|o| o.captured_offline).count() as i64,
    };
    for o in &captured {
        match OrderKind::parse(&o.kind)? {
            OrderKind::Sale => {
                money.gross_sales += o.total;
                money.sales_tax += o.tax_total;
                money.sales_count += 1;
            }
            OrderKind::Refund => {
                money.refund_total += o.total;
                money.refund_tax += o.tax_total;
                money.refunds_count += 1;
            }
        }
    }
    for p in payments {
        let bucket = match kind_of.get(&p.order_id) {
            Some(OrderKind::Sale) => &mut money.tender_sales,
            Some(OrderKind::Refund) => &mut money.tender_refunds,
            None => continue,
        };
        *bucket.entry(p.tender.clone()).or_default() += p.amount;
    }

    let drawer = cash_movement::Entity::find()
        .filter(cash_movement::Column::SessionId.eq(session_row.id))
        .all(conn)
        .await?;
    for m in drawer {
        if m.kind == "paid_in" {
            money.paid_in += m.amount;
        } else {
            money.paid_out += m.amount;
        }
    }
    Ok(money)
}

/// What each tender should hold at count time: cash starts from the
/// float and moves with the drawer, the clearing tenders are simply
/// their net takings.
async fn expected_by_tender<C: ConnectionTrait>(
    conn: &C,
    session_row: &session::Model,
) -> Result<HashMap<String, Decimal>> {
    let money = session_money(conn, session_row).await?;
    let mut map = HashMap::new();
    map.insert(
        "cash".to_string(),
        session_row.opening_float + money.tender_net("cash") + money.paid_in - money.paid_out,
    );
    map.insert("mpesa".to_string(), money.tender_net("mpesa"));
    map.insert("card".to_string(), money.tender_net("card"));
    Ok(map)
}

/// One item × batch position, net of refunds: positive = sold (stock
/// out), negative = net returned (stock back in).
struct Aggregate {
    item_id: Uuid,
    batch_id: Option<Uuid>,
    net_qty: Decimal,
}

async fn aggregate_lines(
    txn: &DatabaseTransaction,
    session_id: Uuid,
) -> Result<Vec<Aggregate>> {
    let orders = order::Entity::find()
        .filter(order::Column::SessionId.eq(session_id))
        .filter(order::Column::Status.eq(OrderStatus::Captured.as_str()))
        .all(txn)
        .await?;
    if orders.is_empty() {
        return Ok(Vec::new());
    }
    let kind_of: HashMap<Uuid, OrderKind> = orders
        .iter()
        .map(|o| Ok((o.id, OrderKind::parse(&o.kind)?)))
        .collect::<Result<_>>()?;
    let lines = order_line::Entity::find()
        .filter(order_line::Column::OrderId.is_in(orders.iter().map(|o| o.id).collect::<Vec<_>>()))
        .all(txn)
        .await?;
    let mut map: HashMap<(Uuid, Option<Uuid>), Decimal> = HashMap::new();
    for l in lines {
        let sign = match kind_of.get(&l.order_id) {
            Some(OrderKind::Sale) => Decimal::ONE,
            Some(OrderKind::Refund) => -Decimal::ONE,
            None => continue,
        };
        *map.entry((l.item_id, l.batch_id)).or_default() += sign * l.qty;
    }
    let mut out: Vec<Aggregate> = map
        .into_iter()
        .filter(|(_, qty)| !qty.is_zero())
        .map(|((item_id, batch_id), net_qty)| Aggregate {
            item_id,
            batch_id,
            net_qty,
        })
        .collect();
    // Deterministic line order: by item then batch.
    out.sort_by_key(|a| (a.item_id, a.batch_id));
    Ok(out)
}

async fn load_session<C: ConnectionTrait>(conn: &C, id: Uuid) -> Result<session::Model> {
    session::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("pos session {id}")))
}

async fn load_session_locked(
    txn: &DatabaseTransaction,
    id: Uuid,
) -> Result<session::Model> {
    session::Entity::find_by_id(id)
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("pos session {id}")))
}

fn view_of(row: &session::Model, register_row: Option<&register::Model>) -> Result<SessionView> {
    Ok(SessionView {
        id: row.id,
        number: row.number.clone(),
        register_id: row.register_id,
        register_code: register_row.map(|r| r.code.clone()).unwrap_or_default(),
        register_name: register_row.map(|r| r.name.clone()).unwrap_or_default(),
        cashier_id: row.cashier_id,
        status: SessionStatus::parse(&row.status)?,
        opened_at: row.opened_at,
        opening_float: row.opening_float,
        closed_at: row.closed_at,
        closed_by: row.closed_by,
        closing_note: row.closing_note.clone(),
        move_id: row.move_id,
        gl_source: row.gl_source.clone(),
    })
}

// ---------------------------------------------------------------------------
// Views (API DTOs)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SessionView {
    pub id: Uuid,
    pub number: Option<String>,
    pub register_id: Uuid,
    pub register_code: String,
    pub register_name: String,
    pub cashier_id: Uuid,
    pub status: SessionStatus,
    #[schema(value_type = String, format = DateTime)]
    pub opened_at: chrono::DateTime<chrono::Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub opening_float: Decimal,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub closed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub closed_by: Option<Uuid>,
    pub closing_note: Option<String>,
    /// The consolidated stock movement, once closed.
    pub move_id: Option<Uuid>,
    /// The revenue entry's outbox source key, once closed.
    pub gl_source: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CashMovementView {
    pub id: Uuid,
    pub session_id: Uuid,
    pub kind: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub amount: Decimal,
    pub reason: String,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct TenderReportLine {
    pub tender: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub sales: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub refunds: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub net: Decimal,
    /// What the tender should hold: cash includes float and drawer
    /// movements, the clearing tenders are net takings.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub expected: Decimal,
    /// Stored at close; absent on an X report.
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub counted: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub variance: Option<Decimal>,
}

/// The X (live) or Z (stored) picture of a session.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SessionReport {
    pub session: SessionView,
    pub orders: i64,
    pub refunds: i64,
    pub voids: i64,
    /// Offline-synced sales whose price no longer matched at sync.
    pub price_drift: i64,
    /// Sales captured with the network down.
    pub offline: i64,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub gross_sales: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub refund_total: Decimal,
    /// Net takings (sales − refunds), VAT included.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub net_total: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub tax_total: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub paid_in: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub paid_out: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub expected_cash: Decimal,
    pub tenders: Vec<TenderReportLine>,
}

pub struct SessionFilter {
    pub register_id: Option<Uuid>,
    pub status: Option<SessionStatus>,
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
pub struct OpenSessionRequest {
    pub register_id: Uuid,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub opening_float: Decimal,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CashMovementRequest {
    /// paid_in | paid_out.
    pub kind: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub amount: Decimal,
    pub reason: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SessionCountRequest {
    /// cash | mpesa | card.
    pub tender: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub counted: Decimal,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CloseSessionRequest {
    pub counts: Vec<SessionCountRequest>,
    /// Required when any tender's count differs from expected.
    pub note: Option<String>,
    /// How many sales the till still holds unsynced; nonzero refuses the
    /// close.
    #[serde(default)]
    pub unsynced: i64,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListSessionsQuery {
    pub register_id: Option<Uuid>,
    pub status: Option<SessionStatus>,
    /// Opened-at range, inclusive.
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CurrentSessionQuery {
    pub register_id: Uuid,
}

pub(crate) fn routes() -> Router {
    Router::new()
        .route("/pos/sessions", get(list_sessions))
        .route("/pos/sessions/open", post(open_session))
        .route("/pos/sessions/current", get(current_session))
        .route("/pos/sessions/{id}", get(get_session))
        .route("/pos/sessions/{id}/cash-movements", post(add_cash_movement))
        .route("/pos/sessions/{id}/x-report", get(x_report))
        .route("/pos/sessions/{id}/close", post(close_session))
        .route("/pos/sessions/{id}/z-report", get(z_report))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    list_sessions,
    open_session,
    current_session,
    get_session,
    add_cash_movement,
    x_report,
    close_session,
    z_report
))]
struct ApiDoc;

#[utoipa::path(get, path = "/pos/sessions", tag = "pos",
    params(ListSessionsQuery),
    responses((status = 200, body = Vec<SessionView>)))]
async fn list_sessions(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<ListSessionsQuery>,
) -> Result<Json<Vec<SessionView>>> {
    authz.require(names::REPORTS_VIEW).await?;
    SessionService::new(db)
        .list(SessionFilter {
            register_id: q.register_id,
            status: q.status,
            from: q.from,
            to: q.to,
        })
        .await
        .map(Json)
}

#[utoipa::path(post, path = "/pos/sessions/open", tag = "pos",
    request_body = OpenSessionRequest,
    responses((status = 200, body = SessionView)))]
async fn open_session(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Extension(numbering): Extension<Numbering>,
    Json(req): Json<OpenSessionRequest>,
) -> Result<Json<SessionView>> {
    authz.require(names::SESSIONS_OPEN).await?;
    let view = SessionService::new(db)
        .open(req.register_id, req.opening_float, authz.user.id, &numbering)
        .await?;
    audit
        .0
        .event(format!(
            "opened POS session {} with a float of {}",
            view.number.as_deref().unwrap_or(""),
            view.opening_float
        ))
        .await;
    Ok(Json(view))
}

/// Resume after a refresh: the register's open session, or nothing.
#[utoipa::path(get, path = "/pos/sessions/current", tag = "pos",
    params(CurrentSessionQuery),
    responses((status = 200, body = Option<SessionView>)))]
async fn current_session(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<CurrentSessionQuery>,
) -> Result<Json<Option<SessionView>>> {
    authz.require(names::SELL).await?;
    SessionService::new(db).current(q.register_id).await.map(Json)
}

#[utoipa::path(get, path = "/pos/sessions/{id}", tag = "pos",
    params(("id" = Uuid, Path, description = "Session id")),
    responses((status = 200, body = SessionView)))]
async fn get_session(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<SessionView>> {
    authz.require(names::SELL).await?;
    SessionService::new(db).view(id).await.map(Json)
}

#[utoipa::path(post, path = "/pos/sessions/{id}/cash-movements", tag = "pos",
    params(("id" = Uuid, Path, description = "Session id")),
    request_body = CashMovementRequest,
    responses((status = 200, body = CashMovementView)))]
async fn add_cash_movement(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<CashMovementRequest>,
) -> Result<Json<CashMovementView>> {
    authz.require(names::SESSIONS_PAID_IN_OUT).await?;
    let view = SessionService::new(db)
        .cash_movement(id, &req.kind, req.amount, &req.reason, Some(authz.user.id))
        .await?;
    audit
        .0
        .event(format!(
            "{} {} at the drawer: {}",
            view.kind.replace('_', " "),
            view.amount,
            view.reason
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(get, path = "/pos/sessions/{id}/x-report", tag = "pos",
    params(("id" = Uuid, Path, description = "Session id")),
    responses((status = 200, body = SessionReport)))]
async fn x_report(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<SessionReport>> {
    authz.require(names::SELL).await?;
    SessionService::new(db).x_report(id).await.map(Json)
}

#[utoipa::path(post, path = "/pos/sessions/{id}/close", tag = "pos",
    params(("id" = Uuid, Path, description = "Session id")),
    request_body = CloseSessionRequest,
    responses((status = 200, body = SessionView)))]
async fn close_session(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    CurrentTenant(tenant): CurrentTenant,
    Extension(events): Extension<Events>,
    Path(id): Path<Uuid>,
    Json(req): Json<CloseSessionRequest>,
) -> Result<Json<SessionView>> {
    authz.require(names::SESSIONS_CLOSE).await?;
    let gl = gl::Gl::new(events, tenant.map(|t| t.id));
    let view = SessionService::new(db)
        .close(
            id,
            req.counts
                .into_iter()
                .map(|c| CountInput {
                    tender: c.tender,
                    counted: c.counted,
                })
                .collect(),
            req.note,
            req.unsynced,
            Some(authz.user.id),
            &gl,
        )
        .await?;
    audit
        .0
        .event(format!(
            "closed POS session {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(get, path = "/pos/sessions/{id}/z-report", tag = "pos",
    params(("id" = Uuid, Path, description = "Session id")),
    responses((status = 200, body = SessionReport)))]
async fn z_report(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<SessionReport>> {
    authz.require(names::REPORTS_VIEW).await?;
    SessionService::new(db).z_report(id).await.map(Json)
}
