//! The sales dashboard's widgets, plus the AR tile the workspace
//! dashboard borrows. Stats that must match the reports use the same
//! [`SalesQueries`]; the monthly chart and the top-customers list bucket
//! invoice *lines* in two queries instead of totalling every invoice
//! separately — a dashboard trades the last shilling of header discounts
//! for not running a query per invoice.

use super::customer::customer;
use super::invoice::{self, InvoiceStatus, invoice as sinvoice, invoice_line};
use super::order::{OrderStatus, effective_price, order};
use super::permissions::names;
use super::reports::queries::SalesQueries;
use crate::widgets::{chart_value, count, delta_vs, last_months, money, month_start, previous_month};
use nebula::{
    ChartData, ChartType, ListData, ListItemData, Result, SeriesData, StatData, TableColumnData,
    TableData, WidgetCx, WidgetData, WidgetDefinition, WidgetKind, sea_orm,
};
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{QueryOrder, QuerySelect};
use std::collections::HashMap;
use uuid::Uuid;

/// Posted invoices dated on or after `from`, with their lines — the two
/// queries behind the line-net aggregations.
async fn posted_since(
    db: &sea_orm::DatabaseConnection,
    from: chrono::NaiveDate,
) -> Result<(Vec<sinvoice::Model>, Vec<invoice_line::Model>)> {
    let invoices = sinvoice::Entity::find()
        .filter(sinvoice::Column::Status.eq(InvoiceStatus::Posted.as_str()))
        .filter(sinvoice::Column::InvoiceDate.gte(from))
        .all(db)
        .await?;
    let lines = if invoices.is_empty() {
        Vec::new()
    } else {
        invoice_line::Entity::find()
            .filter(
                invoice_line::Column::InvoiceId
                    .is_in(invoices.iter().map(|i| i.id).collect::<Vec<_>>()),
            )
            .all(db)
            .await?
    };
    Ok((invoices, lines))
}

/// A line's base-currency net at the invoice's rate.
fn line_net(l: &invoice_line::Model, inv: &sinvoice::Model) -> Decimal {
    l.qty * effective_price(l.unit_price, l.discount_pct) * inv.exchange_rate
}

async fn ar_outstanding(cx: &WidgetCx<'_>) -> Result<WidgetData> {
    let db = cx.require_db()?;
    let aging = SalesQueries::new(db.clone())
        .ar_aging(chrono::Utc::now().date_naive())
        .await?;
    Ok(WidgetData::stat(StatData {
        value: money(aging.total),
        caption: Some(format!("{} customers owing", count(aging.rows.len() as i64))),
        delta: None,
        trend: None,
    }))
}

pub struct InvoicedMonthWidget;

#[async_trait::async_trait]
impl WidgetDefinition for InvoicedMonthWidget {
    fn name(&self) -> &'static str {
        "sales-invoiced-month"
    }
    fn dashboard(&self) -> &'static str {
        "sales"
    }
    fn title(&self) -> &'static str {
        "Invoiced this month"
    }
    fn description(&self) -> &'static str {
        "Posted invoice totals for the current month, against last month."
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
        let db = cx.require_db()?;
        let today = chrono::Utc::now().date_naive();
        let queries = SalesQueries::new(db.clone());
        let this = queries.register(Some(month_start(today)), Some(today), None).await?;
        let (prev_from, prev_to) = previous_month(today);
        let prev = queries.register(Some(prev_from), Some(prev_to), None).await?;
        let (delta, trend) = delta_vs(this.total, prev.total, "last month");
        Ok(WidgetData::stat(StatData {
            value: money(this.total),
            caption: Some(format!("{} invoices posted", count(this.rows.len() as i64))),
            delta,
            trend,
        }))
    }
}

pub struct ArOutstandingWidget;

#[async_trait::async_trait]
impl WidgetDefinition for ArOutstandingWidget {
    fn name(&self) -> &'static str {
        "sales-ar-outstanding"
    }
    fn dashboard(&self) -> &'static str {
        "sales"
    }
    fn title(&self) -> &'static str {
        "Receivables outstanding"
    }
    fn description(&self) -> &'static str {
        "What customers owe on posted invoices, all ages."
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
        ar_outstanding(cx).await
    }
}

pub struct OverdueArWidget;

