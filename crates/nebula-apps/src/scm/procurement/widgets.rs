//! The procurement dashboard's widgets: the purchase-to-pay position at
//! a glance. GRNI and supplier exposure reuse the report queries; the
//! AP figure reads the ledger's control account and degrades to a dash
//! when the deployment runs without accounting.

use super::order::{OrderStatus, order};
use super::permissions::names;
use super::reports::queries::ProcurementQueries;
use super::requisition::{RequisitionStatus, requisition};
use super::supplier::supplier;
use crate::widgets::{count, money};
use nebula::{
    ListData, ListItemData, Result, StatData, TableColumnData, TableData, WidgetCx, WidgetData,
    WidgetDefinition, WidgetKind, sea_orm,
};
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{DbBackend, QueryOrder, QuerySelect, Statement};
use std::collections::HashMap;
use uuid::Uuid;

pub struct OpenOrdersWidget;

#[async_trait::async_trait]
impl WidgetDefinition for OpenOrdersWidget {
    fn name(&self) -> &'static str {
        "procurement-open-orders"
    }
    fn dashboard(&self) -> &'static str {
        "procurement"
    }
    fn title(&self) -> &'static str {
        "Open purchase orders"
    }
    fn description(&self) -> &'static str {
        "Orders submitted, approved or part-received — goods still owed."
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
        let open = order::Entity::find()
            .filter(order::Column::Status.is_in([
                OrderStatus::Submitted.as_str(),
                OrderStatus::Approved.as_str(),
                OrderStatus::PartiallyReceived.as_str(),
            ]))
            .count(db)
            .await?;
        Ok(WidgetData::stat(StatData {
            value: count(open as i64),
            caption: Some("Awaiting goods".into()),
            delta: None,
            trend: None,
        }))
    }
}

pub struct PendingRequisitionsWidget;

#[async_trait::async_trait]
impl WidgetDefinition for PendingRequisitionsWidget {
    fn name(&self) -> &'static str {
        "procurement-pending-requisitions"
    }
    fn dashboard(&self) -> &'static str {
        "procurement"
    }
    fn title(&self) -> &'static str {
        "Requisitions to approve"
    }
    fn description(&self) -> &'static str {
        "Submitted purchase requisitions awaiting a decision."
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
        let pending = requisition::Entity::find()
            .filter(requisition::Column::Status.eq(RequisitionStatus::Submitted.as_str()))
            .count(db)
            .await?;
        Ok(WidgetData::stat(StatData {
            value: count(pending as i64),
            caption: Some("Awaiting approval".into()),
            delta: None,
            trend: None,
        }))
    }
}

pub struct GrniWidget;

#[async_trait::async_trait]
impl WidgetDefinition for GrniWidget {
    fn name(&self) -> &'static str {
        "procurement-grni"
    }
    fn dashboard(&self) -> &'static str {
        "procurement"
    }
    fn title(&self) -> &'static str {
        "Received, not invoiced"
    }
    fn description(&self) -> &'static str {
        "The GRNI position: goods in, bills still to come."
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
        let grni = ProcurementQueries::new(db.clone()).grni().await?;
        Ok(WidgetData::stat(StatData {
            value: money(grni.total),
            caption: Some(format!("{} order lines open", count(grni.rows.len() as i64))),
            delta: None,
            trend: None,
        }))
    }
}

pub struct ApOutstandingWidget;

