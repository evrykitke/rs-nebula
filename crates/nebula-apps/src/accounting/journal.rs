//! Double-entry journal: entries and their debit/credit postings, plus
//! the service that enforces the bookkeeping invariants.
//!
//! An entry is created as a **draft** (editable and deletable,
//! unnumbered), then **posted** — the point at which it must balance, is
//! assigned a gap-free number from the `accounting.journal` series, and
//! becomes immutable. A posted entry is corrected only by a **reversal**:
//! a new mirror entry (debits and credits swapped) that is posted
//! immediately and linked back to the original.
//!
//! All rows live in the request's tenant database; numbering and the
//! status change happen inside one transaction so a number is never
//! burned by an entry that fails to post.

use crate::accounting::account;
use crate::accounting::permissions::names;
use axum::extract::Path;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use nebula::audit::Audit;
use nebula::auth::Authz;
use nebula::error::{Error, Result};
use nebula::{Numbering, TenantDb, sea_orm};
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{
    ConnectionTrait, DatabaseConnection, FromQueryResult, QueryOrder, QuerySelect, Set,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Where a journal entry is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EntryStatus {
    /// Being composed; editable, not yet in the ledger.
    Draft,
    /// Committed to the ledger; immutable.
    Posted,
    /// Corrected by a reversal entry.
    Reversed,
}

impl EntryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryStatus::Draft => "draft",
            EntryStatus::Posted => "posted",
            EntryStatus::Reversed => "reversed",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "draft" => Ok(EntryStatus::Draft),
            "posted" => Ok(EntryStatus::Posted),
            "reversed" => Ok(EntryStatus::Reversed),
            other => Err(Error::internal(format!("unknown entry status {other:?}"))),
        }
    }
}

/// The journal entry entity.
pub mod entry {
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "accounting_journal_entries")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub number: Option<String>,
        pub entry_date: Date,
        pub memo: String,
        pub reference: Option<String>,
        pub currency: String,
        pub status: String,
        pub reverses_id: Option<Uuid>,
        pub reversed_by_id: Option<Uuid>,
        pub posted_at: Option<DateTimeUtc>,
        pub created_at: DateTimeUtc,
        pub created_by: Option<Uuid>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// A single debit/credit line of an entry.
pub mod posting {
    use sea_orm::entity::prelude::*;
    use serde::Serialize;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
    #[sea_orm(table_name = "accounting_postings")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub entry_id: Uuid,
        pub account_id: Uuid,
        pub line_no: i32,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))")]
        pub debit: Decimal,
        #[sea_orm(column_type = "Decimal(Some((20, 4)))")]
        pub credit: Decimal,
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

/// A debit/credit line as supplied by a caller.
pub struct PostingInput {
    pub account_id: Uuid,
    pub debit: Decimal,
    pub credit: Decimal,
    pub memo: Option<String>,
}

/// A new draft entry as supplied by a caller.
pub struct NewEntry {
    pub entry_date: chrono::NaiveDate,
    pub memo: String,
    pub reference: Option<String>,
    pub currency: String,
    pub lines: Vec<PostingInput>,
    pub created_by: Option<Uuid>,
}

/// The double-entry ledger service over one (tenant) connection.
pub struct Ledger {
    db: DatabaseConnection,
}

impl Ledger {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Create a draft entry. Line shape and account/currency validity are
    /// checked now; the balancing rule is enforced at posting time so a
    /// work-in-progress can be saved.
    pub async fn create_draft(&self, new: NewEntry) -> Result<JournalEntryView> {
        if new.memo.trim().is_empty() {
            return Err(Error::Validation("an entry needs a memo".into()));
        }
        nebula::Currency::new(&new.currency, 2)?;
        validate_lines(&self.db, &new.currency, &new.lines).await?;

        let txn = self.db.begin().await?;
        let entry_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        entry::ActiveModel {
            id: Set(entry_id),
            number: Set(None),
            entry_date: Set(new.entry_date),
            memo: Set(new.memo.trim().to_string()),
            reference: Set(new.reference.filter(|r| !r.trim().is_empty())),
            currency: Set(new.currency),
            status: Set(EntryStatus::Draft.as_str().to_string()),
            reverses_id: Set(None),
            reversed_by_id: Set(None),
            posted_at: Set(None),
            created_at: Set(now),
            created_by: Set(new.created_by),
        }
        .insert(&txn)
        .await?;
        insert_postings(&txn, entry_id, &new.lines).await?;
        txn.commit().await?;
        self.view(entry_id).await
    }

