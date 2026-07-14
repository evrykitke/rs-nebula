//! SCM's side of the GL posting port: perpetual-inventory bookkeeping.
//!
//! Every posted document with a financial effect asks accounting (via
//! [`nebula::ports::gl`]) to book the matrix from the blueprint:
//!
//! | Document | Debit | Credit |
//! |---|---|---|
//! | Direct stock receipt / count up | Inventory | Stock adjustments |
//! | Stock issue | COGS | Inventory |
//! | Count down | Stock adjustments | Inventory |
//! | Transfer | — no entry (same asset account) — |
//! | Goods receipt (against PO) | Inventory | GRNI |
//! | Purchase invoice | GRNI (+ charges, ± variance) | Accounts payable |
//! | Reversal / cancellation | the mirror, by sign |
//!
//! Account references are **roles** (seeded system keys), resolved per
//! item → category → module default, so the publisher never needs the
//! tenant's chart. Amounts come from the stock ledger's own `value_delta`
//! rows, so the GL always mirrors what the engine actually booked.
//!
//! Delivery: the request is staged in `scm_gl_outbox` **inside the
//! document's posting transaction** (a crash can't lose it), published
//! after commit, and deleted when accounting answers `gl.posting_booked`.
//! A background sweeper re-publishes anything still staged after a grace
//! period — the subscriber deduplicates on `source`, so re-emission is
//! always safe.

use crate::scm::inventory::item::{category, item};
use crate::scm::inventory::moves::{MoveType, doc as move_doc};
use crate::scm::inventory::stock::{ledger, round_money};
use crate::scm::procurement::invoice as pinvoice;
use crate::scm::procurement::order::{effective_price, order, order_line};
use crate::scm::procurement::reports::ProcurementQueries;
use axum::routing::get;
use axum::{Json, Router};
use nebula::auth::Authz;
use nebula::error::{Error, Result};
use nebula::ports::gl::{GlLine, GlPostingBooked, GlPostingRequested};
use nebula::sea_orm;
use nebula::tenancy::TenantManager;
use nebula::{
    Column as ReportColumn, DataCx, Events, ModuleContext, Report, ReportData, ReportDataSource,
    ReportDefinition, ReportFormat, ReportOutput, Table, TenantDb,
};
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, QueryOrder, Set,
    Statement,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// Roles resolved when neither the item nor its category overrides them.
/// These are the accounting seed's stable system keys — the only shared
/// vocabulary between the two apps, by design.
const DEFAULT_INVENTORY_ROLE: &str = "inventory";
const DEFAULT_COGS_ROLE: &str = "cogs";
const DEFAULT_ADJUSTMENT_ROLE: &str = "stock_adjustment";
const GRNI_ROLE: &str = "grni";
const AP_ROLE: &str = "ap";
const PPV_ROLE: &str = "purchase_price_variance";
const OTHER_CHARGES_ROLE: &str = "opex";
const ROUNDING_ROLE: &str = "rounding";

/// Rows staged after this long without an acknowledgement are re-published.
const SWEEP_GRACE_SECS: i64 = 120;
/// How often the sweeper looks for lingering rows.
const SWEEP_INTERVAL_SECS: u64 = 300;

/// A staged posting request awaiting accounting's acknowledgement.
pub mod outbox {
    use nebula::sea_orm;
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "scm_gl_outbox")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub source: String,
        #[sea_orm(column_type = "JsonBinary")]
        pub payload: Json,
        pub created_at: DateTimeUtc,
        pub attempts: i32,
        pub last_attempt_at: Option<DateTimeUtc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// The publishing context a posting handler carries into its service:
/// the event bus plus the tenant whose books the entry belongs in.
#[derive(Clone)]
pub struct Gl {
    events: Events,
    tenant_id: Option<Uuid>,
}

impl Gl {
    pub fn new(events: Events, tenant_id: Option<Uuid>) -> Self {
        Self { events, tenant_id }
    }

    pub fn tenant_id(&self) -> Option<Uuid> {
        self.tenant_id
    }

