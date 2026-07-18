//! The accounting dashboard's widgets, plus the financial tiles the
//! workspace dashboard borrows (cash, revenue, net income — the numbers
//! an owner checks before opening any app).
//!
//! Everything reads what the ledger already holds: statements through
//! [`LedgerQueries`], balances straight off the postings. All widgets
//! require the accounting reports permission — a dashboard tile is a
//! report at a glance.

use super::ledger::LedgerQueries;
use super::permissions::names;
use crate::widgets::{chart_value, delta_vs, last_months, money, month_start, previous_month};
use nebula::{
    ChartData, ChartType, Error, ListData, ListItemData, Result, StatData, TableColumnData,
    TableData, TrendDirection, WidgetCx, WidgetData, WidgetDefinition, WidgetKind, sea_orm,
};
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, DbBackend, QueryOrder, QuerySelect, Statement};

/// Debit-minus-credit balance across the accounts holding the given
/// roles. The role list is compile-time constant, so it is inlined into
/// the SQL rather than bound.
async fn role_balance(db: &DatabaseConnection, roles: &[&str]) -> Result<Decimal> {
    let list = roles
        .iter()
        .map(|r| format!("'{r}'"))
        .collect::<Vec<_>>()
        .join(", ");

    let row = db
        .query_one(Statement::from_string(
            DbBackend::Postgres,
            format!(
                "SELECT COALESCE(SUM(p.debit - p.credit), 0)::numeric AS v
                 FROM accounting_postings p
                 JOIN accounting_journal_entries e ON e.id = p.entry_id
                 JOIN accounting_accounts a ON a.id = p.account_id
                 WHERE e.status IN ('posted', 'reversed') AND a.system_key IN ({list})"
            ),
        ))
        .await?;
    Ok(row
        .map(|r| r.try_get::<Decimal>("", "v").unwrap_or(Decimal::ZERO))
        .unwrap_or(Decimal::ZERO))
}

/// Revenue and expense activity per calendar month since `from`, keyed
/// `YYYY-MM` — one grouped query instead of a statement per month.
async fn monthly_pnl(
    db: &DatabaseConnection,
    from: chrono::NaiveDate,
) -> Result<Vec<(String, Decimal, Decimal)>> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT to_char(date_trunc('month', e.entry_date::timestamp), 'YYYY-MM') AS month,
                    COALESCE(SUM(CASE WHEN a.account_type = 'revenue' THEN p.credit - p.debit END), 0)::numeric AS revenue,
                    COALESCE(SUM(CASE WHEN a.account_type = 'expense' THEN p.debit - p.credit END), 0)::numeric AS expense
             FROM accounting_postings p
             JOIN accounting_journal_entries e ON e.id = p.entry_id
             JOIN accounting_accounts a ON a.id = p.account_id
             WHERE e.status IN ('posted', 'reversed')
               AND a.account_type IN ('revenue', 'expense')
               AND e.entry_date >= $1
             GROUP BY 1 ORDER BY 1",
            [from.into()],
        ))
        .await?;
    rows.into_iter()
        .map(|r| {
            Ok((
                r.try_get::<String>("", "month")?,
                r.try_get::<Decimal>("", "revenue").unwrap_or(Decimal::ZERO),
                r.try_get::<Decimal>("", "expense").unwrap_or(Decimal::ZERO),
            ))
        })
        .collect::<std::result::Result<_, sea_orm::DbErr>>()
        .map_err(Error::from)
}

async fn cash_position(cx: &WidgetCx<'_>) -> Result<WidgetData> {
    let db = cx.require_db()?;
    let balance = role_balance(db, &[super::account::keys::CASH, super::account::keys::BANK]).await?;
    Ok(WidgetData::stat(StatData {
        value: money(balance),
        caption: Some("Cash and bank, per the books".into()),
        delta: None,
        trend: None,
    }))
}

/// This month's revenue (or expenses) with the previous month alongside.
async fn month_section(cx: &WidgetCx<'_>, expenses: bool) -> Result<WidgetData> {
    let db = cx.require_db()?;
    let today = chrono::Utc::now().date_naive();
    let queries = LedgerQueries::new(db.clone());
    let this = queries
        .income_statement(Some(month_start(today)), Some(today))
        .await?;
    let (prev_from, prev_to) = previous_month(today);
    let prev = queries
        .income_statement(Some(prev_from), Some(prev_to))
        .await?;
    let (current, previous) = if expenses {
        (this.expenses.total, prev.expenses.total)
    } else {
        (this.revenue.total, prev.revenue.total)
    };
    let (delta, trend) = delta_vs(current, previous, "last month");
    Ok(WidgetData::stat(StatData {
        value: money(current),
        caption: Some(today.format("Month to date, %B").to_string()),
        delta,
        trend,
    }))
}

pub struct CashPositionWidget;

#[async_trait::async_trait]
impl WidgetDefinition for CashPositionWidget {
    fn name(&self) -> &'static str {
        "accounting-cash-position"
    }
    fn dashboard(&self) -> &'static str {
        "accounting"
    }
    fn title(&self) -> &'static str {
        "Cash position"
    }
    fn description(&self) -> &'static str {
        "The combined balance of the cash and bank accounts."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::Stat
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn default_position(&self) -> Option<u8> {
        Some(1)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        cash_position(cx).await
    }
}

