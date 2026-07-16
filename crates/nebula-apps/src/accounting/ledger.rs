//! Ledger reads: the trial balance and a per-account ledger.
//!
//! Both aggregate entries that have hit the ledger — `posted` and
//! `reversed`. A reversed entry's postings are a historical fact that its
//! reversal offsets, so **both** stay in the ledger and net to zero;
//! dropping the original would leave the canceling reversal double-counted.
//! Drafts are never part of the ledger. Amounts are exact decimals summed
//! in the database. The trial balance presents each account's ending
//! balance in its natural debit or credit column, so the two columns
//! always foot to the same total.

use crate::accounting::account::{self, AccountType, NormalBalance};
use crate::accounting::permissions::names;
use axum::extract::{Path, Query};
use axum::routing::get;
use axum::{Json, Router};
use nebula::auth::Authz;
use nebula::error::Result;
use nebula::{TenantDb, sea_orm};
use rust_decimal::Decimal;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter, Statement,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Ledger read queries over one (tenant) connection.
pub struct LedgerQueries {
    db: DatabaseConnection,
}

/// Debit/credit totals for one account, keyed by account id.
struct Totals {
    debit: Decimal,
    credit: Decimal,
}

impl LedgerQueries {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Posted debit/credit totals per account, optionally up to `as_of`.
    async fn totals(
        &self,
        as_of: Option<chrono::NaiveDate>,
    ) -> Result<std::collections::HashMap<Uuid, Totals>> {
        let backend = self.db.get_database_backend();
        let stmt = match as_of {
            Some(date) => Statement::from_sql_and_values(
                backend,
                "SELECT p.account_id, \
                    COALESCE(SUM(p.debit), 0) AS debit, \
                    COALESCE(SUM(p.credit), 0) AS credit \
                 FROM accounting_postings p \
                 JOIN accounting_journal_entries e ON e.id = p.entry_id \
                 WHERE e.status IN ('posted', 'reversed') AND e.entry_date <= $1 \
                 GROUP BY p.account_id",
                [date.into()],
            ),
            None => Statement::from_string(
                backend,
                "SELECT p.account_id, \
                    COALESCE(SUM(p.debit), 0) AS debit, \
                    COALESCE(SUM(p.credit), 0) AS credit \
                 FROM accounting_postings p \
                 JOIN accounting_journal_entries e ON e.id = p.entry_id \
                 WHERE e.status IN ('posted', 'reversed') \
                 GROUP BY p.account_id"
                    .to_string(),
            ),
        };
        let rows = self.db.query_all(stmt).await?;
        let mut out = std::collections::HashMap::with_capacity(rows.len());
        for row in rows {
            let account_id = row.try_get::<Uuid>("", "account_id")?;
            let debit = row.try_get::<Decimal>("", "debit")?;
            let credit = row.try_get::<Decimal>("", "credit")?;
            out.insert(account_id, Totals { debit, credit });
        }
        Ok(out)
    }

    /// Posted debit/credit totals per account over a date range (inclusive),
    /// either bound optional — used for period statements like the income
    /// statement, where only activity within the period counts.
    async fn totals_range(
        &self,
        from: Option<chrono::NaiveDate>,
        to: Option<chrono::NaiveDate>,
    ) -> Result<std::collections::HashMap<Uuid, Totals>> {
        let backend = self.db.get_database_backend();
        let stmt = Statement::from_sql_and_values(
            backend,
            "SELECT p.account_id, \
                COALESCE(SUM(p.debit), 0) AS debit, \
                COALESCE(SUM(p.credit), 0) AS credit \
             FROM accounting_postings p \
             JOIN accounting_journal_entries e ON e.id = p.entry_id \
             WHERE e.status IN ('posted', 'reversed') \
               AND ($1::date IS NULL OR e.entry_date >= $1) \
               AND ($2::date IS NULL OR e.entry_date <= $2) \
             GROUP BY p.account_id",
            [from.into(), to.into()],
        );
        let rows = self.db.query_all(stmt).await?;
        let mut out = std::collections::HashMap::with_capacity(rows.len());
        for row in rows {
            let account_id = row.try_get::<Uuid>("", "account_id")?;
            let debit = row.try_get::<Decimal>("", "debit")?;
            let credit = row.try_get::<Decimal>("", "credit")?;
            out.insert(account_id, Totals { debit, credit });
        }
        Ok(out)
    }

