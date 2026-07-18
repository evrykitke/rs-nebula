//! The inventory dashboard's widgets, plus the stock-value tile the
//! workspace dashboard borrows. All read the level cache and the
//! movement documents through the same [`StockQueries`] the reports use.

use super::levels::{LevelView, LevelsFilter, StockQueries};
use super::moves;
use super::permissions::names;
use crate::widgets::{chart_value, count, money};
use nebula::{
    ChartData, ChartType, ListData, ListItemData, Result, SeriesData, StatData, TableColumnData,
    TableData, WidgetCx, WidgetData, WidgetDefinition, WidgetKind, sea_orm,
};
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{QueryOrder, QuerySelect};

async fn all_levels(cx: &WidgetCx<'_>) -> Result<Vec<LevelView>> {
    let db = cx.require_db()?;
    StockQueries::new(db.clone())
        .levels(LevelsFilter { warehouse_id: None, item_id: None, below_reorder: false })
        .await
}

async fn stock_value(cx: &WidgetCx<'_>) -> Result<WidgetData> {
    let levels = all_levels(cx).await?;
    let value: Decimal = levels.iter().map(|l| l.value).sum();
    let stocked = levels.iter().filter(|l| !l.on_hand.is_zero()).count();
    Ok(WidgetData::stat(StatData {
        value: money(value),
        caption: Some(format!("{} stocked positions, at moving average", count(stocked as i64))),
        delta: None,
        trend: None,
    }))
}

pub struct StockValueWidget;

#[async_trait::async_trait]
impl WidgetDefinition for StockValueWidget {
    fn name(&self) -> &'static str {
        "inventory-stock-value"
    }
    fn dashboard(&self) -> &'static str {
        "inventory"
    }
    fn title(&self) -> &'static str {
        "Stock value"
    }
    fn description(&self) -> &'static str {
        "Total stock on hand at moving-average value."
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
        stock_value(cx).await
    }
}

pub struct BelowReorderWidget;

#[async_trait::async_trait]
impl WidgetDefinition for BelowReorderWidget {
    fn name(&self) -> &'static str {
        "inventory-below-reorder"
    }
    fn dashboard(&self) -> &'static str {
        "inventory"
    }
    fn title(&self) -> &'static str {
        "Needs reordering"
    }
    fn description(&self) -> &'static str {
        "Positions at or below their reorder level."
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
        let short = StockQueries::new(db.clone())
            .levels(LevelsFilter { warehouse_id: None, item_id: None, below_reorder: true })
            .await?;
        Ok(WidgetData::stat(StatData {
            value: count(short.len() as i64),
            caption: Some("Positions at or below their reorder level".into()),
            delta: None,
            trend: None,
        }))
    }
}

pub struct ValueByWarehouseWidget;

#[async_trait::async_trait]
impl WidgetDefinition for ValueByWarehouseWidget {
    fn name(&self) -> &'static str {
        "inventory-value-by-warehouse"
    }
    fn dashboard(&self) -> &'static str {
        "inventory"
    }
    fn title(&self) -> &'static str {
        "Value by warehouse"
    }
    fn description(&self) -> &'static str {
        "How the stock value is spread across warehouses."
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
        let levels = all_levels(cx).await?;
        let mut per_warehouse: std::collections::HashMap<String, Decimal> =
            std::collections::HashMap::new();
        for l in &levels {
            *per_warehouse.entry(l.warehouse_code.clone()).or_default() += l.value;
        }
        let mut entries: Vec<(String, Decimal)> = per_warehouse
            .into_iter()
            .filter(|(_, v)| !v.is_zero())
            .collect();
        entries.sort_by(|a, b| b.1.cmp(&a.1));
        Ok(WidgetData::chart(ChartData {
            chart: ChartType::Donut,
            labels: entries.iter().map(|(w, _)| w.clone()).collect(),
            series: vec![SeriesData {
                name: "Value".into(),
                values: entries.iter().map(|(_, v)| chart_value(*v)).collect(),
            }],
            unit: None,
        }))
    }
}

pub struct TopStockItemsWidget;

#[async_trait::async_trait]
impl WidgetDefinition for TopStockItemsWidget {
    fn name(&self) -> &'static str {
        "inventory-top-stock-items"
    }
    fn dashboard(&self) -> &'static str {
        "inventory"
    }
    fn title(&self) -> &'static str {
        "Most valuable stock"
    }
    fn description(&self) -> &'static str {
        "The items holding the most value on hand."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::List
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn default_position(&self) -> Option<u8> {
        Some(4)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        let levels = all_levels(cx).await?;
        // The level cache is per item × warehouse; the tile ranks items.
        let mut per_item: std::collections::HashMap<Uuid, (String, String, Decimal)> =
            std::collections::HashMap::new();
        for l in levels {
            let entry = per_item
                .entry(l.item_id)
                .or_insert((l.item_name.clone(), l.sku.clone(), Decimal::ZERO));
            entry.2 += l.value;
        }
        let mut items: Vec<(String, String, Decimal)> = per_item.into_values().collect();
        items.sort_by(|a, b| b.2.cmp(&a.2));
        Ok(WidgetData::list(ListData {
            items: items
                .into_iter()
                .filter(|(_, _, v)| !v.is_zero())
                .take(5)
                .map(|(name, sku, value)| ListItemData {
                    title: name,
                    subtitle: Some(sku),
                    value: Some(money(value)),
                    trend: None,
                })
                .collect(),
            empty_text: Some("No stock on hand yet.".into()),
        }))
    }
}

pub struct RecentMovementsWidget;

#[async_trait::async_trait]
impl WidgetDefinition for RecentMovementsWidget {
    fn name(&self) -> &'static str {
        "inventory-recent-movements"
    }
    fn dashboard(&self) -> &'static str {
        "inventory"
    }
    fn title(&self) -> &'static str {
        "Recent movements"
    }
    fn description(&self) -> &'static str {
        "The latest stock movement documents, newest first."
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
        let rows = moves::doc::Entity::find()
            .order_by_desc(moves::doc::Column::CreatedAt)
            .limit(6)
            .all(db)
            .await?;
        Ok(WidgetData::table(TableData {
            columns: vec![
                TableColumnData { label: "Number".into(), numeric: false },
                TableColumnData { label: "Type".into(), numeric: false },
                TableColumnData { label: "Date".into(), numeric: false },
                TableColumnData { label: "Status".into(), numeric: false },
            ],
            rows: rows
                .into_iter()
                .map(|m| {
                    vec![
                        m.number.unwrap_or_else(|| "—".into()),
                        m.move_type,
                        m.entry_date.format("%d %b").to_string(),
                        m.status,
                    ]
                })
                .collect(),
            empty_text: Some("No stock movements yet.".into()),
        }))
    }
}

// ---------------------------------------------------------------------------
// Workspace tile
// ---------------------------------------------------------------------------

pub struct WorkspaceStockValueWidget;

#[async_trait::async_trait]
impl WidgetDefinition for WorkspaceStockValueWidget {
    fn name(&self) -> &'static str {
        "workspace-stock-value"
    }
    fn dashboard(&self) -> &'static str {
        "workspace"
    }
    fn title(&self) -> &'static str {
        "Stock value"
    }
    fn description(&self) -> &'static str {
        "Total stock on hand at moving-average value."
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
        stock_value(cx).await
    }
}
