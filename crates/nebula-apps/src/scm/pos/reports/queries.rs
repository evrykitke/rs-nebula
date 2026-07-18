//! The POS reports' data layer, shared by the JSON endpoints and the report
//! engine — the API and the PDF can never disagree about a number.
//!
//! Everything derives from captured orders and the sessions that held them;
//! voided orders count only as voids. Windows are date-based: a session
//! belongs to the day it opened, an order to the day it sold.

use crate::scm::inventory::item::item;
use crate::scm::pos::register;
use crate::scm::pos::sale::{OrderKind, OrderStatus, order, order_line, order_payment};
use crate::scm::pos::session::{
    self, DenominationCount, SessionReport, SessionService, SessionStatus, session_count,
    session_money,
};
use nebula::error::{Error, Result};
use nebula::sea_orm;
use rust_decimal::Decimal;
use sea_orm::DatabaseConnection;
use sea_orm::entity::prelude::*;
use sea_orm::QueryOrder;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// One session's day, summarized.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SessionSummaryRow {
    pub session_id: Uuid,
    pub number: Option<String>,
    pub register_code: String,
    pub status: SessionStatus,
    #[schema(value_type = String, format = DateTime)]
    pub opened_at: chrono::DateTime<chrono::Utc>,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub closed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub orders: i64,
    pub refunds: i64,
    pub voids: i64,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub gross_sales: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub refund_total: Decimal,
    /// Net takings (sales − refunds), VAT included.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub net_total: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub tax_total: Decimal,
    /// Counted minus expected cash, from the counts stored at close.
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub cash_variance: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub avg_sale_seconds: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub avg_sale_inputs: Option<Decimal>,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SessionSummaryView {
    pub rows: Vec<SessionSummaryRow>,
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub gross_sales: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub refund_total: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub net_total: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub tax_total: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub cash_variance: Decimal,
}

/// One tender's share of a window.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TenderMixRow {
    pub tender: String,
    /// Payment lines seen on sales.
    pub payments: i64,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub sales: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub refunds: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub net: Decimal,
    /// This tender's slice of the net takings, in percent; `None` when the
    /// window took no money at all.
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub share_pct: Option<Decimal>,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TenderMixView {
    pub rows: Vec<TenderMixRow>,
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub net_total: Decimal,
}

/// One item's till performance over a window.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ItemSalesRow {
    pub item_id: Uuid,
    pub sku: String,
    pub name: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty_sold: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty_refunded: Decimal,
    /// Net takings for the item (sales − refunds), VAT included.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub gross: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub tax: Decimal,
    /// Net of the VAT inside `gross`.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub net: Decimal,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ItemSalesView {
    pub rows: Vec<ItemSalesRow>,
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub gross: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub tax: Decimal,
}

/// One hour of the day across the window.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HourlyRow {
    /// The local hour of day, 0–23 (see [`PosQueries::hourly`] on "local").
    pub hour: u32,
    pub sales: i64,
    pub refunds: i64,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub gross_sales: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub refund_total: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub net_total: Decimal,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HourlyView {
    pub rows: Vec<HourlyRow>,
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
    /// The minutes east of UTC the hours were bucketed in.
    pub tz_offset_minutes: i32,
}

/// A tender's stored count sheet, for the Z document.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TenderSheet {
    pub tender: String,
    pub lines: Vec<DenominationCount>,
}

/// One item line of the Z document's sales summary.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ZItemRow {
    pub description: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub gross: Decimal,
}

/// Everything the printable Z carries: the stored session report, the
/// count sheets behind the counts, and what was sold.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ZView {
    pub report: SessionReport,
    pub sheets: Vec<TenderSheet>,
    pub items: Vec<ZItemRow>,
}

pub struct PosQueries {
    db: DatabaseConnection,
}