    /// The trial balance: one row per account with activity (or that is
    /// active), each in its natural debit/credit column.
    pub async fn trial_balance(&self, as_of: Option<chrono::NaiveDate>) -> Result<TrialBalance> {
        let totals = self.totals(as_of).await?;
        let accounts = account::Store::new(self.db.clone()).find_all().await?;

        let mut rows = Vec::new();
        let mut total_debit = Decimal::ZERO;
        let mut total_credit = Decimal::ZERO;
        for acc in accounts {
            let t = totals.get(&acc.id);
            let has_activity = t.is_some();
            if !has_activity && !acc.is_active {
                continue;
            }
            let debit_sum = t.map(|t| t.debit).unwrap_or(Decimal::ZERO);
            let credit_sum = t.map(|t| t.credit).unwrap_or(Decimal::ZERO);
            let net = debit_sum - credit_sum; // > 0 means a net debit
            let (debit, credit) = if net >= Decimal::ZERO {
                (net, Decimal::ZERO)
            } else {
                (Decimal::ZERO, -net)
            };
            if !has_activity && debit == Decimal::ZERO && credit == Decimal::ZERO {
                // An active but never-used account: skip zero rows.
                continue;
            }
            total_debit += debit;
            total_credit += credit;
            rows.push(TrialBalanceRow {
                account_id: acc.id,
                code: acc.code,
                name: acc.name,
                account_type: AccountType::parse(&acc.account_type)?,
                currency: acc.currency,
                debit,
                credit,
            });
        }
        Ok(TrialBalance {
            as_of,
            rows,
            total_debit,
            total_credit,
        })
    }

    /// A single account's ledger: its posted postings in date order with a
    /// running balance on the account's normal side.
    pub async fn account_ledger(
        &self,
        account_id: Uuid,
        from: Option<chrono::NaiveDate>,
        to: Option<chrono::NaiveDate>,
    ) -> Result<AccountLedger> {
        let account = account::Store::new(self.db.clone())
            .find_by_id(account_id)
            .await?;
        let account_type = AccountType::parse(&account.account_type)?;
        let normal = account_type.normal_balance();
        let sign = |debit: Decimal, credit: Decimal| match normal {
            NormalBalance::Debit => debit - credit,
            NormalBalance::Credit => credit - debit,
        };

        // Opening balance: everything strictly before `from`.
        let opening = match from {
            Some(from) => {
                let day_before = from.pred_opt().unwrap_or(from);
                let totals = self.totals(Some(day_before)).await?;
                totals
                    .get(&account_id)
                    .map(|t| sign(t.debit, t.credit))
                    .unwrap_or(Decimal::ZERO)
            }
            None => Decimal::ZERO,
        };

        let backend = self.db.get_database_backend();
        let stmt = Statement::from_sql_and_values(
            backend,
            "SELECT e.id AS entry_id, e.number, e.entry_date, e.memo, e.reference, \
                    p.debit, p.credit \
             FROM accounting_postings p \
             JOIN accounting_journal_entries e ON e.id = p.entry_id \
             WHERE p.account_id = $1 AND e.status IN ('posted', 'reversed') \
               AND ($2::date IS NULL OR e.entry_date >= $2) \
               AND ($3::date IS NULL OR e.entry_date <= $3) \
             ORDER BY e.entry_date ASC, e.posted_at ASC, p.line_no ASC",
            [account_id.into(), from.into(), to.into()],
        );
        let db_rows = self.db.query_all(stmt).await?;

        let mut running = opening;
        let mut lines = Vec::with_capacity(db_rows.len());
        for row in db_rows {
            let debit = row.try_get::<Decimal>("", "debit")?;
            let credit = row.try_get::<Decimal>("", "credit")?;
            running += sign(debit, credit);
            lines.push(AccountLedgerLine {
                entry_id: row.try_get::<Uuid>("", "entry_id")?,
                number: row.try_get::<Option<String>>("", "number")?,
                entry_date: row.try_get::<chrono::NaiveDate>("", "entry_date")?,
                memo: row.try_get::<String>("", "memo")?,
                reference: row.try_get::<Option<String>>("", "reference")?,
                debit,
                credit,
                balance: running,
            });
        }

        Ok(AccountLedger {
            account_id: account.id,
            code: account.code,
            name: account.name,
            account_type,
            currency: account.currency,
            opening_balance: opening,
            closing_balance: running,
            lines,
        })
    }