    /// Publish a staged request (call after the posting transaction has
    /// committed). In-process subscribers run before this returns, so on
    /// the happy path the entry is booked and the outbox row already
    /// cleared when the HTTP response leaves.
    pub async fn publish(&self, req: GlPostingRequested) {
        self.events.publish(req).await;
    }
}

/// Stage a request in the outbox on the document's own transaction.
pub(crate) async fn stage(txn: &DatabaseTransaction, req: &GlPostingRequested) -> Result<()> {
    let payload = serde_json::to_value(req)
        .map_err(|e| Error::internal(format!("failed to serialize GL request: {e}")))?;
    outbox::ActiveModel {
        source: Set(req.source.clone()),
        payload: Set(payload),
        created_at: Set(chrono::Utc::now()),
        attempts: Set(0),
        last_attempt_at: Set(None),
    }
    .insert(txn)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Building requests: the posting matrix
// ---------------------------------------------------------------------------

/// Accumulates net debit (positive) / credit (negative) per role, then
/// emits one line per role. Because every booking adds the same amount to
/// one role and subtracts it from another, the result balances by
/// construction.
#[derive(Default)]
struct EntryBuilder {
    net: Vec<(String, Decimal, Option<String>)>,
}

impl EntryBuilder {
    fn add(&mut self, role: &str, amount: Decimal, memo: Option<&str>) {
        if amount.is_zero() {
            return;
        }
        match self.net.iter_mut().find(|(r, _, _)| r == role) {
            Some((_, total, _)) => *total += amount,
            None => self
                .net
                .push((role.to_string(), amount, memo.map(str::to_string))),
        }
    }

    fn debit(&mut self, role: &str, amount: Decimal, memo: Option<&str>) {
        self.add(role, amount, memo);
    }

    fn credit(&mut self, role: &str, amount: Decimal, memo: Option<&str>) {
        self.add(role, -amount, memo);
    }

    /// The finished lines; empty when everything netted to zero.
    fn lines(self) -> Vec<GlLine> {
        self.net
            .into_iter()
            .filter(|(_, net, _)| !net.is_zero())
            .map(|(role, net, memo)| {
                if net > Decimal::ZERO {
                    GlLine::debit(role, round_money(net), memo)
                } else {
                    GlLine::credit(role, round_money(-net), memo)
                }
            })
            .collect()
    }
}

/// The GL roles one item books against, resolved item → category → default.
struct ItemRoles {
    inventory: String,
    cogs: String,
    adjustment: String,
}

/// Resolve roles for every item id given, on the caller's connection.
async fn item_roles<C: ConnectionTrait>(
    conn: &C,
    item_ids: &[Uuid],
) -> Result<HashMap<Uuid, ItemRoles>> {
    let items: Vec<item::Model> = item::Entity::find()
        .filter(item::Column::Id.is_in(item_ids.to_vec()))
        .all(conn)
        .await?;
    let category_ids: Vec<Uuid> = items.iter().filter_map(|i| i.category_id).collect();
    let categories: HashMap<Uuid, category::Model> = category::Entity::find()
        .filter(category::Column::Id.is_in(category_ids))
        .all(conn)
        .await?
        .into_iter()
        .map(|c| (c.id, c))
        .collect();

    let pick = |own: &Option<String>, cat: Option<&Option<String>>, default: &str| {
        own.clone()
            .or_else(|| cat.and_then(|c| c.clone()))
            .unwrap_or_else(|| default.to_string())
    };

    Ok(items
        .into_iter()
        .map(|i| {
            let cat = i.category_id.and_then(|id| categories.get(&id));
            let roles = ItemRoles {
                inventory: pick(
                    &i.inventory_account_role,
                    cat.map(|c| &c.inventory_account_role),
                    DEFAULT_INVENTORY_ROLE,
                ),
                cogs: pick(
                    &i.cogs_account_role,
                    cat.map(|c| &c.cogs_account_role),
                    DEFAULT_COGS_ROLE,
                ),
                adjustment: pick(
                    &i.adjustment_account_role,
                    cat.map(|c| &c.adjustment_account_role),
                    DEFAULT_ADJUSTMENT_ROLE,
                ),
            };
            (i.id, roles)
        })
        .collect())
}

/// The ledger rows a posted movement wrote, in engine order.
async fn ledger_rows<C: ConnectionTrait>(conn: &C, move_id: Uuid) -> Result<Vec<ledger::Model>> {
    ledger::Entity::find()
        .filter(ledger::Column::MoveId.eq(move_id))
        .order_by_asc(ledger::Column::Seq)
        .all(conn)
        .await
        .map_err(Error::from)
}

/// The entry for an inventory-module movement (direct receipt, issue,
/// adjustment; `None` for transfers, zero-value movements, and movements
/// generated by a source document — the source books those itself).
pub(crate) async fn stock_move_request(
    txn: &DatabaseTransaction,
    move_id: Uuid,
    tenant_id: Option<Uuid>,
) -> Result<Option<GlPostingRequested>> {
    let doc = move_doc::Entity::find_by_id(move_id)
        .one(txn)
        .await?
        .ok_or_else(|| Error::internal("movement vanished inside its transaction"))?;
    if doc.source.is_some() {
        return Ok(None);
    }
    let move_type = MoveType::parse(&doc.move_type)?;
    if move_type == MoveType::Transfer {
        return Ok(None);
    }

    let rows = ledger_rows(txn, move_id).await?;
    let item_ids: Vec<Uuid> = rows.iter().map(|r| r.item_id).collect();
    let roles = item_roles(txn, &item_ids).await?;

    let mut entry = EntryBuilder::default();
    for row in &rows {
        let r = roles
            .get(&row.item_id)
            .ok_or_else(|| Error::internal("ledger row without an item"))?;
        // The inventory asset always follows the ledger's own value; the
        // contra side depends on why the stock moved.
        let contra = match move_type {
            MoveType::Issue => r.cogs.as_str(),
            _ => r.adjustment.as_str(),
        };
        entry.debit(&r.inventory, row.value_delta, None);
        entry.credit(contra, row.value_delta, None);
    }
    let lines = entry.lines();
    if lines.is_empty() {
        return Ok(None);
    }

    let label = match move_type {
        MoveType::Receipt => "Stock receipt",
        MoveType::Issue => "Stock issue",
        MoveType::Adjustment => "Stock adjustment",
        MoveType::Transfer => unreachable!("transfers book nothing"),
    };
    Ok(Some(GlPostingRequested {
        tenant_id,
        source: format!("scm.move:{move_id}:post"),
        entry_date: doc.entry_date,
        memo: format!("{label} {}", doc.number.as_deref().unwrap_or("")).trim().to_string(),
        currency: None,
        lines,
    }))
}

/// The entry for a procurement goods receipt (or its reversal): the stock
/// movement's value against GRNI. Reversal movements carry negative
/// deltas, so the same computation yields the mirror entry.
pub(crate) async fn goods_receipt_request(
    txn: &DatabaseTransaction,
    receipt_id: Uuid,
    move_id: Uuid,
    order_number: Option<&str>,
    entry_date: chrono::NaiveDate,
    tenant_id: Option<Uuid>,
) -> Result<Option<GlPostingRequested>> {
    let rows = ledger_rows(txn, move_id).await?;
    let item_ids: Vec<Uuid> = rows.iter().map(|r| r.item_id).collect();
    let roles = item_roles(txn, &item_ids).await?;

    let mut entry = EntryBuilder::default();
    for row in &rows {
        let r = roles
            .get(&row.item_id)
            .ok_or_else(|| Error::internal("ledger row without an item"))?;
        entry.debit(&r.inventory, row.value_delta, None);
        entry.credit(GRNI_ROLE, row.value_delta, None);
    }
    let lines = entry.lines();
    if lines.is_empty() {
        return Ok(None);
    }

    Ok(Some(GlPostingRequested {
        tenant_id,
        source: format!("scm.receipt:{receipt_id}:post"),
        entry_date,
        memo: format!(
            "Goods receipt against {}",
            order_number.unwrap_or("purchase order")
        ),
        currency: None,
        lines,
    }))
}

/// The entry for posting (`mirror = false`) or cancelling (`mirror =
/// true`) a purchase invoice:
///
/// - **Dr GRNI** at billed qty × order price × the invoice's rate — the
///   same formula the receipt accrued with, so matched prices clear the
///   interim account exactly (rate overrides surface on reconciliation).
/// - **Dr Other charges** (freight and the like) to opex.
/// - **Cr Purchase price variance** for header discounts.
/// - **Cr Accounts payable** at the invoice total in base.
/// - A rounding line absorbs any sub-cent gap between the converted
///   components and the converted total.
pub(crate) async fn purchase_invoice_request(
    txn: &DatabaseTransaction,
    inv: &pinvoice::invoice::Model,
    mirror: bool,
    tenant_id: Option<Uuid>,
) -> Result<Option<GlPostingRequested>> {
    let lines: Vec<pinvoice::invoice_line::Model> = pinvoice::invoice_line::Entity::find()
        .filter(pinvoice::invoice_line::Column::InvoiceId.eq(inv.id))
        .order_by_asc(pinvoice::invoice_line::Column::LineNo)
        .all(txn)
        .await?;
    let order_id = inv
        .order_id
        .ok_or_else(|| Error::internal("invoice without an order"))?;
    let order_row = order::Entity::find_by_id(order_id)
        .one(txn)
        .await?
        .ok_or_else(|| Error::internal("invoice lost its order"))?;
    let order_lines: HashMap<Uuid, order_line::Model> = order_line::Entity::find()
        .filter(order_line::Column::OrderId.eq(order_id))
        .all(txn)
        .await?
        .into_iter()
        .map(|l| (l.id, l))
        .collect();

    let rate = inv.exchange_rate;

    // Goods value in base, line by line with the receipt's own rounding
    // (value = qty × cost-rounded unit price), so GRNI relief mirrors the
    // accrual when prices match.
    let mut subtotal = Decimal::ZERO; // invoice currency
    let mut goods_base = Decimal::ZERO;
    for l in &lines {
        let ol = l
            .order_line_id
            .and_then(|id| order_lines.get(&id))
            .ok_or_else(|| Error::internal("invoice line lost its order line"))?;
        let price = effective_price(ol.unit_price, ol.discount_pct);
        subtotal += round_money(l.qty * price);
        goods_base += round_money(l.qty * crate::scm::inventory::stock::round_cost(price * rate));
    }

    // Header effects, converted at the invoice rate.
    let pct_discount = inv
        .discount_pct
        .map(|pct| round_money(subtotal * pct / Decimal::ONE_HUNDRED))
        .unwrap_or(Decimal::ZERO);
    let discounts = pct_discount + inv.discount_amount.unwrap_or(Decimal::ZERO);
    let discounts_base = round_money(discounts * rate);
    let charges_base = round_money(inv.other_charges.unwrap_or(Decimal::ZERO) * rate);
    let total = round_money(subtotal - discounts + inv.other_charges.unwrap_or(Decimal::ZERO));
    let ap_base = round_money(total * rate);

    let mut entry = EntryBuilder::default();
    entry.debit(GRNI_ROLE, goods_base, None);
    entry.debit(
        OTHER_CHARGES_ROLE,
        charges_base,
        Some(&format!("Other charges on {}", inv.supplier_invoice_no)),
    );
    entry.credit(PPV_ROLE, discounts_base, Some("Header discount"));
    entry.credit(AP_ROLE, ap_base, None);
    // Component-wise conversion can drift a cent from the converted total.
    let gap = goods_base + charges_base - discounts_base - ap_base;
    entry.credit(ROUNDING_ROLE, gap, None);

    let mut lines = entry.lines();
    if lines.is_empty() {
        return Ok(None);
    }
    if mirror {
        lines = lines
            .into_iter()
            .map(|l| GlLine {
                account_role: l.account_role,
                debit: l.credit,
                credit: l.debit,
                memo: l.memo,
            })
            .collect();
    }

    let (action, memo, entry_date) = if mirror {
        (
            "cancel",
            format!(
                "Cancellation of purchase invoice {} ({})",
                inv.number.as_deref().unwrap_or("?"),
                inv.supplier_invoice_no
            ),
            chrono::Utc::now().date_naive(),
        )
    } else {
        (
            "post",
            format!(
                "Purchase invoice {} against {} ({})",
                inv.number.as_deref().unwrap_or("?"),
                order_row.number.as_deref().unwrap_or("purchase order"),
                inv.supplier_invoice_no
            ),
            inv.invoice_date,
        )
    };
    Ok(Some(GlPostingRequested {
        tenant_id,
        source: format!("scm.invoice:{}:{action}", inv.id),
        entry_date,
        memo,
        currency: None,
        lines,
    }))
}

// ---------------------------------------------------------------------------
// Acknowledgements and the sweeper
// ---------------------------------------------------------------------------

/// The database a booked/staged source lives in.
async fn resolve_db(
    tenants: &Option<Arc<TenantManager>>,
    main: &Option<DatabaseConnection>,
    tenant_id: Option<Uuid>,
) -> Result<Option<DatabaseConnection>> {
    match (tenant_id, tenants) {
        (Some(id), Some(tenants)) => match tenants.find_by_id(id).await? {
            Some(tenant) => Ok(Some(tenants.connection_for(&tenant).await?)),
            None => Ok(None),
        },
        _ => Ok(main.clone()),
    }
}

/// Clear outbox rows when accounting acknowledges their source. Public so
/// an integration harness can wire the port without registering the app.
pub fn subscribe_acks(ctx: &mut ModuleContext) {
    let tenants = ctx.tenants();
    let main = ctx.db().cloned();
    ctx.events().subscribe::<GlPostingBooked, _, _>(move |ev| {
        let tenants = tenants.clone();
        let main = main.clone();
        async move {
            // Only SCM sources are ours to clear.
            if !ev.source.starts_with("scm.") {
                return Ok(());
            }
            if let Some(db) = resolve_db(&tenants, &main, ev.tenant_id).await? {
                outbox::Entity::delete_by_id(&ev.source).exec(&db).await?;
                tracing::debug!(source = %ev.source, "GL outbox row cleared");
            }
            Ok(())
        }
    });
}

/// Re-publish staged requests that never got acknowledged (a crash between
/// commit and publish, accounting temporarily unable to book). Runs on a
/// plain interval so it works with or without the job system; the
/// subscriber's `source` dedup makes re-emission harmless.
pub(crate) fn spawn_sweeper(ctx: &mut ModuleContext) {
    let tenants = ctx.tenants();
    let main = ctx.db().cloned();
    let events = ctx.events();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(SWEEP_INTERVAL_SECS)).await;
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
                                    "GL sweep could not reach tenant database"),
                            }
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "GL sweep could not list tenants"),
                }
            }
            for (name, db) in dbs {
                if let Err(e) = sweep_one(&db, &events).await {
                    tracing::warn!(database = %name, error = %e, "GL outbox sweep failed");
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Reconciliation: does the GL agree with the stock ledger?
// ---------------------------------------------------------------------------

/// The stock world vs. the GL world, side by side. The GL columns are
/// `None` when no accounting schema lives in this database.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct GlReconciliationView {
    /// Total stock value across all levels (the engine's truth).
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub stock_value: Decimal,
    /// Net balance of every account carrying an inventory role in use.
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub inventory_account_balance: Option<Decimal>,
    /// `stock_value − inventory_account_balance`; zero when reconciled.
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub inventory_gap: Option<Decimal>,
    /// The operational GRNI position (received not billed, from order lines).
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub grni_open: Decimal,
    /// Credit balance of the GRNI account.
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub grni_account_balance: Option<Decimal>,
    /// `grni_open − grni_account_balance`; zero when reconciled.
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub grni_gap: Option<Decimal>,
    /// Posting requests still awaiting acknowledgement — a nonzero count
    /// explains a gap that will close on its own.
    pub pending_outbox: i64,
}

/// Compute the reconciliation on one (tenant) database.
///
/// The GL side reads the accounting tables with raw SQL — a deliberate,
/// contained seam: reconciling two bounded contexts is inherently a
/// cross-context read, and going through SQL (not code) keeps the apps
/// unlinked. `to_regclass` guards the case where no accounting schema
/// exists in this database.
pub async fn reconciliation(db: &DatabaseConnection) -> Result<GlReconciliationView> {
    let stock_value: Decimal = scalar(
        db,
        "SELECT COALESCE(SUM(value), 0)::numeric AS v FROM inventory_stock_levels",
        [],
    )
    .await?
    .unwrap_or(Decimal::ZERO);

    let grni_open = ProcurementQueries::new(db.clone()).grni().await?.total;

    let pending_outbox = outbox::Entity::find().count(db).await? as i64;

    let has_accounting = db
        .query_one(Statement::from_string(
            DbBackend::Postgres,
            "SELECT to_regclass('accounting_postings') IS NOT NULL AS present",
        ))
        .await?
        .map(|r| r.try_get::<bool>("", "present").unwrap_or(false))
        .unwrap_or(false);

    let (inventory_account_balance, grni_account_balance) = if has_accounting {
        // Every inventory role actually in use: the default plus any item
        // or category override.
        let mut roles: Vec<String> = vec![DEFAULT_INVENTORY_ROLE.to_string()];
        for r in item::Entity::find().all(db).await? {
            if let Some(role) = r.inventory_account_role {
                if !roles.contains(&role) {
                    roles.push(role);
                }
            }
        }
        for c in category::Entity::find().all(db).await? {
            if let Some(role) = c.inventory_account_role {
                if !roles.contains(&role) {
                    roles.push(role);
                }
            }
        }
        let inventory = scalar(
            db,
            "SELECT COALESCE(SUM(p.debit - p.credit), 0)::numeric AS v
             FROM accounting_postings p
             JOIN accounting_journal_entries e ON e.id = p.entry_id
             JOIN accounting_accounts a ON a.id = p.account_id
             WHERE e.status IN ('posted', 'reversed') AND a.system_key = ANY($1)",
            [roles.into()],
        )
        .await?
        .unwrap_or(Decimal::ZERO);
        let grni = scalar(
            db,
            "SELECT COALESCE(SUM(p.credit - p.debit), 0)::numeric AS v
             FROM accounting_postings p
             JOIN accounting_journal_entries e ON e.id = p.entry_id
             JOIN accounting_accounts a ON a.id = p.account_id
             WHERE e.status IN ('posted', 'reversed') AND a.system_key = $1",
            [GRNI_ROLE.into()],
        )
        .await?
        .unwrap_or(Decimal::ZERO);
        (Some(inventory), Some(grni))
    } else {
        (None, None)
    };

    Ok(GlReconciliationView {
        stock_value,
        inventory_account_balance,
        inventory_gap: inventory_account_balance.map(|b| stock_value - b),
        grni_open,
        grni_account_balance,
        grni_gap: grni_account_balance.map(|b| grni_open - b),
        pending_outbox,
    })
}

/// One numeric scalar out of a raw query.
async fn scalar<I>(
    db: &DatabaseConnection,
    sql: &str,
    values: I,
) -> Result<Option<Decimal>>
where
    I: IntoIterator<Item = sea_orm::Value>,
{
    let row = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            values,
        ))
        .await?;
    Ok(row.map(|r| r.try_get::<Decimal>("", "v").unwrap_or(Decimal::ZERO)))
}

