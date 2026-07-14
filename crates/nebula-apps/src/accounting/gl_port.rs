//! The bookkeeping end of the framework's GL posting port.
//!
//! Other apps (SCM today, sales/POS tomorrow) publish
//! [`GlPostingRequested`] events naming **account roles** — the stable
//! [`account::keys`](crate::accounting::account::keys) the seed installs —
//! and this subscriber turns each request into one posted journal entry
//! under the `accounting.system` series.
//!
//! Guarantees, in order of importance:
//!
//! - **Exactly once per source.** The request's `source` is stored as the
//!   entry's `reference`; booking runs inside a transaction that first
//!   takes a per-source Postgres advisory lock and re-checks for an
//!   existing entry, so duplicate delivery — including a sweep re-emission
//!   racing the original — books nothing twice.
//! - **Always answered.** Success (fresh or duplicate) publishes
//!   [`GlPostingBooked`] so outbox-keeping publishers can clear the row.
//!   Failure (unseeded role, closed period, unreachable tenant DB) is
//!   logged and *not* acked — the publisher's sweep will try again.
//! - **Zero-total requests are acked without booking** (`entry_id: None`):
//!   a zero-value document has nothing to say to the ledger.

use crate::accounting::account;
use crate::accounting::journal::{Ledger, NewEntry, PostingInput, entry};
use nebula::error::{Error, Result};
use nebula::ports::gl::{GlPostingBooked, GlPostingRequested};
use nebula::sea_orm;
use nebula::tenancy::TenantManager;
use nebula::{Events, ModuleContext, NumberingHandle};
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, DbBackend, Statement, TransactionTrait};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// The accounting app's GL-port subscriber state.
pub struct GlPort {
    tenants: Option<Arc<TenantManager>>,
    main_db: Option<DatabaseConnection>,
    events: Events,
    numbering: NumberingHandle,
}

impl GlPort {
    /// Wire the subscriber during `configure`. Public so an integration
    /// harness can wire the port without registering the whole app.
    pub fn subscribe(ctx: &mut ModuleContext) {
        let port = Arc::new(GlPort {
            tenants: ctx.tenants(),
            main_db: ctx.db().cloned(),
            events: ctx.events(),
            numbering: ctx.numbering(),
        });
        ctx.events().subscribe::<GlPostingRequested, _, _>(move |ev| {
            let port = port.clone();
            async move { port.handle(ev).await }
        });
    }

    async fn handle(&self, req: GlPostingRequested) -> Result<()> {
        let db = self.connection(req.tenant_id).await?;
        let entry_id = self.book(&db, &req).await?;
        self.events
            .publish(GlPostingBooked {
                tenant_id: req.tenant_id,
                source: req.source.clone(),
                entry_id,
            })
            .await;
        Ok(())
    }

    /// The database whose books the request belongs in.
    async fn connection(&self, tenant_id: Option<Uuid>) -> Result<DatabaseConnection> {
        match (tenant_id, &self.tenants) {
            (Some(id), Some(tenants)) => {
                let tenant = tenants
                    .find_by_id(id)
                    .await?
                    .ok_or_else(|| Error::NotFound(format!("tenant {id}")))?;
                tenants.connection_for(&tenant).await
            }
            (None, _) => self.main_db.clone().ok_or_else(|| {
                Error::internal("a tenantless GL posting needs a main database")
            }),
            (Some(id), None) => Err(Error::internal(format!(
                "GL posting names tenant {id} but multitenancy is disabled"
            ))),
        }
    }

    /// Book the request idempotently. Answers the journal entry now
    /// holding the source's postings, or `None` for a zero-total request.
    async fn book(
        &self,
        db: &DatabaseConnection,
        req: &GlPostingRequested,
    ) -> Result<Option<Uuid>> {
        // Zero lines carry no information; a request that nets to nothing
        // (a zero-cost receipt) books nothing.
        let lines: Vec<_> = req
            .lines
            .iter()
            .filter(|l| !(l.debit.is_zero() && l.credit.is_zero()))
            .collect();
        if lines.is_empty() {
            return Ok(None);
        }

        let txn = db.begin().await?;

        // Serialize deliveries of the same source: the advisory lock holds
        // for the life of this transaction, so a concurrent duplicate
        // waits, then finds the entry the first delivery committed.
        txn.execute(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT pg_advisory_xact_lock(hashtextextended($1, 74))",
            [req.source.clone().into()],
        ))
        .await?;
        if let Some(existing) = entry::Entity::find()
            .filter(entry::Column::Reference.eq(&req.source))
            .one(&txn)
            .await?
        {
            return Ok(Some(existing.id));
        }

        // Roles → seeded accounts. A missing role is a hard error: the
        // request stays unbooked (and unacked) until the seed provides it.
        let mut accounts: HashMap<&str, account::Model> = HashMap::new();
        for line in &lines {
            if accounts.contains_key(line.account_role.as_str()) {
                continue;
            }
            let acc = account::Entity::find()
                .filter(account::Column::SystemKey.eq(&line.account_role))
                .one(&txn)
                .await?
                .ok_or_else(|| {
                    Error::internal(format!(
                        "no account carries the role {:?}; cannot book {}",
                        line.account_role, req.source
                    ))
                })?;
            accounts.insert(line.account_role.as_str(), acc);
        }

        // The publisher may not know the tenant's base currency — the
        // chart of accounts is the authority on it.
        let currency = match &req.currency {
            Some(c) => c.clone(),
            None => accounts
                .values()
                .next()
                .map(|a| a.currency.clone())
                .unwrap_or_default(),
        };

        let postings: Vec<PostingInput> = lines
            .iter()
            .map(|l| PostingInput {
                account_id: accounts[l.account_role.as_str()].id,
                debit: round_ledger(l.debit),
                credit: round_ledger(l.credit),
                memo: l.memo.clone(),
            })
            .collect();

        let numbering = self.numbering.get()?;
        let entry_id = Ledger::create_posted_in(
            &txn,
            NewEntry {
                entry_date: req.entry_date,
                memo: req.memo.clone(),
                reference: Some(req.source.clone()),
                currency,
                lines: postings,
                created_by: None,
            },
            super::SYSTEM_SERIES,
            &numbering,
        )
        .await?;
        txn.commit().await?;
        tracing::info!(source = %req.source, entry = %entry_id, "GL posting booked");
        Ok(Some(entry_id))
    }
}

/// Ledger money is booked to 2 decimals.
fn round_ledger(v: Decimal) -> Decimal {
    v.round_dp(2)
}