    /// Replace a draft's header fields and lines wholesale. Only a draft
    /// may change; posting is the point of no return.
    pub async fn update_draft(&self, id: Uuid, new: NewEntry) -> Result<JournalEntryView> {
        if new.memo.trim().is_empty() {
            return Err(Error::Validation("an entry needs a memo".into()));
        }
        nebula::Currency::new(&new.currency, 2)?;

        let txn = self.db.begin().await?;
        let entry = load_entry_locked(&txn, id).await?;
        if EntryStatus::parse(&entry.status)? != EntryStatus::Draft {
            return Err(Error::Validation("only a draft entry can be edited".into()));
        }
        validate_lines(&txn, &new.currency, &new.lines).await?;

        posting::Entity::delete_many()
            .filter(posting::Column::EntryId.eq(id))
            .exec(&txn)
            .await?;
        insert_postings(&txn, id, &new.lines).await?;

        let mut active: entry::ActiveModel = entry.into();
        active.entry_date = Set(new.entry_date);
        active.memo = Set(new.memo.trim().to_string());
        active.reference = Set(new.reference.filter(|r| !r.trim().is_empty()));
        active.currency = Set(new.currency);
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Delete a draft (its lines cascade). A posted or reversed entry is
    /// part of the ledger's history and can never be deleted.
    pub async fn delete_draft(&self, id: Uuid) -> Result<JournalEntryView> {
        let view = self.view(id).await?;
        let txn = self.db.begin().await?;
        let entry = load_entry_locked(&txn, id).await?;
        if EntryStatus::parse(&entry.status)? != EntryStatus::Draft {
            return Err(Error::Validation("only a draft entry can be deleted".into()));
        }
        entry::Entity::delete_by_id(id).exec(&txn).await?;
        txn.commit().await?;
        Ok(view)
    }

    /// Post a draft: enforce the balancing rule, allocate a gap-free
    /// number and freeze the entry — all in one transaction. The entry row
    /// is locked so two concurrent posts cannot both succeed, and the
    /// lines are re-validated because the chart may have changed since the
    /// draft was written (an account deactivated, sub-accounts added).
    pub async fn post(&self, id: Uuid, numbering: &Numbering) -> Result<JournalEntryView> {
        let txn = self.db.begin().await?;
        let entry = load_entry_locked(&txn, id).await?;
        if EntryStatus::parse(&entry.status)? != EntryStatus::Draft {
            return Err(Error::Validation("only a draft entry can be posted".into()));
        }
        super::fiscal::ensure_open_for_post(&txn, entry.entry_date).await?;
        let lines = load_postings(&txn, id).await?;
        check_balanced(&lines)?;
        let inputs: Vec<PostingInput> = lines
            .iter()
            .map(|l| PostingInput {
                account_id: l.account_id,
                debit: l.debit,
                credit: l.credit,
                memo: None,
            })
            .collect();
        validate_lines(&txn, &entry.currency, &inputs).await?;

        let number = numbering.next(&txn, super::JOURNAL_SERIES).await?;
        let mut active: entry::ActiveModel = entry.into();
        active.status = Set(EntryStatus::Posted.as_str().to_string());
        active.number = Set(Some(number.formatted));
        active.posted_at = Set(Some(chrono::Utc::now()));
        active.update(&txn).await?;
        txn.commit().await?;
        self.view(id).await
    }

    /// Create an entry and post it in one transaction, numbered from
    /// `series` — for source documents (an expense voucher, a POS sale)
    /// whose entries are born final rather than composed as drafts. The
    /// lines must balance up front; nothing is written otherwise.
    pub async fn create_posted(
        &self,
        new: NewEntry,
        series: &str,
        numbering: &Numbering,
    ) -> Result<JournalEntryView> {
        let txn = self.db.begin().await?;
        let entry_id = Self::create_posted_in(&txn, new, series, numbering).await?;
        txn.commit().await?;
        self.view(entry_id).await
    }

    /// The transaction-scoped body of [`Ledger::create_posted`], for
    /// callers that must couple the entry with their own writes or locks
    /// (the GL port books under an idempotency lock). Nothing is written
    /// unless the caller's transaction commits.
    pub async fn create_posted_in(
        txn: &sea_orm::DatabaseTransaction,
        new: NewEntry,
        series: &str,
        numbering: &Numbering,
    ) -> Result<Uuid> {
        if new.memo.trim().is_empty() {
            return Err(Error::Validation("an entry needs a memo".into()));
        }
        nebula::Currency::new(&new.currency, 2)?;

        validate_lines(txn, &new.currency, &new.lines).await?;
        let debits: Decimal = new.lines.iter().map(|l| l.debit).sum();
        let credits: Decimal = new.lines.iter().map(|l| l.credit).sum();
        if debits != credits {
            return Err(Error::Validation(format!(
                "entry does not balance: debits {debits} ≠ credits {credits}"
            )));
        }
        if debits == Decimal::ZERO {
            return Err(Error::Validation(
                "entry total must be greater than zero".into(),
            ));
        }
        super::fiscal::ensure_open_for_post(txn, new.entry_date).await?;

        let entry_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        let number = numbering.next(txn, series).await?;
        entry::ActiveModel {
            id: Set(entry_id),
            number: Set(Some(number.formatted)),
            entry_date: Set(new.entry_date),
            memo: Set(new.memo.trim().to_string()),
            reference: Set(new.reference.filter(|r| !r.trim().is_empty())),
            currency: Set(new.currency),
            status: Set(EntryStatus::Posted.as_str().to_string()),
            reverses_id: Set(None),
            reversed_by_id: Set(None),
            posted_at: Set(Some(now)),
            created_at: Set(now),
            created_by: Set(new.created_by),
        }
        .insert(txn)
        .await?;
        insert_postings(txn, entry_id, &new.lines).await?;
        Ok(entry_id)
    }

    /// Reverse a posted entry: create a mirror entry (debits and credits
    /// swapped), post it immediately, and link the two together. The
    /// original moves to `reversed`; neither entry is ever mutated again.
    /// The reversal is dated `entry_date` (defaulting to today) — never
    /// before the original, and always in an open period. The original's
    /// row is locked so an entry cannot be reversed twice concurrently.
    pub async fn reverse(
        &self,
        id: Uuid,
        reason: &str,
        entry_date: Option<chrono::NaiveDate>,
        numbering: &Numbering,
        created_by: Option<Uuid>,
    ) -> Result<JournalEntryView> {
        let txn = self.db.begin().await?;
        let original = load_entry_locked(&txn, id).await?;
        match EntryStatus::parse(&original.status)? {
            EntryStatus::Posted => {}
            EntryStatus::Draft => {
                return Err(Error::Validation(
                    "a draft entry has not been posted and cannot be reversed".into(),
                ));
            }
            EntryStatus::Reversed => {
                return Err(Error::Validation("entry is already reversed".into()));
            }
        }
        let lines = load_postings(&txn, id).await?;

        let now = chrono::Utc::now();
        let reversal_date = entry_date.unwrap_or_else(|| now.date_naive());
        if reversal_date < original.entry_date {
            return Err(Error::Validation(
                "a reversal cannot be dated before the entry it reverses".into(),
            ));
        }
        super::fiscal::ensure_open_for_post(&txn, reversal_date).await?;
        let reversal_id = Uuid::new_v4();
        let number = numbering.next(&txn, super::JOURNAL_SERIES).await?;
        let memo = if reason.trim().is_empty() {
            format!(
                "Reversal of {}",
                original.number.as_deref().unwrap_or("entry")
            )
        } else {
            format!(
                "Reversal of {}: {}",
                original.number.as_deref().unwrap_or("entry"),
                reason.trim()
            )
        };
        entry::ActiveModel {
            id: Set(reversal_id),
            number: Set(Some(number.formatted)),
            entry_date: Set(reversal_date),
            memo: Set(memo),
            reference: Set(original.number.clone()),
            currency: Set(original.currency.clone()),
            status: Set(EntryStatus::Posted.as_str().to_string()),
            reverses_id: Set(Some(original.id)),
            reversed_by_id: Set(None),
            posted_at: Set(Some(now)),
            created_at: Set(now),
            created_by: Set(created_by),
        }
        .insert(&txn)
        .await?;

        // Mirror every line with the sides swapped.
        let mirrored: Vec<PostingInput> = lines
            .iter()
            .map(|l| PostingInput {
                account_id: l.account_id,
                debit: l.credit,
                credit: l.debit,
                memo: l.memo.clone(),
            })
            .collect();
        insert_postings(&txn, reversal_id, &mirrored).await?;

        let mut active: entry::ActiveModel = original.into();
        active.status = Set(EntryStatus::Reversed.as_str().to_string());
        active.reversed_by_id = Set(Some(reversal_id));
        active.update(&txn).await?;

        txn.commit().await?;
        self.view(reversal_id).await
    }

    pub async fn list(&self, status: Option<EntryStatus>) -> Result<Vec<JournalEntryHeader>> {
        let mut query = entry::Entity::find();
        if let Some(status) = status {
            query = query.filter(entry::Column::Status.eq(status.as_str()));
        }
        let rows = query
            .order_by_desc(entry::Column::EntryDate)
            .order_by_desc(entry::Column::CreatedAt)
            .all(&self.db)
            .await?;
        self.headers(rows).await
    }

    /// Build register rows (header + amount) for a set of entries, with
    /// the debit totals summed per entry in one pass.
    pub(crate) async fn headers(
        &self,
        rows: Vec<entry::Model>,
    ) -> Result<Vec<JournalEntryHeader>> {
        let totals = self.debit_totals(&rows).await?;
        rows.into_iter()
            .map(|r| {
                let total = totals.get(&r.id).copied().unwrap_or(Decimal::ZERO);
                header(r, total)
            })
            .collect()
    }

    /// Load a full entry with its lines and account labels.
    pub async fn view(&self, id: Uuid) -> Result<JournalEntryView> {
        let entry = load_entry(&self.db, id).await?;
        let lines = load_postings(&self.db, id).await?;
        let accounts = self
            .account_labels(lines.iter().map(|l| l.account_id))
            .await?;

        let mut total_debit = Decimal::ZERO;
        let mut total_credit = Decimal::ZERO;
        let mut line_views = Vec::with_capacity(lines.len());
        for l in lines {
            total_debit += l.debit;
            total_credit += l.credit;
            let label = accounts.get(&l.account_id);
            line_views.push(PostingView {
                id: l.id,
                account_id: l.account_id,
                account_code: label.map(|(c, _)| c.clone()).unwrap_or_default(),
                account_name: label.map(|(_, n)| n.clone()).unwrap_or_default(),
                line_no: l.line_no,
                debit: l.debit,
                credit: l.credit,
                memo: l.memo,
            });
        }
        Ok(JournalEntryView {
            id: entry.id,
            number: entry.number,
            entry_date: entry.entry_date,
            memo: entry.memo,
            reference: entry.reference,
            currency: entry.currency,
            status: EntryStatus::parse(&entry.status)?,
            reverses_id: entry.reverses_id,
            reversed_by_id: entry.reversed_by_id,
            posted_at: entry.posted_at,
            created_at: entry.created_at,
            lines: line_views,
            total_debit,
            total_credit,
        })
    }

    /// `account_id -> (code, name)` for the given ids.
    async fn account_labels(
        &self,
        ids: impl Iterator<Item = Uuid>,
    ) -> Result<HashMap<Uuid, (String, String)>> {
        let ids: Vec<Uuid> = ids.collect();
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = account::Entity::find()
            .filter(account::Column::Id.is_in(ids))
            .all(&self.db)
            .await?;
        Ok(rows.into_iter().map(|a| (a.id, (a.code, a.name))).collect())
    }

    /// Total debit per entry, for the register list — summed in the
    /// database, one row per entry.
    async fn debit_totals(&self, entries: &[entry::Model]) -> Result<HashMap<Uuid, Decimal>> {
        #[derive(FromQueryResult)]
        struct EntryTotal {
            entry_id: Uuid,
            total: Decimal,
        }

        let ids: Vec<Uuid> = entries.iter().map(|e| e.id).collect();
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = posting::Entity::find()
            .select_only()
            .column(posting::Column::EntryId)
            .column_as(posting::Column::Debit.sum(), "total")
            .filter(posting::Column::EntryId.is_in(ids))
            .group_by(posting::Column::EntryId)
            .into_model::<EntryTotal>()
            .all(&self.db)
            .await?;
        Ok(rows.into_iter().map(|r| (r.entry_id, r.total)).collect())
    }
}

/// Validate lines: each is non-negative, single-sided, within the ledger's
/// two-decimal precision, and posts to an existing, active, non-header
/// account in the entry's currency.
async fn validate_lines<C: ConnectionTrait>(
    conn: &C,
    currency: &str,
    lines: &[PostingInput],
) -> Result<()> {
    if lines.len() < 2 {
        return Err(Error::Validation(
            "an entry needs at least two postings (one debit, one credit)".into(),
        ));
    }
    for line in lines {
        if line.debit < Decimal::ZERO || line.credit < Decimal::ZERO {
            return Err(Error::Validation(
                "posting amounts must not be negative".into(),
            ));
        }
        let debit_set = line.debit > Decimal::ZERO;
        let credit_set = line.credit > Decimal::ZERO;
        if debit_set == credit_set {
            return Err(Error::Validation(
                "each posting must be exactly one of a debit or a credit".into(),
            ));
        }
        if line.debit.normalize().scale() > 2 || line.credit.normalize().scale() > 2 {
            return Err(Error::Validation(
                "posting amounts must not have more than two decimal places".into(),
            ));
        }
    }

    let ids: Vec<Uuid> = lines.iter().map(|l| l.account_id).collect();
    let accounts: HashMap<Uuid, account::Model> = account::Entity::find()
        .filter(account::Column::Id.is_in(ids.clone()))
        .all(conn)
        .await?
        .into_iter()
        .map(|a| (a.id, a))
        .collect();
    // An account with sub-accounts is a header: it organizes the chart and
    // never carries postings itself.
    let headers: HashSet<Uuid> = account::Entity::find()
        .filter(account::Column::ParentId.is_in(ids))
        .all(conn)
        .await?
        .into_iter()
        .filter_map(|a| a.parent_id)
        .collect();

    for line in lines {
        let Some(acc) = accounts.get(&line.account_id) else {
            return Err(Error::NotFound(format!("account {}", line.account_id)));
        };
        if !acc.is_active {
            return Err(Error::Validation(format!(
                "account {} is inactive and cannot be posted to",
                acc.code
            )));
        }
        if headers.contains(&acc.id) {
            return Err(Error::Validation(format!(
                "account {} is a header with sub-accounts; post to one of its sub-accounts",
                acc.code
            )));
        }
        if acc.currency != currency {
            return Err(Error::Validation(format!(
                "account {} is in {}, but the entry is in {currency}",
                acc.code, acc.currency
            )));
        }
    }
    Ok(())
}

/// Insert a set of lines for an entry, numbered from one.
async fn insert_postings<C: ConnectionTrait>(
    conn: &C,
    entry_id: Uuid,
    lines: &[PostingInput],
) -> Result<()> {
    let now = chrono::Utc::now();
    for (i, line) in lines.iter().enumerate() {
        posting::ActiveModel {
            id: Set(Uuid::new_v4()),
            entry_id: Set(entry_id),
            account_id: Set(line.account_id),
            line_no: Set((i + 1) as i32),
            debit: Set(line.debit),
            credit: Set(line.credit),
            memo: Set(line.memo.clone().filter(|m| !m.trim().is_empty())),
            created_at: Set(now),
        }
        .insert(conn)
        .await?;
    }
    Ok(())
}

async fn load_entry<C: ConnectionTrait>(conn: &C, id: Uuid) -> Result<entry::Model> {
    entry::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("journal entry {id}")))
}