// ---------------------------------------------------------------------------
// Reconciliation report + JSON endpoint
// ---------------------------------------------------------------------------

const RECONCILIATION_KEY: &str = "scm_gl_reconciliation";

pub struct GlReconciliationDataSource;

#[async_trait::async_trait]
impl ReportDataSource for GlReconciliationDataSource {
    fn key(&self) -> &'static str {
        RECONCILIATION_KEY
    }

    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = reconciliation(db).await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct GlReconciliationReport;

impl ReportDefinition for GlReconciliationReport {
    fn name(&self) -> &'static str {
        "gl-reconciliation"
    }

    fn title(&self) -> &'static str {
        "Stock / GL Reconciliation"
    }

    fn group(&self) -> &'static str {
        "Inventory"
    }

    fn default_format(&self) -> ReportFormat {
        ReportFormat::Compact
    }

    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf, ReportOutput::Excel, ReportOutput::Table]
    }

    fn permission(&self) -> Option<&'static str> {
        Some(crate::scm::inventory::permissions::names::REPORTS_VIEW)
    }

    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(GlReconciliationDataSource)]
    }

    fn build(&self, data: &ReportData) -> Result<Report> {
        let view: GlReconciliationView = data.get(RECONCILIATION_KEY)?;
        let money = |v: Decimal| format!("{:.2}", v);
        let opt = |v: Option<Decimal>| v.map(money).unwrap_or_else(|| "—".to_string());

        let table = Table::new(vec![
            ReportColumn::new("Measure"),
            ReportColumn::number("Operational"),
            ReportColumn::number("GL balance"),
            ReportColumn::number("Gap"),
        ])
        .title("Stock / GL Reconciliation")
        .row([
            "Inventory value".to_string(),
            money(view.stock_value),
            opt(view.inventory_account_balance),
            opt(view.inventory_gap),
        ])
        .row([
            "Goods received not invoiced".to_string(),
            money(view.grni_open),
            opt(view.grni_account_balance),
            opt(view.grni_gap),
        ])
        .row([
            "Pending GL requests".to_string(),
            view.pending_outbox.to_string(),
            String::new(),
            String::new(),
        ]);

        Ok(Report::new("Stock / GL Reconciliation")
            .subtitle("The stock engine's value against the ledger's, base currency")
            .with(table.into_widget()))
    }
}