#[async_trait::async_trait]
impl WidgetDefinition for OverdueArWidget {
    fn name(&self) -> &'static str {
        "sales-overdue-receivables"
    }
    fn dashboard(&self) -> &'static str {
        "sales"
    }
    fn title(&self) -> &'static str {
        "Overdue receivables"
    }
    fn description(&self) -> &'static str {
        "The slice of receivables past its due date."
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
        let db = cx.require_db()?;
        let aging = SalesQueries::new(db.clone())
            .ar_aging(chrono::Utc::now().date_naive())
            .await?;
        let overdue: Decimal = aging
            .rows
            .iter()
            .map(|r| r.d1_30 + r.d31_60 + r.d61_90 + r.d90_plus)
            .sum();
        let caption = if aging.total.is_zero() {
            "Nothing outstanding".to_string()
        } else {
            format!(
                "{}% of what is outstanding",
                (overdue / aging.total * Decimal::ONE_HUNDRED).round_dp(1)
            )
        };
        Ok(WidgetData::stat(StatData {
            value: money(overdue),
            caption: Some(caption),
            delta: None,
            trend: None,
        }))
    }
}

pub struct OpenOrdersWidget;

#[async_trait::async_trait]
impl WidgetDefinition for OpenOrdersWidget {
    fn name(&self) -> &'static str {
        "sales-open-orders"
    }
    fn dashboard(&self) -> &'static str {
        "sales"
    }
    fn title(&self) -> &'static str {
        "Open sales orders"
    }
    fn description(&self) -> &'static str {
        "Confirmed orders not yet fully delivered."
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
        let db = cx.require_db()?;
        let open = order::Entity::find()
            .filter(order::Column::Status.is_in([
                OrderStatus::Confirmed.as_str(),
                OrderStatus::PartiallyDelivered.as_str(),
            ]))
            .count(db)
            .await?;
        Ok(WidgetData::stat(StatData {
            value: count(open as i64),
            caption: Some("Confirmed or part-delivered".into()),
            delta: None,
            trend: None,
        }))
    }
}

pub struct InvoicedTrendWidget;

#[async_trait::async_trait]
impl WidgetDefinition for InvoicedTrendWidget {
    fn name(&self) -> &'static str {
        "sales-invoiced-trend"
    }
    fn dashboard(&self) -> &'static str {
        "sales"
    }
    fn title(&self) -> &'static str {
        "Invoicing trend"
    }
    fn description(&self) -> &'static str {
        "Invoiced line value by month, last six months (net of tax)."
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
        let (invoices, lines) = posted_since(db, months[0].0).await?;
        let by_id: HashMap<Uuid, &sinvoice::Model> = invoices.iter().map(|i| (i.id, i)).collect();
        let mut buckets: HashMap<String, Decimal> = HashMap::new();
        for l in &lines {
            let Some(inv) = by_id.get(&l.invoice_id) else {
                continue;
            };
            let key = inv.invoice_date.format("%Y-%m").to_string();
            *buckets.entry(key).or_default() += line_net(l, inv);
        }
        Ok(WidgetData::chart(ChartData {
            chart: ChartType::Area,
            labels: months.iter().map(|(_, _, label)| label.clone()).collect(),
            series: vec![SeriesData {
                name: "Invoiced".into(),
                values: months
                    .iter()
                    .map(|(_, key, _)| {
                        chart_value(buckets.get(key).copied().unwrap_or(Decimal::ZERO))
                    })
                    .collect(),
            }],
            unit: None,
        }))
    }
}

pub struct ArAgingChartWidget;

#[async_trait::async_trait]
impl WidgetDefinition for ArAgingChartWidget {
    fn name(&self) -> &'static str {
        "sales-ar-aging"
    }
    fn dashboard(&self) -> &'static str {
        "sales"
    }
    fn title(&self) -> &'static str {
        "Receivables by age"
    }
    fn description(&self) -> &'static str {
        "The outstanding balance in aging buckets."
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
        let aging = SalesQueries::new(db.clone())
            .ar_aging(chrono::Utc::now().date_naive())
            .await?;
        let mut buckets = [Decimal::ZERO; 5];
        for r in &aging.rows {
            buckets[0] += r.current;
            buckets[1] += r.d1_30;
            buckets[2] += r.d31_60;
            buckets[3] += r.d61_90;
            buckets[4] += r.d90_plus;
        }
        Ok(WidgetData::chart(ChartData {
            chart: ChartType::Bar,
            labels: vec![
                "Current".into(),
                "1–30".into(),
                "31–60".into(),
                "61–90".into(),
                "90+".into(),
            ],
            series: vec![SeriesData {
                name: "Outstanding".into(),
                values: buckets.iter().map(|v| chart_value(*v)).collect(),
            }],
            unit: None,
        }))
    }
}