/// Load an entry holding its row lock, so concurrent state transitions
/// (post, reverse, edit, delete) on the same entry serialize instead of
/// both acting on the same stale status.
async fn load_entry_locked(
    txn: &sea_orm::DatabaseTransaction,
    id: Uuid,
) -> Result<entry::Model> {
    entry::Entity::find_by_id(id)
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(|| Error::NotFound(format!("journal entry {id}")))
}

async fn load_postings<C: ConnectionTrait>(
    conn: &C,
    entry_id: Uuid,
) -> Result<Vec<posting::Model>> {
    posting::Entity::find()
        .filter(posting::Column::EntryId.eq(entry_id))
        .order_by_asc(posting::Column::LineNo)
        .all(conn)
        .await
        .map_err(Error::from)
}

/// The golden rule: at least two lines and total debits == total credits,
/// with a non-zero total.
fn check_balanced(lines: &[posting::Model]) -> Result<()> {
    if lines.len() < 2 {
        return Err(Error::Validation(
            "an entry needs at least two postings to post".into(),
        ));
    }
    let debits: Decimal = lines.iter().map(|l| l.debit).sum();
    let credits: Decimal = lines.iter().map(|l| l.credit).sum();
    if debits != credits {
        return Err(Error::Validation(format!(
            "entry does not balance: debits {debits} ≠ credits {credits}"
        )));
    }
    if debits == Decimal::ZERO {
        return Err(Error::Validation(
            "entry total must be greater than zero".into(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Views (API DTOs)
// ---------------------------------------------------------------------------

#[derive(Serialize, utoipa::ToSchema)]
pub struct PostingView {
    pub id: Uuid,
    pub account_id: Uuid,
    pub account_code: String,
    pub account_name: String,
    pub line_no: i32,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub debit: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub credit: Decimal,
    pub memo: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct JournalEntryView {
    pub id: Uuid,
    pub number: Option<String>,
    #[schema(value_type = String, format = Date)]
    pub entry_date: chrono::NaiveDate,
    pub memo: String,
    pub reference: Option<String>,
    pub currency: String,
    pub status: EntryStatus,
    pub reverses_id: Option<Uuid>,
    pub reversed_by_id: Option<Uuid>,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub posted_at: Option<chrono::DateTime<chrono::Utc>>,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub lines: Vec<PostingView>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total_debit: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total_credit: Decimal,
}

/// A row of the journal register (entry without its lines).
#[derive(Serialize, utoipa::ToSchema)]
pub struct JournalEntryHeader {
    pub id: Uuid,
    pub number: Option<String>,
    #[schema(value_type = String, format = Date)]
    pub entry_date: chrono::NaiveDate,
    pub memo: String,
    pub reference: Option<String>,
    pub currency: String,
    pub status: EntryStatus,
    /// Total debit (== total credit for a posted entry).
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub amount: Decimal,
}

fn header(row: entry::Model, amount: Decimal) -> Result<JournalEntryHeader> {
    Ok(JournalEntryHeader {
        id: row.id,
        number: row.number,
        entry_date: row.entry_date,
        memo: row.memo,
        reference: row.reference,
        currency: row.currency,
        status: EntryStatus::parse(&row.status)?,
        amount,
    })
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

pub(super) fn routes() -> Router {
    Router::new()
        .route("/accounting/journal", get(list_entries).post(create_entry))
        .route(
            "/accounting/journal/{id}",
            get(get_entry).put(update_entry).delete(delete_entry),
        )
        .route("/accounting/journal/{id}/post", post(post_entry))
        .route("/accounting/journal/{id}/reverse", post(reverse_entry))
}

pub(super) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    list_entries,
    get_entry,
    create_entry,
    update_entry,
    delete_entry,
    post_entry,
    reverse_entry
))]
struct ApiDoc;

#[derive(Deserialize, utoipa::ToSchema)]
pub struct PostingInputRequest {
    pub account_id: Uuid,
    /// Non-negative; exactly one of debit/credit is set on a line.
    #[serde(default, with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub debit: Decimal,
    #[serde(default, with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub credit: Decimal,
    pub memo: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateEntryRequest {
    #[schema(value_type = String, format = Date)]
    pub entry_date: chrono::NaiveDate,
    pub memo: String,
    pub reference: Option<String>,
    /// ISO 4217 code; every line's account must be in this currency.
    pub currency: String,
    pub lines: Vec<PostingInputRequest>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ReverseEntryRequest {
    #[serde(default)]
    pub reason: String,
    /// The reversal's date; defaults to today. Never before the original
    /// entry's date, and its period must be open.
    #[schema(value_type = Option<String>, format = Date)]
    pub entry_date: Option<chrono::NaiveDate>,
}

/// Optional `?status=draft|posted|reversed` register filter.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct ListEntriesQuery {
    pub status: Option<EntryStatus>,
}

#[utoipa::path(get, path = "/accounting/journal", tag = "accounting",
    params(("status" = Option<EntryStatus>, Query, description = "Filter by status")),
    responses((status = 200, body = Vec<JournalEntryHeader>)))]
async fn list_entries(
    authz: Authz,
    TenantDb(db): TenantDb,
    axum::extract::Query(q): axum::extract::Query<ListEntriesQuery>,
) -> Result<Json<Vec<JournalEntryHeader>>> {
    authz.require(names::JOURNAL_VIEW).await?;
    Ledger::new(db).list(q.status).await.map(Json)
}

#[utoipa::path(get, path = "/accounting/journal/{id}", tag = "accounting",
    params(("id" = Uuid, Path, description = "Entry id")),
    responses((status = 200, body = JournalEntryView)))]
async fn get_entry(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<JournalEntryView>> {
    authz.require(names::JOURNAL_VIEW).await?;
    Ledger::new(db).view(id).await.map(Json)
}

#[utoipa::path(post, path = "/accounting/journal", tag = "accounting",
    request_body = CreateEntryRequest,
    responses((status = 200, body = JournalEntryView)))]
