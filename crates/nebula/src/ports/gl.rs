//! The general-ledger posting port: how a module with financial side
//! effects asks *whichever app keeps the books* to record them.
//!
//! A stock module receiving goods, a POS closing a sale, a payroll run —
//! all of them produce balanced debit/credit effects but must not know the
//! tenant's chart of accounts, or even that an accounting app is
//! installed. They publish a [`GlPostingRequested`] naming **account
//! roles** (stable seeded-account keys like `"inventory"`, `"grni"`,
//! `"ap"`), and the bookkeeping subscriber resolves roles to real accounts
//! and books a journal entry.
//!
//! Delivery is the event bus's: eventually consistent with the source
//! document, at-most-once per publish. Publishers that need the request to
//! survive a crash keep it in their own outbox and re-publish until the
//! subscriber answers with [`GlPostingBooked`]; the subscriber deduplicates
//! on [`GlPostingRequested::source`], so re-emission is always safe.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One debit/credit line of a requested entry. Amounts are positive, in
/// the tenant's base currency, at ledger precision (2 decimals); exactly
/// one side is set per line.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GlLine {
    /// A role key the subscriber resolves against seeded accounts
    /// (e.g. `"inventory"`, `"grni"`, `"cogs"`, `"ap"`). The publisher
    /// never sees account ids.
    pub account_role: String,
    pub debit: Decimal,
    pub credit: Decimal,
    pub memo: Option<String>,
}

impl GlLine {
    /// A debit line on `role`.
    pub fn debit(role: impl Into<String>, amount: Decimal, memo: Option<String>) -> Self {
        Self {
            account_role: role.into(),
            debit: amount,
            credit: Decimal::ZERO,
            memo,
        }
    }

    /// A credit line on `role`.
    pub fn credit(role: impl Into<String>, amount: Decimal, memo: Option<String>) -> Self {
        Self {
            account_role: role.into(),
            debit: Decimal::ZERO,
            credit: amount,
            memo,
        }
    }
}

/// A request that the bookkeeping app record a balanced journal entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GlPostingRequested {
    /// The tenant whose books the entry belongs in; `None` in
    /// single-tenant deployments (the main database).
    pub tenant_id: Option<Uuid>,
    /// Idempotency key, `"{module}.{document}:{id}:{action}"` — the
    /// subscriber books each source exactly once, so duplicate delivery
    /// and outbox re-emission are harmless.
    pub source: String,
    pub entry_date: chrono::NaiveDate,
    pub memo: String,
    /// Base-currency ISO code when the publisher knows it; the subscriber
    /// falls back to the resolved accounts' own currency otherwise.
    pub currency: Option<String>,
    pub lines: Vec<GlLine>,
}

impl crate::events::Event for GlPostingRequested {
    const NAME: &'static str = "gl.posting_requested";
}

/// The bookkeeping app's answer: `source` is on the books (whether this
/// delivery booked it or an earlier one already had). Publishers keeping
/// an outbox clear the row on this event.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GlPostingBooked {
    pub tenant_id: Option<Uuid>,
    /// The idempotency key of the request this acknowledges.
    pub source: String,
    /// The journal entry holding the postings; `None` when the request
    /// nets to zero and there is nothing to book.
    pub entry_id: Option<Uuid>,
}

impl crate::events::Event for GlPostingBooked {
    const NAME: &'static str = "gl.posting_booked";
}
