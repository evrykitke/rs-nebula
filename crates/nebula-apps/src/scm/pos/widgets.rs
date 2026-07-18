//! The POS dashboard's widgets, plus the takings tile the workspace
//! dashboard borrows. Everything reads captured orders and open
//! sessions through the same [`PosQueries`] the reports use; "today" is
//! the UTC date, like the report windows.

use super::permissions::names;
use super::register;
use super::reports::queries::PosQueries;
use super::sale::{OrderKind, OrderStatus, order};
use super::session::{SessionStatus, session};
use crate::widgets::{chart_value, count, delta_vs, money};
use nebula::{
    ChartData, ChartType, ListData, ListItemData, Result, SeriesData, StatData, TableColumnData,
    TableData, WidgetCx, WidgetData, WidgetDefinition, WidgetKind, sea_orm,
};
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use std::collections::HashMap;
use uuid::Uuid;

fn today() -> chrono::NaiveDate {
    chrono::Utc::now().date_naive()
}

/// Captured orders sold on `day`, split into (sales, refunds).
async fn day_orders(
    db: &sea_orm::DatabaseConnection,
    day: chrono::NaiveDate,
) -> Result<(Vec<order::Model>, Vec<order::Model>)> {
    let start = day.and_hms_opt(0, 0, 0).expect("midnight exists").and_utc();
    let end = start + chrono::Duration::days(1);
    let orders = order::Entity::find()
        .filter(order::Column::Status.eq(OrderStatus::Captured.as_str()))
        .filter(order::Column::SoldAt.gte(start))
        .filter(order::Column::SoldAt.lt(end))
        .all(db)
        .await?;
    let mut sales = Vec::new();
    let mut refunds = Vec::new();
    for o in orders {
        match OrderKind::parse(&o.kind)? {
            OrderKind::Sale => sales.push(o),
            OrderKind::Refund => refunds.push(o),
        }
    }
    Ok((sales, refunds))
}

fn net_of(sales: &[order::Model], refunds: &[order::Model]) -> Decimal {
    sales.iter().map(|o| o.total).sum::<Decimal>()
        - refunds.iter().map(|o| o.total).sum::<Decimal>()
}

async fn takings_today(cx: &WidgetCx<'_>) -> Result<WidgetData> {
    let db = cx.require_db()?;
    let (sales, refunds) = day_orders(db, today()).await?;
    let (y_sales, y_refunds) = day_orders(db, today() - chrono::Duration::days(1)).await?;
    let net = net_of(&sales, &refunds);
    let (delta, trend) = delta_vs(net, net_of(&y_sales, &y_refunds), "yesterday");
    Ok(WidgetData::stat(StatData {
        value: money(net),
        caption: Some(format!(
            "{} sales, {} refunds",
            count(sales.len() as i64),
            count(refunds.len() as i64)
        )),
        delta,
        trend,
    }))
}

pub struct TakingsTodayWidget;

#[async_trait::async_trait]
impl WidgetDefinition for TakingsTodayWidget {
    fn name(&self) -> &'static str {
        "pos-takings-today"
    }
    fn dashboard(&self) -> &'static str {
        "pos"
    }
    fn title(&self) -> &'static str {
        "Takings today"
    }
    fn description(&self) -> &'static str {
        "Net till takings today (sales minus refunds), against yesterday."
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
        takings_today(cx).await
    }
}

pub struct BasketTodayWidget;

#[async_trait::async_trait]
impl WidgetDefinition for BasketTodayWidget {
    fn name(&self) -> &'static str {
        "pos-basket-today"
    }
    fn dashboard(&self) -> &'static str {
        "pos"
    }
    fn title(&self) -> &'static str {
        "Average basket"
    }
    fn description(&self) -> &'static str {
        "Today's average sale value at the till."
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
        let db = cx.require_db()?;
        let (sales, _) = day_orders(db, today()).await?;
        let gross: Decimal = sales.iter().map(|o| o.total).sum();
        let avg = if sales.is_empty() {
            Decimal::ZERO
        } else {
            (gross / Decimal::from(sales.len() as i64)).round_dp(2)
        };
        Ok(WidgetData::stat(StatData {
            value: money(avg),
            caption: Some(format!("Across {} sales today", count(sales.len() as i64))),
            delta: None,
            trend: None,
        }))
    }
}

pub struct TenderMixTodayWidget;

#[async_trait::async_trait]
impl WidgetDefinition for TenderMixTodayWidget {
    fn name(&self) -> &'static str {
        "pos-tender-mix-today"
    }
    fn dashboard(&self) -> &'static str {
        "pos"
    }
    fn title(&self) -> &'static str {
        "Tender mix today"
    }
    fn description(&self) -> &'static str {
        "How today's money arrived: cash, M-Pesa, card."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::Chart
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn default_position(&self) -> Option<u8> {
        Some(3)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        let db = cx.require_db()?;
        let mix = PosQueries::new(db.clone())
            .tender_mix(Some(today()), Some(today()))
            .await?;
        Ok(WidgetData::chart(ChartData {
            chart: ChartType::Donut,
            labels: mix.rows.iter().map(|r| r.tender.clone()).collect(),
            series: vec![SeriesData {
                name: "Net".into(),
                values: mix.rows.iter().map(|r| chart_value(r.net)).collect(),
            }],
            unit: None,
        }))
    }
}

pub struct WeekTrendWidget;