async fn create_entry(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Json(req): Json<CreateEntryRequest>,
) -> Result<Json<JournalEntryView>> {
    authz.require(names::JOURNAL_CREATE).await?;
    let view = Ledger::new(db)
        .create_draft(new_entry(req, Some(authz.user.id)))
        .await?;
    audit.0.created("accounting.journal", view.id, &view).await;
    Ok(Json(view))
}

/// Map a request body onto the service input.
fn new_entry(req: CreateEntryRequest, created_by: Option<Uuid>) -> NewEntry {
    NewEntry {
        entry_date: req.entry_date,
        memo: req.memo,
        reference: req.reference,
        currency: req.currency,
        lines: req
            .lines
            .into_iter()
            .map(|l| PostingInput {
                account_id: l.account_id,
                debit: l.debit,
                credit: l.credit,
                memo: l.memo,
            })
            .collect(),
        created_by,
    }
}

#[utoipa::path(put, path = "/accounting/journal/{id}", tag = "accounting",
    params(("id" = Uuid, Path, description = "Entry id")),
    request_body = CreateEntryRequest,
    responses((status = 200, body = JournalEntryView)))]
async fn update_entry(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Json(req): Json<CreateEntryRequest>,
) -> Result<Json<JournalEntryView>> {
    authz.require(names::JOURNAL_CREATE).await?;
    let ledger = Ledger::new(db);
    let before = ledger.view(id).await?;
    let after = ledger.update_draft(id, new_entry(req, None)).await?;
    audit
        .0
        .updated("accounting.journal", id, &before, &after)
        .await;
    Ok(Json(after))
}