pub struct RecentInvoicesWidget;

#[async_trait::async_trait]
impl WidgetDefinition for RecentInvoicesWidget {
    fn name(&self) -> &'static str {
        "sales-recent-invoices"
    }
    fn dashboard(&self) -> &'static str {
        "sales"
    }
    fn title(&self) -> &'static str {
        "Recent invoices"
    }
    fn description(&self) -> &'static str {
        "The latest posted invoices, newest first."
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
        let invoices = sinvoice::Entity::find()
            .filter(sinvoice::Column::Status.eq(InvoiceStatus::Posted.as_str()))
            .order_by_desc(sinvoice::Column::InvoiceDate)
            .limit(6)
            .all(db)
            .await?;
        let customers: HashMap<Uuid, customer::Model> = customer::Entity::find()
            .filter(
                customer::Column::Id
                    .is_in(invoices.iter().map(|i| i.customer_id).collect::<Vec<_>>()),
            )
            .all(db)
            .await?
            .into_iter()
            .map(|c| (c.id, c))
            .collect();
        let mut rows = Vec::with_capacity(invoices.len());
        for inv in &invoices {
            let total = invoice::invoice_total(db, inv.id).await?;
            rows.push(vec![
                inv.number.clone().unwrap_or_else(|| "—".into()),
                inv.invoice_date.format("%d %b").to_string(),
                customers
                    .get(&inv.customer_id)
                    .map(|c| c.name.clone())
                    .unwrap_or_default(),
                money(total),
            ]);
        }
        Ok(WidgetData::table(TableData {
            columns: vec![
                TableColumnData { label: "Number".into(), numeric: false },
                TableColumnData { label: "Date".into(), numeric: false },
                TableColumnData { label: "Customer".into(), numeric: false },
                TableColumnData { label: "Total".into(), numeric: true },
            ],
            rows,
            empty_text: Some("No invoices posted yet.".into()),
        }))
    }
}

pub struct TopCustomersWidget;

#[async_trait::async_trait]
impl WidgetDefinition for TopCustomersWidget {
    fn name(&self) -> &'static str {
        "sales-top-customers"
    }
    fn dashboard(&self) -> &'static str {
        "sales"
    }
    fn title(&self) -> &'static str {
        "Top customers"
    }
    fn description(&self) -> &'static str {
        "Who bought the most in the last 90 days (invoiced line value)."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::List
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        let db = cx.require_db()?;
        let from = chrono::Utc::now().date_naive() - chrono::Duration::days(90);
        let (invoices, lines) = posted_since(db, from).await?;
        let by_id: HashMap<Uuid, &sinvoice::Model> = invoices.iter().map(|i| (i.id, i)).collect();
        let mut per_customer: HashMap<Uuid, Decimal> = HashMap::new();
        for l in &lines {
            let Some(inv) = by_id.get(&l.invoice_id) else {
                continue;
            };
            *per_customer.entry(inv.customer_id).or_default() += line_net(l, inv);
        }
        let customers: HashMap<Uuid, customer::Model> = customer::Entity::find()
            .filter(customer::Column::Id.is_in(per_customer.keys().copied().collect::<Vec<_>>()))
            .all(db)
            .await?
            .into_iter()
            .map(|c| (c.id, c))
            .collect();
        let mut ranked: Vec<(Uuid, Decimal)> = per_customer.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1));
        Ok(WidgetData::list(ListData {
            items: ranked
                .into_iter()
                .take(5)
                .map(|(id, value)| {
                    let c = customers.get(&id);
                    ListItemData {
                        title: c.map(|c| c.name.clone()).unwrap_or_default(),
                        subtitle: c.map(|c| c.code.clone()),
                        value: Some(money(value)),
                        trend: None,
                    }
                })
                .collect(),
            empty_text: Some("No invoices in the last 90 days.".into()),
        }))
    }
}

// ---------------------------------------------------------------------------
// Workspace tile
// ---------------------------------------------------------------------------

pub struct WorkspaceArOutstandingWidget;

#[async_trait::async_trait]
impl WidgetDefinition for WorkspaceArOutstandingWidget {
    fn name(&self) -> &'static str {
        "workspace-ar-outstanding"
    }
    fn dashboard(&self) -> &'static str {
        "workspace"
    }
    fn title(&self) -> &'static str {
        "Receivables outstanding"
    }
    fn description(&self) -> &'static str {
        "What customers owe on posted invoices, all ages."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::Stat
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn default_position(&self) -> Option<u8> {
        Some(5)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        ar_outstanding(cx).await
    }
}
