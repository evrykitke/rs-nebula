//! The accounting app: double-entry bookkeeping for a tenant.
//!
//! - [`account`] — the chart of accounts (financial buckets)
//! - [`journal`] — journal entries and postings, with post/reverse and
//!   the double-entry invariants
//! - [`ledger`] — the trial balance and per-account ledger reads
//! - [`tax`] — tax codes (rates + the accounts tax books to)
//! - [`reports`] — the Trial Balance report on the framework engine
//! - [`seed`] — the default chart of accounts and tax codes seeded per
//!   tenant so a business transacts with zero configuration
//!
//! Every tenant is seeded with a standard chart of accounts and tax codes
//! (on registration, and rolled out to existing tenants at boot) so a POS
//! or a small trader can sell and read reports immediately, while a
//! business that cares keeps full control of its setup. All mutations are
//! written to the audit trail.
//!
//! Depends on [`AdministrationModule`]: accounting is done by signed-in
//! people of a tenant, which pulls in authentication and the currency
//! table.

pub mod account;
pub mod journal;
pub mod ledger;
pub mod reports;
pub mod seed;
pub mod tax;

use nebula::error::Result;
use nebula::tenancy::{TenantCreated, TenantManager};
use nebula::{AdministrationModule, Module, ModuleContext, Reset, SeriesDef};
use std::sync::Arc;
use uuid::Uuid;

/// The document number series for posted journal entries.
pub(crate) const JOURNAL_SERIES: &str = "accounting.journal";

/// The currency a tenant with no configured default is seeded in.
const FALLBACK_CURRENCY: &str = "USD";

pub struct AccountingApp;

impl Module for AccountingApp {
    fn name(&self) -> &'static str {
        "accounting"
    }

    fn depends_on(&self) -> Vec<Box<dyn Module>> {
        vec![Box::new(AdministrationModule)]
    }

    fn configure(&self, ctx: &mut ModuleContext) {
        ctx.add_permissions(permissions::tree());
        ctx.declare_series(
            SeriesDef::new(
                JOURNAL_SERIES,
                "Journal Entry",
                "JV-{YYYY}-{SEQ:5}",
                Reset::Yearly,
            )
            .expect("valid journal series template"),
        );

        ctx.add_api(account::api());
        ctx.add_api(journal::api());
        ctx.add_api(ledger::api());
        ctx.add_api(tax::api());
        ctx.add_routes(
            account::routes()
                .merge(journal::routes())
                .merge(ledger::routes())
                .merge(tax::routes()),
        );

        ctx.declare_report(Arc::new(reports::TrialBalanceReport));

        self.seed_tenants(ctx);
    }
}

impl AccountingApp {
    /// Seed every tenant with the default chart of accounts and tax codes:
    /// new tenants react to [`TenantCreated`], existing tenants are rolled
    /// out once at boot (both idempotent). Migrations have already run for
    /// every database by the time `configure` is called, so the schema is
    /// ready.
    fn seed_tenants(&self, ctx: &mut ModuleContext) {
        match ctx.tenants() {
            Some(tenants) => {
                let on_create = tenants.clone();
                ctx.events().subscribe::<TenantCreated, _, _>(move |ev| {
                    let tenants = on_create.clone();
                    async move { seed_for_tenant(&tenants, ev.tenant_id).await }
                });

                let rollout = tenants.clone();
                tokio::spawn(async move {
                    match rollout.find_all().await {
                        Ok(list) => {
                            for tenant in list.into_iter().filter(|t| t.is_active) {
                                if let Err(e) = seed_for_tenant(&rollout, tenant.id).await {
                                    tracing::warn!(tenant = %tenant.name, error = %e,
                                        "accounting seed rollout failed");
                                }
                            }
                        }
                        Err(e) => tracing::warn!(error = %e,
                            "could not list tenants for the accounting seed rollout"),
                    }
                });
            }
            // Single-tenant: the app runs against the main database.
            None => {
                if let Some(db) = ctx.db() {
                    let db = db.clone();
                    tokio::spawn(async move {
                        if let Err(e) = seed::seed_defaults(&db, FALLBACK_CURRENCY).await {
                            tracing::warn!(error = %e, "accounting seed failed");
                        }
                    });
                }
            }
        }
    }
}