pub struct RevenueMonthWidget;

#[async_trait::async_trait]
impl WidgetDefinition for RevenueMonthWidget {
    fn name(&self) -> &'static str {
        "accounting-revenue-month"
    }
    fn dashboard(&self) -> &'static str {
        "accounting"
    }
    fn title(&self) -> &'static str {
        "Revenue this month"
    }
    fn description(&self) -> &'static str {
        "Booked revenue for the current month, against last month."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::Stat
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn default_position(&self) -> Option<u8> {
        Some(2)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        month_section(cx, false).await
    }
}

pub struct ExpensesMonthWidget;

#[async_trait::async_trait]
impl WidgetDefinition for ExpensesMonthWidget {
    fn name(&self) -> &'static str {
        "accounting-expenses-month"
    }
    fn dashboard(&self) -> &'static str {
        "accounting"
    }
    fn title(&self) -> &'static str {
        "Expenses this month"
    }
    fn description(&self) -> &'static str {
        "Booked expenses for the current month, against last month."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::Stat
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn default_position(&self) -> Option<u8> {
        Some(3)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        month_section(cx, true).await
    }
}

pub struct NetIncomeMonthWidget;

#[async_trait::async_trait]
impl WidgetDefinition for NetIncomeMonthWidget {
    fn name(&self) -> &'static str {
        "accounting-net-income-month"
    }
    fn dashboard(&self) -> &'static str {
        "accounting"
    }
    fn title(&self) -> &'static str {
        "Net income this month"
    }
    fn description(&self) -> &'static str {
        "Revenue minus expenses for the current month."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::Stat
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn default_position(&self) -> Option<u8> {
        Some(4)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        net_income_month(cx).await
    }
}

async fn net_income_month(cx: &WidgetCx<'_>) -> Result<WidgetData> {
    let db = cx.require_db()?;
    let today = chrono::Utc::now().date_naive();
    let is = LedgerQueries::new(db.clone())
        .income_statement(Some(month_start(today)), Some(today))
        .await?;
    let trend = if is.net_income.is_zero() {
        TrendDirection::Flat
    } else if is.net_income.is_sign_positive() {
        TrendDirection::Up
    } else {
        TrendDirection::Down
    };
    Ok(WidgetData::stat(StatData {
        value: money(is.net_income),
        caption: Some(today.format("Month to date, %B").to_string()),
        delta: None,
        trend: Some(trend),
    }))
}

pub struct RevenueVsExpensesWidget;

#[async_trait::async_trait]
impl WidgetDefinition for RevenueVsExpensesWidget {
    fn name(&self) -> &'static str {
        "accounting-revenue-vs-expenses"
    }
    fn dashboard(&self) -> &'static str {
        "accounting"
    }
    fn title(&self) -> &'static str {
        "Revenue vs expenses"
    }
    fn description(&self) -> &'static str {
        "Monthly booked revenue against expenses, last six months."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::Chart
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn default_position(&self) -> Option<u8> {
        Some(5)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        let db = cx.require_db()?;
        let today = chrono::Utc::now().date_naive();
        let months = last_months(today, 6);
        let by_month: std::collections::HashMap<String, (Decimal, Decimal)> =
            monthly_pnl(db, months[0].0)
                .await?
                .into_iter()
                .map(|(m, r, e)| (m, (r, e)))
                .collect();
        let mut labels = Vec::with_capacity(months.len());
        let (mut revenue, mut expenses) = (Vec::new(), Vec::new());
        for (_, key, label) in &months {
            let (r, e) = by_month.get(key).copied().unwrap_or_default();
            labels.push(label.clone());
            revenue.push(chart_value(r));
            expenses.push(chart_value(e));
        }
        Ok(WidgetData::chart(ChartData {
            chart: ChartType::Bar,
            labels,
            series: vec![
                nebula::SeriesData { name: "Revenue".into(), values: revenue },
                nebula::SeriesData { name: "Expenses".into(), values: expenses },
            ],
            unit: None,
        }))
    }
}

pub struct ExpenseBreakdownWidget;

#[async_trait::async_trait]
impl WidgetDefinition for ExpenseBreakdownWidget {
    fn name(&self) -> &'static str {
        "accounting-expense-breakdown"
    }
    fn dashboard(&self) -> &'static str {
        "accounting"
    }
    fn title(&self) -> &'static str {
        "Expense breakdown"
    }
    fn description(&self) -> &'static str {
        "This month's expenses by account, largest first."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::Chart
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn default_position(&self) -> Option<u8> {
        Some(6)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        let db = cx.require_db()?;
        let today = chrono::Utc::now().date_naive();
        let is = LedgerQueries::new(db.clone())
            .income_statement(Some(month_start(today)), Some(today))
            .await?;
        let mut lines = is.expenses.lines;
        lines.sort_by(|a, b| b.amount.cmp(&a.amount));
        let mut labels = Vec::new();
        let mut values = Vec::new();
        let mut other = Decimal::ZERO;
        for (i, line) in lines.iter().enumerate() {
            if i < 6 {
                labels.push(line.name.clone());
                values.push(chart_value(line.amount));
            } else {
                other += line.amount;
            }
        }
        if !other.is_zero() {
            labels.push("Other".into());
            values.push(chart_value(other));
        }
        Ok(WidgetData::chart(ChartData {
            chart: ChartType::Donut,
            labels,
            series: vec![nebula::SeriesData { name: "Expenses".into(), values }],
            unit: None,
        }))
    }
}