    /// The balance sheet as of a date: assets, liabilities and equity, each
    /// account shown at its ending balance on its natural side. The nominal
    /// (revenue/expense) accounts are never closed in real time, so their
    /// lifetime net income is computed and folded into equity — split into
    /// `prior_earnings` (fiscal years before the one covering the reference
    /// date, presented as retained earnings) and `current_earnings` (the
    /// current fiscal year) so the statement balances
    /// (assets == liabilities + equity + prior + current earnings).
    pub async fn balance_sheet(&self, as_of: Option<chrono::NaiveDate>) -> Result<BalanceSheet> {
        let totals = self.totals(as_of).await?;
        let accounts = account::Store::new(self.db.clone()).find_all().await?;

        let mut assets = StatementSection::new("Assets");
        let mut liabilities = StatementSection::new("Liabilities");
        let mut equity = StatementSection::new("Equity");

        for acc in &accounts {
            let ty = AccountType::parse(&acc.account_type)?;
            let t = totals.get(&acc.id);
            let debit = t.map(|t| t.debit).unwrap_or(Decimal::ZERO);
            let credit = t.map(|t| t.credit).unwrap_or(Decimal::ZERO);
            let amount = match ty.normal_balance() {
                NormalBalance::Debit => debit - credit,
                NormalBalance::Credit => credit - debit,
            };
            match ty {
                AccountType::Asset => assets.push(acc, amount),
                AccountType::Liability => liabilities.push(acc, amount),
                AccountType::Equity => equity.push(acc, amount),
                // Nominal accounts are handled below as earnings.
                AccountType::Revenue | AccountType::Expense => {}
            }
        }

        let net_income = net_income_of(&accounts, &totals)?;
        let prior_earnings = self.prior_earnings(&accounts, as_of).await?;
        let current_earnings = net_income - prior_earnings;

        let total_assets = assets.total;
        let total_liabilities_and_equity = liabilities.total + equity.total + net_income;
        Ok(BalanceSheet {
            as_of,
            assets,
            liabilities,
            equity,
            prior_earnings,
            current_earnings,
            total_assets,
            total_liabilities_and_equity,
            balanced: total_assets == total_liabilities_and_equity,
        })
    }

    /// Net income accumulated before the fiscal year covering the reference
    /// date (`as_of`, or today) — presented on the balance sheet as retained
    /// earnings. Zero when no fiscal year covers the date: with no calendar
    /// there is no year boundary to split on.
    async fn prior_earnings(
        &self,
        accounts: &[account::Model],
        as_of: Option<chrono::NaiveDate>,
    ) -> Result<Decimal> {
        use crate::accounting::fiscal::year;
        let ref_date = as_of.unwrap_or_else(|| chrono::Utc::now().date_naive());
        let fy = year::Entity::find()
            .filter(year::Column::StartDate.lte(ref_date))
            .filter(year::Column::EndDate.gte(ref_date))
            .one(&self.db)
            .await?;
        let Some(fy) = fy else {
            return Ok(Decimal::ZERO);
        };
        let Some(day_before) = fy.start_date.pred_opt() else {
            return Ok(Decimal::ZERO);
        };
        let prior_totals = self.totals(Some(day_before)).await?;
        net_income_of(accounts, &prior_totals)
    }