#[async_trait::async_trait]
impl WidgetDefinition for ApOutstandingWidget {
    fn name(&self) -> &'static str {
        "procurement-ap-outstanding"
    }
    fn dashboard(&self) -> &'static str {
        "procurement"
    }
    fn title(&self) -> &'static str {
        "Payables outstanding"
    }
    fn description(&self) -> &'static str {
        "The AP control account's balance — what suppliers are owed."
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
        // The books may not exist on a deployment without the accounting
        // app; a dash reads better than an error tile.
        let has_accounting = db
            .query_one(Statement::from_string(
                DbBackend::Postgres,
                "SELECT to_regclass('accounting_postings') IS NOT NULL AS present",
            ))
            .await?
            .map(|r| r.try_get::<bool>("", "present").unwrap_or(false))
            .unwrap_or(false);
        if !has_accounting {
            return Ok(WidgetData::stat(StatData {
                value: "—".into(),
                caption: Some("Accounting is not enabled".into()),
                delta: None,
                trend: None,
            }));
        }
        let row = db
            .query_one(Statement::from_string(
                DbBackend::Postgres,
                "SELECT COALESCE(SUM(p.credit - p.debit), 0)::numeric AS v
                 FROM accounting_postings p
                 JOIN accounting_journal_entries e ON e.id = p.entry_id
                 JOIN accounting_accounts a ON a.id = p.account_id
                 WHERE e.status IN ('posted', 'reversed') AND a.system_key = 'ap'",
            ))
            .await?;
        let balance = row
            .map(|r| r.try_get::<Decimal>("", "v").unwrap_or(Decimal::ZERO))
            .unwrap_or(Decimal::ZERO);
        Ok(WidgetData::stat(StatData {
            value: money(balance),
            caption: Some("Per the AP control account".into()),
            delta: None,
            trend: None,
        }))
    }
}

pub struct TopSuppliersWidget;

#[async_trait::async_trait]
impl WidgetDefinition for TopSuppliersWidget {
    fn name(&self) -> &'static str {
        "procurement-top-suppliers"
    }
    fn dashboard(&self) -> &'static str {
        "procurement"
    }
    fn title(&self) -> &'static str {
        "Top suppliers"
    }
    fn description(&self) -> &'static str {
        "The suppliers billing the most, all posted invoices."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::List
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn default_position(&self) -> Option<u8> {
        Some(5)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        let db = cx.require_db()?;
        let balances = ProcurementQueries::new(db.clone()).supplier_balances().await?;
        let mut rows = balances.rows;
        rows.sort_by(|a, b| b.base_balance.cmp(&a.base_balance));
        Ok(WidgetData::list(ListData {
            items: rows
                .into_iter()
                .take(5)
                .map(|r| ListItemData {
                    title: r.name,
                    subtitle: Some(r.code),
                    value: Some(money(r.base_balance)),
                    trend: None,
                })
                .collect(),
            empty_text: Some("No supplier invoices posted yet.".into()),
        }))
    }
}

pub struct RecentOrdersWidget;

#[async_trait::async_trait]
impl WidgetDefinition for RecentOrdersWidget {
    fn name(&self) -> &'static str {
        "procurement-recent-orders"
    }
    fn dashboard(&self) -> &'static str {
        "procurement"
    }
    fn title(&self) -> &'static str {
        "Recent purchase orders"
    }
    fn description(&self) -> &'static str {
        "The latest purchase orders, newest first."
    }
    fn kind(&self) -> WidgetKind {
        WidgetKind::Table
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn default_position(&self) -> Option<u8> {
        Some(6)
    }
    async fn load(&self, cx: &WidgetCx<'_>) -> Result<WidgetData> {
        let db = cx.require_db()?;
        let orders = order::Entity::find()
            .order_by_desc(order::Column::CreatedAt)
            .limit(6)
            .all(db)
            .await?;
        let suppliers: HashMap<Uuid, supplier::Model> = supplier::Entity::find()
            .filter(
                supplier::Column::Id
                    .is_in(orders.iter().map(|o| o.supplier_id).collect::<Vec<_>>()),
            )
            .all(db)
            .await?
            .into_iter()
            .map(|s| (s.id, s))
            .collect();
        Ok(WidgetData::table(TableData {
            columns: vec![
                TableColumnData { label: "Number".into(), numeric: false },
                TableColumnData { label: "Supplier".into(), numeric: false },
                TableColumnData { label: "Date".into(), numeric: false },
                TableColumnData { label: "Status".into(), numeric: false },
            ],
            rows: orders
                .into_iter()
                .map(|o| {
                    vec![
                        o.number.clone().unwrap_or_else(|| "—".into()),
                        suppliers
                            .get(&o.supplier_id)
                            .map(|s| s.name.clone())
                            .unwrap_or_default(),
                        o.order_date.format("%d %b").to_string(),
                        o.status,
                    ]
                })
                .collect(),
            empty_text: Some("No purchase orders yet.".into()),
        }))
    }
}