pub(crate) fn routes() -> Router {
    Router::new().route("/inventory/reports/gl-reconciliation", get(reconciliation_json))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(reconciliation_json))]
struct ApiDoc;

#[utoipa::path(get, path = "/inventory/reports/gl-reconciliation", tag = "inventory",
    responses((status = 200, body = GlReconciliationView)))]
async fn reconciliation_json(
    authz: Authz,
    TenantDb(db): TenantDb,
) -> Result<Json<GlReconciliationView>> {
    authz
        .require(crate::scm::inventory::permissions::names::REPORTS_VIEW)
        .await?;
    reconciliation(&db).await.map(Json)
}

/// Re-publish every stale row of one database's outbox.
async fn sweep_one(db: &DatabaseConnection, events: &Events) -> Result<()> {
    use sea_orm::Condition;
    let cutoff = chrono::Utc::now() - chrono::Duration::seconds(SWEEP_GRACE_SECS);
    let stale = outbox::Entity::find()
        .filter(
            Condition::any()
                .add(
                    outbox::Column::LastAttemptAt
                        .is_null()
                        .and(outbox::Column::CreatedAt.lt(cutoff)),
                )
                .add(outbox::Column::LastAttemptAt.lt(cutoff)),
        )
        .order_by_asc(outbox::Column::CreatedAt)
        .all(db)
        .await?;
    for row in stale {
        let req: GlPostingRequested = match serde_json::from_value(row.payload.clone()) {
            Ok(req) => req,
            Err(e) => {
                tracing::error!(source = %row.source, error = %e,
                    "GL outbox payload is unreadable; leaving the row for inspection");
                continue;
            }
        };
        let attempts = row.attempts + 1;
        tracing::warn!(source = %row.source, attempts,
            "re-publishing an unacknowledged GL posting request");
        let mut active: outbox::ActiveModel = row.into();
        active.attempts = Set(attempts);
        active.last_attempt_at = Set(Some(chrono::Utc::now()));
        active.update(db).await?;
        events.publish(req).await;
    }
    Ok(())
}