impl PosQueries {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Sessions opened in the window, each with its money and its close.
    pub async fn sessions(
        &self,
        from: Option<chrono::NaiveDate>,
        to: Option<chrono::NaiveDate>,
        register_id: Option<Uuid>,
    ) -> Result<SessionSummaryView> {
        let mut query = session::session::Entity::find();
        if let Some(f) = from {
            query = query.filter(session::session::Column::OpenedAt.gte(day_start(f)));
        }
        if let Some(t) = to {
            query = query.filter(session::session::Column::OpenedAt.lt(day_end(t)));
        }
        if let Some(r) = register_id {
            query = query.filter(session::session::Column::RegisterId.eq(r));
        }
        let sessions = query
            .order_by_desc(session::session::Column::OpenedAt)
            .all(&self.db)
            .await?;

        let register_ids: Vec<Uuid> = sessions.iter().map(|s| s.register_id).collect();
        let registers: HashMap<Uuid, register::Model> = register::Entity::find()
            .filter(register::Column::Id.is_in(register_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|r| (r.id, r))
            .collect();
        let session_ids: Vec<Uuid> = sessions.iter().map(|s| s.id).collect();
        let cash_variances: HashMap<Uuid, Decimal> = if session_ids.is_empty() {
            HashMap::new()
        } else {
            session_count::Entity::find()
                .filter(session_count::Column::SessionId.is_in(session_ids))
                .filter(session_count::Column::Tender.eq("cash"))
                .all(&self.db)
                .await?
                .into_iter()
                .map(|c| (c.session_id, c.counted - c.expected))
                .collect()
        };

        let mut view = SessionSummaryView {
            rows: Vec::with_capacity(sessions.len()),
            from,
            to,
            gross_sales: Decimal::ZERO,
            refund_total: Decimal::ZERO,
            net_total: Decimal::ZERO,
            tax_total: Decimal::ZERO,
            cash_variance: Decimal::ZERO,
        };
        for s in &sessions {
            let money = session_money(&self.db, s).await?;
            let status = SessionStatus::parse(&s.status)?;
            let closed = status == SessionStatus::Closed;
            let variance = cash_variances.get(&s.id).copied().filter(|_| closed);
            let net = money.gross_sales - money.refund_total;
            view.gross_sales += money.gross_sales;
            view.refund_total += money.refund_total;
            view.net_total += net;
            view.tax_total += money.tax_net();
            view.cash_variance += variance.unwrap_or(Decimal::ZERO);
            view.rows.push(SessionSummaryRow {
                session_id: s.id,
                number: s.number.clone(),
                register_code: registers
                    .get(&s.register_id)
                    .map(|r| r.code.clone())
                    .unwrap_or_default(),
                status,
                opened_at: s.opened_at,
                closed_at: s.closed_at,
                orders: money.sales_count,
                refunds: money.refunds_count,
                voids: money.voids_count,
                gross_sales: money.gross_sales,
                refund_total: money.refund_total,
                net_total: net,
                tax_total: money.tax_net(),
                cash_variance: variance,
                avg_sale_seconds: s.avg_sale_seconds.or_else(|| money.avg_sale_seconds()),
                avg_sale_inputs: s.avg_sale_inputs.or_else(|| money.avg_sale_inputs()),
            });
        }
        Ok(view)
    }

    /// How the window's money arrived, per tender.
    pub async fn tender_mix(
        &self,
        from: Option<chrono::NaiveDate>,
        to: Option<chrono::NaiveDate>,
    ) -> Result<TenderMixView> {
        let orders = self.captured_orders(from, to).await?;
        let kind_of: HashMap<Uuid, OrderKind> = orders
            .iter()
            .map(|o| Ok((o.id, OrderKind::parse(&o.kind)?)))
            .collect::<Result<_>>()?;
        let payments = if orders.is_empty() {
            Vec::new()
        } else {
            order_payment::Entity::find()
                .filter(
                    order_payment::Column::OrderId
                        .is_in(orders.iter().map(|o| o.id).collect::<Vec<_>>()),
                )
                .all(&self.db)
                .await?
        };

        struct Bucket {
            payments: i64,
            sales: Decimal,
            refunds: Decimal,
        }
        let mut buckets: HashMap<String, Bucket> = HashMap::new();
        for p in payments {
            let b = buckets.entry(p.tender.clone()).or_insert(Bucket {
                payments: 0,
                sales: Decimal::ZERO,
                refunds: Decimal::ZERO,
            });
            match kind_of.get(&p.order_id) {
                Some(OrderKind::Sale) => {
                    b.payments += 1;
                    b.sales += p.amount;
                }
                Some(OrderKind::Refund) => b.refunds += p.amount,
                None => {}
            }
        }

        let net_total: Decimal = buckets.values().map(|b| b.sales - b.refunds).sum();
        let mut rows: Vec<TenderMixRow> = buckets
            .into_iter()
            .map(|(tender, b)| {
                let net = b.sales - b.refunds;
                TenderMixRow {
                    tender,
                    payments: b.payments,
                    sales: b.sales,
                    refunds: b.refunds,
                    net,
                    share_pct: (!net_total.is_zero())
                        .then(|| (net * Decimal::ONE_HUNDRED / net_total).round_dp(1)),
                }
            })
            .collect();
        rows.sort_by(|a, b| b.net.cmp(&a.net));
        Ok(TenderMixView {
            rows,
            from,
            to,
            net_total,
        })
    }

    /// What sold over the window, by item, best net takings first.
    pub async fn item_sales(
        &self,
        from: Option<chrono::NaiveDate>,
        to: Option<chrono::NaiveDate>,
    ) -> Result<ItemSalesView> {
        let orders = self.captured_orders(from, to).await?;
        let kind_of: HashMap<Uuid, OrderKind> = orders
            .iter()
            .map(|o| Ok((o.id, OrderKind::parse(&o.kind)?)))
            .collect::<Result<_>>()?;
        let lines = if orders.is_empty() {
            Vec::new()
        } else {
            order_line::Entity::find()
                .filter(
                    order_line::Column::OrderId
                        .is_in(orders.iter().map(|o| o.id).collect::<Vec<_>>()),
                )
                .all(&self.db)
                .await?
        };

        #[derive(Default)]
        struct Bucket {
            qty_sold: Decimal,
            qty_refunded: Decimal,
            gross: Decimal,
            tax: Decimal,
        }
        let mut buckets: HashMap<Uuid, Bucket> = HashMap::new();
        for l in lines {
            let b = buckets.entry(l.item_id).or_default();
            match kind_of.get(&l.order_id) {
                Some(OrderKind::Sale) => {
                    b.qty_sold += l.qty;
                    b.gross += l.net;
                    b.tax += l.tax_amount;
                }
                Some(OrderKind::Refund) => {
                    b.qty_refunded += l.qty;
                    b.gross -= l.net;
                    b.tax -= l.tax_amount;
                }
                None => {}
            }
        }

        let items: HashMap<Uuid, item::Model> = if buckets.is_empty() {
            HashMap::new()
        } else {
            item::Entity::find()
                .filter(item::Column::Id.is_in(buckets.keys().copied().collect::<Vec<_>>()))
                .all(&self.db)
                .await?
                .into_iter()
                .map(|i| (i.id, i))
                .collect()
        };

        let mut view = ItemSalesView {
            rows: Vec::with_capacity(buckets.len()),
            from,
            to,
            gross: Decimal::ZERO,
            tax: Decimal::ZERO,
        };
        for (item_id, b) in buckets {
            view.gross += b.gross;
            view.tax += b.tax;
            view.rows.push(ItemSalesRow {
                item_id,
                sku: items.get(&item_id).map(|i| i.sku.clone()).unwrap_or_default(),
                name: items
                    .get(&item_id)
                    .map(|i| i.name.clone())
                    .unwrap_or_default(),
                qty_sold: b.qty_sold,
                qty_refunded: b.qty_refunded,
                gross: b.gross,
                tax: b.tax,
                net: b.gross - b.tax,
            });
        }
        view.rows.sort_by(|a, b| b.gross.cmp(&a.gross));
        Ok(view)
    }

    /// The day's shape: takings per hour of day, summed across the window.
    /// `tz_offset_minutes` shifts the UTC `sold_at` into till-local hours
    /// (the server does not know the shop's timezone; the caller does —
    /// e.g. 180 for Nairobi).
    pub async fn hourly(
        &self,
        from: Option<chrono::NaiveDate>,
        to: Option<chrono::NaiveDate>,
        tz_offset_minutes: i32,
    ) -> Result<HourlyView> {
        let orders = self.captured_orders(from, to).await?;
        let offset = chrono::Duration::minutes(i64::from(tz_offset_minutes));

        #[derive(Default)]
        struct Bucket {
            sales: i64,
            refunds: i64,
            gross_sales: Decimal,
            refund_total: Decimal,
        }
        let mut buckets: HashMap<u32, Bucket> = HashMap::new();
        for o in &orders {
            use chrono::Timelike;
            let hour = (o.sold_at + offset).hour();
            let b = buckets.entry(hour).or_default();
            match OrderKind::parse(&o.kind)? {
                OrderKind::Sale => {
                    b.sales += 1;
                    b.gross_sales += o.total;
                }
                OrderKind::Refund => {
                    b.refunds += 1;
                    b.refund_total += o.total;
                }
            }
        }

        let mut rows: Vec<HourlyRow> = buckets
            .into_iter()
            .map(|(hour, b)| HourlyRow {
                hour,
                sales: b.sales,
                refunds: b.refunds,
                gross_sales: b.gross_sales,
                refund_total: b.refund_total,
                net_total: b.gross_sales - b.refund_total,
            })
            .collect();
        rows.sort_by_key(|r| r.hour);
        Ok(HourlyView {
            rows,
            from,
            to,
            tz_offset_minutes,
        })
    }

    /// The printable Z's data: the stored report, the count sheets, and
    /// the item summary of everything the session captured.
    pub async fn z(&self, session_id: Uuid) -> Result<ZView> {
        let report = SessionService::new(self.db.clone()).z_report(session_id).await?;

        let sheets: Vec<TenderSheet> = session_count::Entity::find()
            .filter(session_count::Column::SessionId.eq(session_id))
            .all(&self.db)
            .await?
            .into_iter()
            .filter_map(|c| {
                let lines: Vec<DenominationCount> =
                    serde_json::from_value(c.denominations?).ok()?;
                Some(TenderSheet {
                    tender: c.tender,
                    lines,
                })
            })
            .collect();

        let orders = order::Entity::find()
            .filter(order::Column::SessionId.eq(session_id))
            .filter(order::Column::Status.eq(OrderStatus::Captured.as_str()))
            .all(&self.db)
            .await?;
        let kind_of: HashMap<Uuid, OrderKind> = orders
            .iter()
            .map(|o| Ok((o.id, OrderKind::parse(&o.kind)?)))
            .collect::<Result<_>>()?;
        let lines = if orders.is_empty() {
            Vec::new()
        } else {
            order_line::Entity::find()
                .filter(
                    order_line::Column::OrderId
                        .is_in(orders.iter().map(|o| o.id).collect::<Vec<_>>()),
                )
                .all(&self.db)
                .await?
        };
        let mut buckets: HashMap<String, (Decimal, Decimal)> = HashMap::new();
        for l in lines {
            let sign = match kind_of.get(&l.order_id) {
                Some(OrderKind::Sale) => Decimal::ONE,
                Some(OrderKind::Refund) => -Decimal::ONE,
                None => continue,
            };
            let (qty, gross) = buckets.entry(l.description.clone()).or_default();
            *qty += sign * l.qty;
            *gross += sign * l.net;
        }
        let mut items: Vec<ZItemRow> = buckets
            .into_iter()
            .map(|(description, (qty, gross))| ZItemRow {
                description,
                qty,
                gross,
            })
            .collect();
        items.sort_by(|a, b| b.gross.cmp(&a.gross));

        Ok(ZView {
            report,
            sheets,
            items,
        })
    }

    /// Captured orders sold in the window, oldest first.
    async fn captured_orders(
        &self,
        from: Option<chrono::NaiveDate>,
        to: Option<chrono::NaiveDate>,
    ) -> Result<Vec<order::Model>> {
        let mut query = order::Entity::find()
            .filter(order::Column::Status.eq(OrderStatus::Captured.as_str()));
        if let Some(f) = from {
            query = query.filter(order::Column::SoldAt.gte(day_start(f)));
        }
        if let Some(t) = to {
            query = query.filter(order::Column::SoldAt.lt(day_end(t)));
        }
        query
            .order_by_asc(order::Column::SoldAt)
            .all(&self.db)
            .await
            .map_err(Error::from)
    }
}

fn day_start(d: chrono::NaiveDate) -> chrono::DateTime<chrono::Utc> {
    d.and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc()
}

/// Exclusive upper bound: the start of the following day.
fn day_end(d: chrono::NaiveDate) -> chrono::DateTime<chrono::Utc> {
    day_start(d + chrono::Duration::days(1))
}