    /// The income statement over a period: revenue and expenses shown at their
    /// activity within `[from, to]`, and the resulting net income.
    pub async fn income_statement(
        &self,
        from: Option<chrono::NaiveDate>,
        to: Option<chrono::NaiveDate>,
    ) -> Result<IncomeStatement> {
        let totals = self.totals_range(from, to).await?;
        let accounts = account::Store::new(self.db.clone()).find_all().await?;

        let mut revenue = StatementSection::new("Revenue");
        let mut expenses = StatementSection::new("Expenses");

        for acc in accounts {
            let ty = AccountType::parse(&acc.account_type)?;
            if ty != AccountType::Revenue && ty != AccountType::Expense {
                continue;
            }
            let t = totals.get(&acc.id);
            let debit = t.map(|t| t.debit).unwrap_or(Decimal::ZERO);
            let credit = t.map(|t| t.credit).unwrap_or(Decimal::ZERO);
            let amount = match ty.normal_balance() {
                NormalBalance::Debit => debit - credit,
                NormalBalance::Credit => credit - debit,
            };
            match ty {
                AccountType::Revenue => revenue.push(&acc, amount),
                AccountType::Expense => expenses.push(&acc, amount),
                _ => {}
            }
        }

        let net_income = revenue.total - expenses.total;
        Ok(IncomeStatement {
            from,
            to,
            revenue,
            expenses,
            net_income,
        })
    }
}

/// The net income (revenue minus expenses) implied by a totals map.
fn net_income_of(
    accounts: &[account::Model],
    totals: &std::collections::HashMap<Uuid, Totals>,
) -> Result<Decimal> {
    let mut net = Decimal::ZERO;
    for acc in accounts {
        let Some(t) = totals.get(&acc.id) else {
            continue;
        };
        match AccountType::parse(&acc.account_type)? {
            AccountType::Revenue => net += t.credit - t.debit,
            AccountType::Expense => net -= t.debit - t.credit,
            _ => {}
        }
    }
    Ok(net)
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, utoipa::ToSchema)]
pub struct TrialBalanceRow {
    pub account_id: Uuid,
    pub code: String,
    pub name: String,
    pub account_type: AccountType,
    pub currency: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub debit: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub credit: Decimal,
}

#[derive(Serialize, Deserialize, utoipa::ToSchema)]
pub struct TrialBalance {
    #[schema(value_type = Option<String>, format = Date)]
    pub as_of: Option<chrono::NaiveDate>,
    pub rows: Vec<TrialBalanceRow>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total_debit: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total_credit: Decimal,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct AccountLedgerLine {
    pub entry_id: Uuid,
    pub number: Option<String>,
    #[schema(value_type = String, format = Date)]
    pub entry_date: chrono::NaiveDate,
    pub memo: String,
    pub reference: Option<String>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub debit: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub credit: Decimal,
    /// Running balance on the account's normal side.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub balance: Decimal,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct AccountLedger {
    pub account_id: Uuid,
    pub code: String,
    pub name: String,
    pub account_type: AccountType,
    pub currency: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub opening_balance: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub closing_balance: Decimal,
    pub lines: Vec<AccountLedgerLine>,
}

/// One account's contribution to a financial statement, at its natural-side
/// balance.
#[derive(Serialize, Deserialize, utoipa::ToSchema)]
pub struct StatementLine {
    pub account_id: Uuid,
    pub code: String,
    pub name: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub amount: Decimal,
}

/// A titled group of statement lines with its subtotal (e.g. "Assets").
#[derive(Serialize, Deserialize, utoipa::ToSchema)]
pub struct StatementSection {
    pub title: String,
    pub lines: Vec<StatementLine>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total: Decimal,
}

impl StatementSection {
    fn new(title: &str) -> Self {
        Self {
            title: title.to_string(),
            lines: Vec::new(),
            total: Decimal::ZERO,
        }
    }