#[utoipa::path(delete, path = "/accounting/journal/{id}", tag = "accounting",
    params(("id" = Uuid, Path, description = "Entry id")),
    responses((status = 200, body = JournalEntryView)))]
async fn delete_entry(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
) -> Result<Json<JournalEntryView>> {
    authz.require(names::JOURNAL_CREATE).await?;
    let view = Ledger::new(db).delete_draft(id).await?;
    audit.0.deleted("accounting.journal", view.id, &view).await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/accounting/journal/{id}/post", tag = "accounting",
    params(("id" = Uuid, Path, description = "Entry id")),
    responses((status = 200, body = JournalEntryView)))]
async fn post_entry(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Extension(numbering): Extension<Numbering>,
    Path(id): Path<Uuid>,
) -> Result<Json<JournalEntryView>> {
    authz.require(names::JOURNAL_POST).await?;
    let view = Ledger::new(db).post(id, &numbering).await?;
    audit
        .0
        .event(format!(
            "posted journal entry {}",
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}

#[utoipa::path(post, path = "/accounting/journal/{id}/reverse", tag = "accounting",
    params(("id" = Uuid, Path, description = "Entry id")),
    request_body = ReverseEntryRequest,
    responses((status = 200, body = JournalEntryView)))]
async fn reverse_entry(
    authz: Authz,
    audit: Audit,
    TenantDb(db): TenantDb,
    Extension(numbering): Extension<Numbering>,
    Path(id): Path<Uuid>,
    Json(req): Json<ReverseEntryRequest>,
) -> Result<Json<JournalEntryView>> {
    authz.require(names::JOURNAL_REVERSE).await?;
    let view = Ledger::new(db)
        .reverse(id, &req.reason, req.entry_date, &numbering, Some(authz.user.id))
        .await?;
    audit
        .0
        .event(format!(
            "reversed journal entry {} with {}",
            req.reason,
            view.number.as_deref().unwrap_or("")
        ))
        .await;
    Ok(Json(view))
}