/// Seed one tenant's ledger in its own currency (idempotent).
async fn seed_for_tenant(tenants: &TenantManager, tenant_id: Uuid) -> Result<()> {
    let Some(tenant) = tenants.find_by_id(tenant_id).await? else {
        return Ok(());
    };
    let db = tenants.connection_for(&tenant).await?;
    let currency = tenant
        .default_currency
        .as_deref()
        .unwrap_or(FALLBACK_CURRENCY);
    if seed::seed_defaults(&db, currency).await? {
        tracing::info!(tenant = %tenant.name, %currency,
            "seeded default chart of accounts and tax codes");
    }
    Ok(())
}

/// The accounting permission tree and the flat names its endpoints check.
pub mod permissions {
    use nebula::auth::PermissionDef;

    pub mod names {
        pub const ACCOUNTING: &str = "Pages.Accounting";
        pub const ACCOUNTS: &str = "Pages.Accounting.Accounts";
        pub const ACCOUNTS_VIEW: &str = "Pages.Accounting.Accounts.View";
        pub const ACCOUNTS_CREATE: &str = "Pages.Accounting.Accounts.Create";
        pub const ACCOUNTS_EDIT: &str = "Pages.Accounting.Accounts.Edit";
        pub const ACCOUNTS_DELETE: &str = "Pages.Accounting.Accounts.Delete";
        pub const JOURNAL: &str = "Pages.Accounting.Journal";
        pub const JOURNAL_VIEW: &str = "Pages.Accounting.Journal.View";
        pub const JOURNAL_CREATE: &str = "Pages.Accounting.Journal.Create";
        pub const JOURNAL_POST: &str = "Pages.Accounting.Journal.Post";
        pub const JOURNAL_REVERSE: &str = "Pages.Accounting.Journal.Reverse";
        pub const TAX: &str = "Pages.Accounting.Tax";
        pub const TAX_VIEW: &str = "Pages.Accounting.Tax.View";
        pub const TAX_CREATE: &str = "Pages.Accounting.Tax.Create";
        pub const TAX_EDIT: &str = "Pages.Accounting.Tax.Edit";
        pub const TAX_DELETE: &str = "Pages.Accounting.Tax.Delete";
        pub const REPORTS: &str = "Pages.Accounting.Reports";
        pub const REPORTS_VIEW: &str = "Pages.Accounting.Reports.View";
    }

    pub fn tree() -> PermissionDef {
        use names::*;
        PermissionDef::new(ACCOUNTING, "Accounting")
            .child(
                PermissionDef::new(ACCOUNTS, "Chart of accounts")
                    .child(PermissionDef::new(ACCOUNTS_VIEW, "View accounts"))
                    .child(PermissionDef::new(ACCOUNTS_CREATE, "Create accounts"))
                    .child(PermissionDef::new(ACCOUNTS_EDIT, "Edit accounts"))
                    .child(PermissionDef::new(ACCOUNTS_DELETE, "Delete accounts")),
            )
            .child(
                PermissionDef::new(JOURNAL, "Journal")
                    .child(PermissionDef::new(JOURNAL_VIEW, "View journal entries"))
                    .child(PermissionDef::new(JOURNAL_CREATE, "Create journal entries"))
                    .child(PermissionDef::new(JOURNAL_POST, "Post journal entries"))
                    .child(PermissionDef::new(
                        JOURNAL_REVERSE,
                        "Reverse journal entries",
                    )),
            )
            .child(
                PermissionDef::new(TAX, "Tax")
                    .child(PermissionDef::new(TAX_VIEW, "View tax codes"))
                    .child(PermissionDef::new(TAX_CREATE, "Create tax codes"))
                    .child(PermissionDef::new(TAX_EDIT, "Edit tax codes"))
                    .child(PermissionDef::new(TAX_DELETE, "Delete tax codes")),
            )
            .child(
                PermissionDef::new(REPORTS, "Accounting reports")
                    .child(PermissionDef::new(REPORTS_VIEW, "View accounting reports")),
            )
    }
}