pub struct RecentJournalsWidget;

#[async_trait::async_trait]
impl WidgetDefinition for RecentJournalsWidget {
    fn name(&self) -> &'static str {
        "accounting-recent-journals"
    }
    fn dashboard(&self) -> &'static str {
        "accounting"
    }
    fn title(&self) -> &'static str {
        "Recent journal entries"
    }
    fn description(&self) -> &'static str {
        "The latest journal entries, newest first."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::Table
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn default_position(&self) -> Option<u8> {
        Some(7)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        let db = cx.require_db()?;
        let entries = super::journal::entry::Entity::find()
            .order_by_desc(super::journal::entry::Column::CreatedAt)
            .limit(6)
            .all(db)
            .await?;
        Ok(WidgetData::table(TableData {
            columns: vec![
                TableColumnData { label: "Number".into(), numeric: false },
                TableColumnData { label: "Date".into(), numeric: false },
                TableColumnData { label: "Memo".into(), numeric: false },
                TableColumnData { label: "Status".into(), numeric: false },
            ],
            rows: entries
                .into_iter()
                .map(|e| {
                    vec![
                        e.number.unwrap_or_else(|| "—".into()),
                        e.entry_date.format("%d %b").to_string(),
                        e.memo,
                        e.status,
                    ]
                })
                .collect(),
            empty_text: Some("No journal entries yet.".into()),
        }))
    }
}

/// The largest expense accounts this month as a list — the catalogue
/// alternative to the donut for people who want the figures.
pub struct TopExpensesWidget;

#[async_trait::async_trait]
impl WidgetDefinition for TopExpensesWidget {
    fn name(&self) -> &'static str {
        "accounting-top-expenses"
    }
    fn dashboard(&self) -> &'static str {
        "accounting"
    }
    fn title(&self) -> &'static str {
        "Largest expenses"
    }
    fn description(&self) -> &'static str {
        "This month's biggest expense accounts, with amounts."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::List
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        let db = cx.require_db()?;
        let today = chrono::Utc::now().date_naive();
        let is = LedgerQueries::new(db.clone())
            .income_statement(Some(month_start(today)), Some(today))
            .await?;
        let mut lines = is.expenses.lines;
        lines.sort_by(|a, b| b.amount.cmp(&a.amount));
        Ok(WidgetData::list(ListData {
            items: lines
                .into_iter()
                .take(5)
                .map(|l| ListItemData {
                    title: l.name,
                    subtitle: Some(l.code),
                    value: Some(money(l.amount)),
                    trend: None,
                })
                .collect(),
            empty_text: Some("No expenses booked this month.".into()),
        }))
    }
}

// ---------------------------------------------------------------------------
// Workspace tiles
// ---------------------------------------------------------------------------

pub struct WorkspaceCashPositionWidget;

#[async_trait::async_trait]
impl WidgetDefinition for WorkspaceCashPositionWidget {
    fn name(&self) -> &'static str {
        "workspace-cash-position"
    }
    fn dashboard(&self) -> &'static str {
        "workspace"
    }
    fn title(&self) -> &'static str {
        "Cash position"
    }
    fn description(&self) -> &'static str {
        "The combined balance of the cash and bank accounts."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::Stat
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn default_position(&self) -> Option<u8> {
        Some(1)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        cash_position(cx).await
    }
}

pub struct WorkspaceRevenueMonthWidget;

#[async_trait::async_trait]
impl WidgetDefinition for WorkspaceRevenueMonthWidget {
    fn name(&self) -> &'static str {
        "workspace-revenue-month"
    }
    fn dashboard(&self) -> &'static str {
        "workspace"
    }
    fn title(&self) -> &'static str {
        "Revenue this month"
    }
    fn description(&self) -> &'static str {
        "Booked revenue for the current month, against last month."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::Stat
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn default_position(&self) -> Option<u8> {
        Some(2)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        month_section(cx, false).await
    }
}

pub struct WorkspaceNetIncomeMonthWidget;

#[async_trait::async_trait]
impl WidgetDefinition for WorkspaceNetIncomeMonthWidget {
    fn name(&self) -> &'static str {
        "workspace-net-income-month"
    }
    fn dashboard(&self) -> &'static str {
        "workspace"
    }
    fn title(&self) -> &'static str {
        "Net income this month"
    }
    fn description(&self) -> &'static str {
        "Revenue minus expenses for the current month."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::Stat
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn default_position(&self) -> Option<u8> {
        Some(3)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        net_income_month(cx).await
    }
}