    /// Add an account's balance to the section, skipping zero balances so the
    /// statement stays scannable. The total always includes it.
    fn push(&mut self, account: &account::Model, amount: Decimal) {
        self.total += amount;
        if amount.is_zero() {
            return;
        }
        self.lines.push(StatementLine {
            account_id: account.id,
            code: account.code.clone(),
            name: account.name.clone(),
            amount,
        });
    }
}

/// The balance sheet as of a date. The still-open nominal accounts' net
/// income is folded into equity so the sheet balances: `prior_earnings` is
/// income from fiscal years before the one covering the reference date
/// (retained earnings), `current_earnings` the current year's.
#[derive(Serialize, Deserialize, utoipa::ToSchema)]
pub struct BalanceSheet {
    #[schema(value_type = Option<String>, format = Date)]
    pub as_of: Option<chrono::NaiveDate>,
    pub assets: StatementSection,
    pub liabilities: StatementSection,
    pub equity: StatementSection,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub prior_earnings: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub current_earnings: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total_assets: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total_liabilities_and_equity: Decimal,
    pub balanced: bool,
}

/// The income statement over a period.
#[derive(Serialize, Deserialize, utoipa::ToSchema)]
pub struct IncomeStatement {
    #[schema(value_type = Option<String>, format = Date)]
    pub from: Option<chrono::NaiveDate>,
    #[schema(value_type = Option<String>, format = Date)]
    pub to: Option<chrono::NaiveDate>,
    pub revenue: StatementSection,
    pub expenses: StatementSection,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub net_income: Decimal,
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

pub(super) fn routes() -> Router {
    Router::new()
        .route("/accounting/trial-balance", get(trial_balance))
        .route("/accounting/balance-sheet", get(balance_sheet))
        .route("/accounting/income-statement", get(income_statement))
        .route("/accounting/accounts/{id}/ledger", get(account_ledger))
}

pub(super) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(trial_balance, balance_sheet, income_statement, account_ledger))]
struct ApiDoc;

#[derive(Deserialize, utoipa::ToSchema)]
pub struct TrialBalanceQuery {
    /// Balances as of this date (inclusive); omit for all time.
    pub as_of: Option<chrono::NaiveDate>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct LedgerQuery {
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

#[utoipa::path(get, path = "/accounting/trial-balance", tag = "accounting",
    params(("as_of" = Option<String>, Query, description = "Balances as of this date")),
    responses((status = 200, body = TrialBalance)))]
async fn trial_balance(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<TrialBalanceQuery>,
) -> Result<Json<TrialBalance>> {
    authz.require(names::REPORTS_VIEW).await?;
    LedgerQueries::new(db)
        .trial_balance(q.as_of)
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/accounting/balance-sheet", tag = "accounting",
    params(("as_of" = Option<String>, Query, description = "Balances as of this date")),
    responses((status = 200, body = BalanceSheet)))]
async fn balance_sheet(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<TrialBalanceQuery>,
) -> Result<Json<BalanceSheet>> {
    authz.require(names::REPORTS_VIEW).await?;
    LedgerQueries::new(db)
        .balance_sheet(q.as_of)
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/accounting/income-statement", tag = "accounting",
    params(
        ("from" = Option<String>, Query, description = "From date (inclusive)"),
        ("to" = Option<String>, Query, description = "To date (inclusive)"),
    ),
    responses((status = 200, body = IncomeStatement)))]
async fn income_statement(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<LedgerQuery>,
) -> Result<Json<IncomeStatement>> {
    authz.require(names::REPORTS_VIEW).await?;
    LedgerQueries::new(db)
        .income_statement(q.from, q.to)
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/accounting/accounts/{id}/ledger", tag = "accounting",
    params(
        ("id" = Uuid, Path, description = "Account id"),
        ("from" = Option<String>, Query, description = "From date (inclusive)"),
        ("to" = Option<String>, Query, description = "To date (inclusive)"),
    ),
    responses((status = 200, body = AccountLedger)))]
async fn account_ledger(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Query(q): Query<LedgerQuery>,
) -> Result<Json<AccountLedger>> {
    authz.require(names::REPORTS_VIEW).await?;
    LedgerQueries::new(db)
        .account_ledger(id, q.from, q.to)
        .await
        .map(Json)
}
