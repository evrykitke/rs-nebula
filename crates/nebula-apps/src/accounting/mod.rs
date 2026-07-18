//! The accounting app: double-entry bookkeeping for a tenant.
//!
//! - [`account`] — the chart of accounts (financial buckets)
//! - [`journal`] — journal entries and postings, with post/reverse and
//!   the double-entry invariants
//! - [`expense`] — everyday expense recording: one call books and posts
//!   the balanced payment-voucher entry
//! - [`fiscal`] — fiscal years and monthly periods: the posting calendar
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
pub mod expense;
pub mod fiscal;
pub mod gl_port;
pub mod journal;
pub mod ledger;
pub mod reports;
pub mod seed;
pub mod tax;
pub mod widgets;

use nebula::error::Result;
use nebula::tenancy::{TenantCreated, TenantCurrencyChanged, TenantManager};
use nebula::{AdministrationModule, Module, ModuleContext, Reset, SeriesDef};
use std::sync::Arc;
use uuid::Uuid;

/// The document number series for posted journal entries.
pub(crate) const JOURNAL_SERIES: &str = "accounting.journal";

/// The document number series for expense (payment) vouchers.
pub(crate) const EXPENSE_SERIES: &str = "accounting.expense";

/// The document number series for entries booked by the system on behalf
/// of source documents in other modules (the GL posting port).
pub(crate) const SYSTEM_SERIES: &str = "accounting.system";

/// The currency a tenant with no configured default is seeded in.
const FALLBACK_CURRENCY: &str = "KES";

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
        ctx.declare_series(
            SeriesDef::new(
                EXPENSE_SERIES,
                "Expense Voucher",
                "PV-{YYYY}-{SEQ:5}",
                Reset::Yearly,
            )
            .expect("valid expense series template"),
        );
        ctx.declare_series(
            SeriesDef::new(
                SYSTEM_SERIES,
                "System Journal Entry",
                "SYS-{YYYY}-{SEQ:5}",
                Reset::Yearly,
            )
            .expect("valid system series template"),
        );

        ctx.add_api(account::api());
        ctx.add_api(journal::api());
        ctx.add_api(expense::api());
        ctx.add_api(ledger::api());
        ctx.add_api(tax::api());
        ctx.add_api(fiscal::api());
        ctx.add_routes(
            account::routes()
                .merge(journal::routes())
                .merge(expense::routes())
                .merge(ledger::routes())
                .merge(tax::routes())
                .merge(fiscal::routes()),
        );

        ctx.declare_report(Arc::new(reports::TrialBalanceReport));
        ctx.declare_report(Arc::new(reports::BalanceSheetReport));
        ctx.declare_report(Arc::new(reports::IncomeStatementReport));

        // The accounting dashboard, and the financial tiles the
        // workspace dashboard shows first.
        ctx.declare_widget(Arc::new(widgets::CashPositionWidget));
        ctx.declare_widget(Arc::new(widgets::RevenueMonthWidget));
        ctx.declare_widget(Arc::new(widgets::ExpensesMonthWidget));
        ctx.declare_widget(Arc::new(widgets::NetIncomeMonthWidget));
        ctx.declare_widget(Arc::new(widgets::RevenueVsExpensesWidget));
        ctx.declare_widget(Arc::new(widgets::ExpenseBreakdownWidget));
        ctx.declare_widget(Arc::new(widgets::RecentJournalsWidget));
        ctx.declare_widget(Arc::new(widgets::TopExpensesWidget));
        ctx.declare_widget(Arc::new(widgets::WorkspaceCashPositionWidget));
        ctx.declare_widget(Arc::new(widgets::WorkspaceRevenueMonthWidget));
        ctx.declare_widget(Arc::new(widgets::WorkspaceNetIncomeMonthWidget));

        // Other modules' financial side effects arrive over the GL port.
        gl_port::GlPort::subscribe(ctx);

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

                // Onboarding asks for the currency after the company exists,
                // by which time the chart is seeded in the fallback one.
                let on_currency = tenants.clone();
                ctx.events()
                    .subscribe::<TenantCurrencyChanged, _, _>(move |ev| {
                        let tenants = on_currency.clone();
                        async move { redenominate_tenant(&tenants, ev.tenant_id).await }
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
                        if let Err(e) = fiscal::FiscalService::new(db).ensure_current_year().await {
                            tracing::warn!(error = %e, "fiscal year seed failed");
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
    // Open the current calendar year for posting (idempotent, independent of
    // the chart seed so it also reaches already-seeded tenants).
    if fiscal::FiscalService::new(db).ensure_current_year().await? {
        tracing::info!(tenant = %tenant.name, "opened the current fiscal year");
    }
    Ok(())
}

/// Follow a company's currency into its ledger: the chart of accounts holds
/// a copy of it, and onboarding only asks for it once the company exists.
///
/// The currency is read from the tenant row rather than taken from the event,
/// so a late-delivered message can never undo a newer choice. Seeding first
/// covers the case where this arrives before the chart was ever cut.
async fn redenominate_tenant(tenants: &TenantManager, tenant_id: Uuid) -> Result<()> {
    let Some(tenant) = tenants.find_by_id(tenant_id).await? else {
        return Ok(());
    };
    let Some(currency) = tenant.default_currency.as_deref() else {
        return Ok(());
    };
    let db = tenants.connection_for(&tenant).await?;
    seed::seed_defaults(&db, currency).await?;
    if seed::redenominate(&db, currency).await? {
        tracing::info!(tenant = %tenant.name, %currency, "re-denominated the ledger");
    } else {
        // Either nothing to do, or the ledger is already in use — in which
        // case the company's currency and its accounts now disagree, and only
        // a restatement can settle that.
        tracing::warn!(tenant = %tenant.name, %currency,
            "ledger not re-denominated: it is either already in this currency \
             or has postings");
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
        pub const FISCAL_YEARS: &str = "Pages.Accounting.FiscalYears";
        pub const FISCAL_YEARS_VIEW: &str = "Pages.Accounting.FiscalYears.View";
        pub const FISCAL_YEARS_MANAGE: &str = "Pages.Accounting.FiscalYears.Manage";
        pub const JOURNAL: &str = "Pages.Accounting.Journal";
        pub const JOURNAL_VIEW: &str = "Pages.Accounting.Journal.View";
        pub const JOURNAL_CREATE: &str = "Pages.Accounting.Journal.Create";
        pub const JOURNAL_POST: &str = "Pages.Accounting.Journal.Post";
        pub const JOURNAL_REVERSE: &str = "Pages.Accounting.Journal.Reverse";
        pub const EXPENSES: &str = "Pages.Accounting.Expenses";
        pub const EXPENSES_VIEW: &str = "Pages.Accounting.Expenses.View";
        pub const EXPENSES_RECORD: &str = "Pages.Accounting.Expenses.Record";
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
                PermissionDef::new(FISCAL_YEARS, "Fiscal years")
                    .child(PermissionDef::new(FISCAL_YEARS_VIEW, "View fiscal years"))
                    .child(PermissionDef::new(
                        FISCAL_YEARS_MANAGE,
                        "Manage fiscal years & periods",
                    )),
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
                PermissionDef::new(EXPENSES, "Expenses")
                    .child(PermissionDef::new(EXPENSES_VIEW, "View expenses"))
                    .child(PermissionDef::new(EXPENSES_RECORD, "Record expenses")),
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