#[async_trait::async_trait]
impl WidgetDefinition for WeekTrendWidget {
    fn name(&self) -> &'static str {
        "pos-week-trend"
    }
    fn dashboard(&self) -> &'static str {
        "pos"
    }
    fn title(&self) -> &'static str {
        "Takings this week"
    }
    fn description(&self) -> &'static str {
        "Net till takings per day, the last seven days."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::Chart
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn default_position(&self) -> Option<u8> {
        Some(4)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        let db = cx.require_db()?;
        let start_day = today() - chrono::Duration::days(6);
        let start = start_day.and_hms_opt(0, 0, 0).expect("midnight exists").and_utc();
        let orders = order::Entity::find()
            .filter(order::Column::Status.eq(OrderStatus::Captured.as_str()))
            .filter(order::Column::SoldAt.gte(start))
            .all(db)
            .await?;
        let mut per_day: HashMap<chrono::NaiveDate, Decimal> = HashMap::new();
        for o in &orders {
            let sign = match OrderKind::parse(&o.kind)? {
                OrderKind::Sale => Decimal::ONE,
                OrderKind::Refund => -Decimal::ONE,
            };
            *per_day.entry(o.sold_at.date_naive()).or_default() += sign * o.total;
        }
        let mut labels = Vec::with_capacity(7);
        let mut values = Vec::with_capacity(7);
        for i in 0..7 {
            let day = start_day + chrono::Duration::days(i);
            labels.push(day.format("%a").to_string());
            values.push(chart_value(per_day.get(&day).copied().unwrap_or(Decimal::ZERO)));
        }
        Ok(WidgetData::chart(ChartData {
            chart: ChartType::Area,
            labels,
            series: vec![SeriesData { name: "Net takings".into(), values }],
            unit: None,
        }))
    }
}

pub struct OpenSessionsWidget;

#[async_trait::async_trait]
impl WidgetDefinition for OpenSessionsWidget {
    fn name(&self) -> &'static str {
        "pos-open-sessions"
    }
    fn dashboard(&self) -> &'static str {
        "pos"
    }
    fn title(&self) -> &'static str {
        "Sessions on the floor"
    }
    fn description(&self) -> &'static str {
        "Registers currently open (or stuck closing)."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::Table
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn default_position(&self) -> Option<u8> {
        Some(5)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        let db = cx.require_db()?;
        let sessions = session::Entity::find()
            .filter(session::Column::Status.is_in([
                SessionStatus::Open.as_str(),
                SessionStatus::Closing.as_str(),
            ]))
            .all(db)
            .await?;
        let registers: HashMap<Uuid, register::Model> = register::Entity::find()
            .filter(
                register::Column::Id
                    .is_in(sessions.iter().map(|s| s.register_id).collect::<Vec<_>>()),
            )
            .all(db)
            .await?
            .into_iter()
            .map(|r| (r.id, r))
            .collect();
        Ok(WidgetData::table(TableData {
            columns: vec![
                TableColumnData { label: "Session".into(), numeric: false },
                TableColumnData { label: "Register".into(), numeric: false },
                TableColumnData { label: "Opened".into(), numeric: false },
                TableColumnData { label: "Status".into(), numeric: false },
            ],
            rows: sessions
                .into_iter()
                .map(|s| {
                    vec![
                        s.number.clone().unwrap_or_else(|| "—".into()),
                        registers
                            .get(&s.register_id)
                            .map(|r| r.code.clone())
                            .unwrap_or_default(),
                        s.opened_at.format("%d %b %H:%M").to_string(),
                        s.status,
                    ]
                })
                .collect(),
            empty_text: Some("No sessions open right now.".into()),
        }))
    }
}

pub struct TopItemsWeekWidget;

#[async_trait::async_trait]
impl WidgetDefinition for TopItemsWeekWidget {
    fn name(&self) -> &'static str {
        "pos-top-items-week"
    }
    fn dashboard(&self) -> &'static str {
        "pos"
    }
    fn title(&self) -> &'static str {
        "Best sellers this week"
    }
    fn description(&self) -> &'static str {
        "What took the most money at the tills, last seven days."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::List
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn default_position(&self) -> Option<u8> {
        Some(6)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        let db = cx.require_db()?;
        let sales = PosQueries::new(db.clone())
            .item_sales(Some(today() - chrono::Duration::days(6)), Some(today()))
            .await?;
        Ok(WidgetData::list(ListData {
            items: sales
                .rows
                .into_iter()
                .take(5)
                .map(|r| ListItemData {
                    title: r.name,
                    subtitle: Some(r.sku),
                    value: Some(money(r.gross)),
                    trend: None,
                })
                .collect(),
            empty_text: Some("Nothing sold in the last seven days.".into()),
        }))
    }
}

// ---------------------------------------------------------------------------
// Workspace tile
// ---------------------------------------------------------------------------

pub struct WorkspaceTakingsTodayWidget;

#[async_trait::async_trait]
impl WidgetDefinition for WorkspaceTakingsTodayWidget {
    fn name(&self) -> &'static str {
        "workspace-pos-takings-today"
    }
    fn dashboard(&self) -> &'static str {
        "workspace"
    }
    fn title(&self) -> &'static str {
        "Till takings today"
    }
    fn description(&self) -> &'static str {
        "Net till takings today (sales minus refunds), against yesterday."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::Stat
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn default_position(&self) -> Option<u8> {
        Some(6)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        takings_today(cx).await
    }
}
