//! Sales reports: the accounts-receivable lens on the order-to-cash trail.
//!
//! - **AR Aging** — every posted invoice with an open balance, bucketed by
//!   how far past its due date it is (current / 1–30 / 31–60 / 61–90 / 90+).
//! - **Sales Register** — posted invoices in a window, net / tax / gross.
//! - **Delivered-Not-Billed** — order lines shipped but not yet invoiced,
//!   valued at the effective price (the outbound twin of GRNI).
//! - **Customer Statement** — opening balance, the period's invoices,
//!   credit notes and receipts, closing balance.
//! - **Sales Margins** — invoice revenue against the true moving-average
//!   COGS the deliveries booked (the ledger's own `value_delta`), per item.
//! - **AR Reconciliation** — the AR control account against Σ open invoice
//!   balances, the receivable sibling of the stock/GRNI health check.
//!
//! Every report renders through the framework engine (PDF/Excel/table) and
//! is served as JSON under `/sales/reports/*` for the client.

use crate::scm::inventory::item::item;
use crate::scm::inventory::stock::{ledger, round_money};
use crate::scm::sales::credit_note::{credit_note, credit_note_total};
use crate::scm::sales::customer::customer;
use crate::scm::sales::delivery::delivery;
use crate::scm::sales::invoice::{
    self, InvoiceStatus, invoice as sinvoice, invoice_line, load_invoice_lines,
};
use crate::scm::sales::order::{effective_price, order_line};
use crate::scm::sales::payment::{self, PaymentStatus};
use crate::scm::sales::payment::payment as spayment;
use crate::scm::sales::permissions::names;
use axum::extract::{Path, Query};
use axum::routing::get;
use axum::{Json, Router};
use nebula::auth::Authz;
use nebula::error::Error;
use nebula::{
    Column as ReportColumn, DataCx, Report, ReportData, ReportDataSource, ReportDefinition, ReportOutput, Result, Table, TenantDb, sea_orm,
};
use rust_decimal::Decimal;
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, DbBackend, QueryOrder, Statement};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

const AR_AGING_KEY: &str = "scm_ar_aging";
const DNB_KEY: &str = "scm_delivered_not_billed";
const REGISTER_KEY: &str = "scm_sales_register";
const MARGINS_KEY: &str = "scm_sales_margins";
const AR_RECON_KEY: &str = "scm_ar_reconciliation";

const AR_ROLE: &str = "ar";

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// One customer's outstanding balance, split into aging buckets.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ArAgingRow {
    pub customer_id: Uuid,
    pub code: String,
    pub name: String,
    pub currency: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub current: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub d1_30: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub d31_60: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub d61_90: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub d90_plus: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total: Decimal,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ArAgingView {
    #[schema(value_type = String, format = Date)]
    pub as_of: chrono::NaiveDate,
    pub rows: Vec<ArAgingRow>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total: Decimal,
}

/// One order line delivered but not yet billed.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DnbRow {
    pub customer_id: Uuid,
    pub customer_name: String,
    pub order_id: Uuid,
    pub order_number: Option<String>,
    pub item_id: Uuid,
    pub sku: String,
    pub item_name: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub qty: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub unit_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub value: Decimal,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DnbView {
    pub rows: Vec<DnbRow>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total: Decimal,
}

/// One posted invoice on the register.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RegisterRow {
    pub invoice_id: Uuid,
    pub number: Option<String>,
    #[schema(value_type = String, format = Date)]
    pub invoice_date: chrono::NaiveDate,
    pub customer_id: Uuid,
    pub customer_name: String,
    pub currency: String,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub net: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub tax: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total: Decimal,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RegisterView {
    pub rows: Vec<RegisterRow>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub net: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub tax: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub total: Decimal,
}

/// One item's revenue against COGS in the window.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MarginRow {
    pub item_id: Uuid,
    pub sku: String,
    pub item_name: String,
    /// Invoiced net (base currency) in the window.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub revenue: Decimal,
    /// Moving-average COGS the deliveries booked (base currency).
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub cogs: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub margin: Decimal,
    /// `margin / revenue × 100`; `None` when there was no revenue.
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub margin_pct: Option<Decimal>,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MarginsView {
    pub rows: Vec<MarginRow>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub revenue: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub cogs: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub margin: Decimal,
}

/// One line of a customer statement.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct StatementLine {
    #[schema(value_type = String, format = Date)]
    pub date: chrono::NaiveDate,
    /// invoice | credit_note | payment.
    pub kind: String,
    pub reference: Option<String>,
    /// Positive raises the balance (invoices), negative lowers it.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub amount: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub balance: Decimal,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct StatementView {
    pub customer_id: Uuid,
    pub customer_name: String,
    pub currency: String,
    #[schema(value_type = String, format = Date)]
    pub from: chrono::NaiveDate,
    #[schema(value_type = String, format = Date)]
    pub to: chrono::NaiveDate,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub opening_balance: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub closing_balance: Decimal,
    pub lines: Vec<StatementLine>,
}

/// The operational AR position against the ledger's AR control account.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ArReconciliationView {
    /// Σ open posted invoice balances (base currency).
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub ar_open: Decimal,
    /// Debit balance of the AR control account (None without accounting).
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub ar_account_balance: Option<Decimal>,
    /// `ar_open − ar_account_balance`; zero when reconciled.
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub ar_gap: Option<Decimal>,
    pub pending_outbox: i64,
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

/// Read-side queries over the sales tables.
pub struct SalesQueries {
    db: DatabaseConnection,
}

impl SalesQueries {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// All posted invoices with an open balance, aged against `as_of`.
    pub async fn ar_aging(&self, as_of: chrono::NaiveDate) -> Result<ArAgingView> {
        let invoices = sinvoice::Entity::find()
            .filter(sinvoice::Column::Status.eq(InvoiceStatus::Posted.as_str()))
            .all(&self.db)
            .await?;
        let ids: Vec<Uuid> = invoices.iter().map(|i| i.id).collect();
        let settled = payment::paid_amounts(&self.db, &ids).await?;
        let customers = self.customers_by_id(invoices.iter().map(|i| i.customer_id)).await?;

        let mut per_customer: HashMap<Uuid, ArAgingRow> = HashMap::new();
        let mut grand = Decimal::ZERO;
        for inv in &invoices {
            let total = invoice::invoice_total(&self.db, inv.id).await?;
            let open = round_money(total - settled.get(&inv.id).copied().unwrap_or(Decimal::ZERO));
            if open <= Decimal::ZERO {
                continue;
            }
            let due = inv.due_date.unwrap_or(inv.invoice_date);
            let overdue = (as_of - due).num_days();
            let c = customers.get(&inv.customer_id);
            let row = per_customer.entry(inv.customer_id).or_insert_with(|| ArAgingRow {
                customer_id: inv.customer_id,
                code: c.map(|c| c.code.clone()).unwrap_or_default(),
                name: c.map(|c| c.name.clone()).unwrap_or_default(),
                currency: inv.currency.clone(),
                current: Decimal::ZERO,
                d1_30: Decimal::ZERO,
                d31_60: Decimal::ZERO,
                d61_90: Decimal::ZERO,
                d90_plus: Decimal::ZERO,
                total: Decimal::ZERO,
            });
            match overdue {
                i64::MIN..=0 => row.current += open,
                1..=30 => row.d1_30 += open,
                31..=60 => row.d31_60 += open,
                61..=90 => row.d61_90 += open,
                _ => row.d90_plus += open,
            }
            row.total += open;
            grand += open;
        }
        let mut rows: Vec<ArAgingRow> = per_customer.into_values().collect();
        rows.sort_by(|a, b| a.code.cmp(&b.code));
        Ok(ArAgingView { as_of, rows, total: grand })
    }

    /// Order lines delivered beyond what has been billed, valued at the
    /// order's effective price.
    pub async fn delivered_not_billed(&self) -> Result<DnbView> {
        let lines = order_line::Entity::find()
            .filter(
                Expr::col(order_line::Column::DeliveredQty)
                    .gt(Expr::col(order_line::Column::BilledQty)),
            )
            .all(&self.db)
            .await?;
        let order_ids: Vec<Uuid> = lines.iter().map(|l| l.order_id).collect();
        let orders: HashMap<Uuid, crate::scm::sales::order::order::Model> =
            crate::scm::sales::order::order::Entity::find()
                .filter(crate::scm::sales::order::order::Column::Id.is_in(order_ids))
                .all(&self.db)
                .await?
                .into_iter()
                .map(|o| (o.id, o))
                .collect();
        let customers = self
            .customers_by_id(orders.values().map(|o| o.customer_id))
            .await?;
        let items = self.items_by_id(lines.iter().map(|l| l.item_id)).await?;

        let mut rows = Vec::new();
        let mut total = Decimal::ZERO;
        for l in &lines {
            let Some(order_row) = orders.get(&l.order_id) else {
                continue;
            };
            let qty = l.delivered_qty - l.billed_qty;
            let unit_price = effective_price(l.unit_price, l.discount_pct);
            let value = round_money(qty * unit_price);
            total += value;
            let item = items.get(&l.item_id);
            let c = customers.get(&order_row.customer_id);
            rows.push(DnbRow {
                customer_id: order_row.customer_id,
                customer_name: c.map(|c| c.name.clone()).unwrap_or_default(),
                order_id: order_row.id,
                order_number: order_row.number.clone(),
                item_id: l.item_id,
                sku: item.map(|i| i.sku.clone()).unwrap_or_default(),
                item_name: item.map(|i| i.name.clone()).unwrap_or_default(),
                qty,
                unit_price,
                value,
            });
        }
        rows.sort_by(|a, b| {
            (a.customer_name.as_str(), a.order_number.as_deref())
                .cmp(&(b.customer_name.as_str(), b.order_number.as_deref()))
        });
        Ok(DnbView { rows, total })
    }

    /// Posted invoices in the window, optionally filtered by customer.
    pub async fn register(
        &self,
        from: Option<chrono::NaiveDate>,
        to: Option<chrono::NaiveDate>,
        customer_id: Option<Uuid>,
    ) -> Result<RegisterView> {
        let mut query = sinvoice::Entity::find()
            .filter(sinvoice::Column::Status.eq(InvoiceStatus::Posted.as_str()));
        if let Some(f) = from {
            query = query.filter(sinvoice::Column::InvoiceDate.gte(f));
        }
        if let Some(t) = to {
            query = query.filter(sinvoice::Column::InvoiceDate.lte(t));
        }
        if let Some(cid) = customer_id {
            query = query.filter(sinvoice::Column::CustomerId.eq(cid));
        }
        let invoices = query
            .order_by_asc(sinvoice::Column::InvoiceDate)
            .all(&self.db)
            .await?;
        let customers = self.customers_by_id(invoices.iter().map(|i| i.customer_id)).await?;

        let mut rows = Vec::new();
        let (mut net_t, mut tax_t, mut total_t) = (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO);
        for inv in &invoices {
            let lines = load_invoice_lines(&self.db, inv.id).await?;
            let c = customers.get(&inv.customer_id);
            let totals = invoice::totals_for(
                &self.db,
                inv,
                &lines,
                c.map(|c| c.tax_exempt).unwrap_or(false),
            )
            .await?;
            let net = totals.total - totals.tax;
            net_t += net;
            tax_t += totals.tax;
            total_t += totals.total;
            rows.push(RegisterRow {
                invoice_id: inv.id,
                number: inv.number.clone(),
                invoice_date: inv.invoice_date,
                customer_id: inv.customer_id,
                customer_name: c.map(|c| c.name.clone()).unwrap_or_default(),
                currency: inv.currency.clone(),
                net,
                tax: totals.tax,
                total: totals.total,
            });
        }
        Ok(RegisterView {
            rows,
            net: net_t,
            tax: tax_t,
            total: total_t,
        })
    }

    /// Revenue (invoice line nets, base) against COGS (delivery ledger
    /// value) per item over the window.
    pub async fn margins(
        &self,
        from: Option<chrono::NaiveDate>,
        to: Option<chrono::NaiveDate>,
    ) -> Result<MarginsView> {
        let in_window =
            |d: chrono::NaiveDate| from.is_none_or(|f| d >= f) && to.is_none_or(|t| d <= t);

        // Revenue: posted invoice lines, mapped to the item through the
        // order line, valued net × the invoice's rate.
        let invoices: HashMap<Uuid, sinvoice::Model> = sinvoice::Entity::find()
            .filter(sinvoice::Column::Status.eq(InvoiceStatus::Posted.as_str()))
            .all(&self.db)
            .await?
            .into_iter()
            .filter(|i| in_window(i.invoice_date))
            .map(|i| (i.id, i))
            .collect();
        let inv_lines = invoice_line::Entity::find()
            .filter(invoice_line::Column::InvoiceId.is_in(invoices.keys().copied().collect::<Vec<_>>()))
            .all(&self.db)
            .await?;
        let order_line_ids: Vec<Uuid> = inv_lines.iter().filter_map(|l| l.order_line_id).collect();
        let order_lines: HashMap<Uuid, order_line::Model> = order_line::Entity::find()
            .filter(order_line::Column::Id.is_in(order_line_ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|l| (l.id, l))
            .collect();

        let mut revenue: HashMap<Uuid, Decimal> = HashMap::new();
        for l in &inv_lines {
            let Some(inv) = invoices.get(&l.invoice_id) else {
                continue;
            };
            let Some(item_id) = l
                .order_line_id
                .and_then(|id| order_lines.get(&id))
                .map(|ol| ol.item_id)
            else {
                continue;
            };
            let net = round_money(l.qty * effective_price(l.unit_price, l.discount_pct));
            *revenue.entry(item_id).or_default() += round_money(net * inv.exchange_rate);
        }

        // COGS: the delivery movements' own ledger value in the window.
        let deliveries: HashMap<Uuid, Uuid> = delivery::Entity::find()
            .all(&self.db)
            .await?
            .into_iter()
            .filter_map(|d| d.move_id.map(|m| (m, d.id)))
            .collect();
        let move_ids: Vec<Uuid> = deliveries.keys().copied().collect();
        let ledger_rows = ledger::Entity::find()
            .filter(ledger::Column::MoveId.is_in(move_ids))
            .all(&self.db)
            .await?;
        let mut cogs: HashMap<Uuid, Decimal> = HashMap::new();
        for r in &ledger_rows {
            if in_window(r.entry_date) {
                // Issues carry a negative value_delta; COGS is its opposite.
                *cogs.entry(r.item_id).or_default() += -r.value_delta;
            }
        }

        let mut item_ids: Vec<Uuid> = revenue.keys().copied().collect();
        item_ids.extend(cogs.keys().copied());
        item_ids.sort();
        item_ids.dedup();
        let items = self.items_by_id(item_ids.iter().copied()).await?;

        let mut rows = Vec::new();
        let (mut rev_t, mut cogs_t) = (Decimal::ZERO, Decimal::ZERO);
        for id in &item_ids {
            let rev = revenue.get(id).copied().unwrap_or(Decimal::ZERO);
            let cost = cogs.get(id).copied().unwrap_or(Decimal::ZERO);
            let margin = rev - cost;
            rev_t += rev;
            cogs_t += cost;
            let item = items.get(id);
            rows.push(MarginRow {
                item_id: *id,
                sku: item.map(|i| i.sku.clone()).unwrap_or_default(),
                item_name: item.map(|i| i.name.clone()).unwrap_or_default(),
                revenue: rev,
                cogs: cost,
                margin,
                margin_pct: (!rev.is_zero()).then(|| (margin / rev * Decimal::ONE_HUNDRED).round_dp(2)),
            });
        }
        rows.sort_by(|a, b| a.sku.cmp(&b.sku));
        Ok(MarginsView {
            rows,
            revenue: rev_t,
            cogs: cogs_t,
            margin: rev_t - cogs_t,
        })
    }

    /// A customer's statement: opening balance before `from`, every posted
    /// document in the window, running to the closing balance.
    pub async fn customer_statement(
        &self,
        customer_id: Uuid,
        from: chrono::NaiveDate,
        to: chrono::NaiveDate,
    ) -> Result<StatementView> {
        let cust = customer::Entity::find_by_id(customer_id)
            .one(&self.db)
            .await?
            .ok_or_else(|| Error::NotFound(format!("customer {customer_id}")))?;

        // Gather every posted document for the customer with its signed
        // amount: invoices raise the balance, credit notes and payments
        // lower it.
        struct Ev {
            date: chrono::NaiveDate,
            kind: &'static str,
            reference: Option<String>,
            amount: Decimal,
        }
        let mut events: Vec<Ev> = Vec::new();

        for inv in sinvoice::Entity::find()
            .filter(sinvoice::Column::CustomerId.eq(customer_id))
            .filter(sinvoice::Column::Status.eq(InvoiceStatus::Posted.as_str()))
            .all(&self.db)
            .await?
        {
            let total = invoice::invoice_total(&self.db, inv.id).await?;
            events.push(Ev {
                date: inv.invoice_date,
                kind: "invoice",
                reference: inv.number.clone(),
                amount: total,
            });
        }
        for n in credit_note::Entity::find()
            .filter(credit_note::Column::CustomerId.eq(customer_id))
            .filter(credit_note::Column::Status.eq("posted"))
            .all(&self.db)
            .await?
        {
            let total = credit_note_total(&self.db, n.id).await?;
            events.push(Ev {
                date: n.credit_date,
                kind: "credit_note",
                reference: n.number.clone(),
                amount: -total,
            });
        }
        for p in spayment::Entity::find()
            .filter(spayment::Column::CustomerId.eq(customer_id))
            .filter(spayment::Column::Status.eq(PaymentStatus::Posted.as_str()))
            .all(&self.db)
            .await?
        {
            events.push(Ev {
                date: p.payment_date,
                kind: "payment",
                reference: p.number.clone(),
                amount: -p.amount,
            });
        }
        events.sort_by(|a, b| a.date.cmp(&b.date));

        let opening: Decimal = events
            .iter()
            .filter(|e| e.date < from)
            .map(|e| e.amount)
            .sum();
        let mut balance = opening;
        let mut lines = Vec::new();
        for e in events.iter().filter(|e| e.date >= from && e.date <= to) {
            balance += e.amount;
            lines.push(StatementLine {
                date: e.date,
                kind: e.kind.to_string(),
                reference: e.reference.clone(),
                amount: e.amount,
                balance,
            });
        }

        Ok(StatementView {
            customer_id,
            customer_name: cust.name,
            currency: cust.currency,
            from,
            to,
            opening_balance: round_money(opening),
            closing_balance: round_money(balance),
            lines,
        })
    }

    /// The operational AR (Σ open posted invoice balances) against the
    /// ledger's AR control account.
    pub async fn ar_reconciliation(&self) -> Result<ArReconciliationView> {
        let invoices = sinvoice::Entity::find()
            .filter(sinvoice::Column::Status.eq(InvoiceStatus::Posted.as_str()))
            .all(&self.db)
            .await?;
        let ids: Vec<Uuid> = invoices.iter().map(|i| i.id).collect();
        let settled = payment::paid_amounts(&self.db, &ids).await?;
        let mut ar_open = Decimal::ZERO;
        for inv in &invoices {
            let total = invoice::invoice_total(&self.db, inv.id).await?;
            let open = round_money(
                (total - settled.get(&inv.id).copied().unwrap_or(Decimal::ZERO))
                    * inv.exchange_rate,
            );
            if open > Decimal::ZERO {
                ar_open += open;
            }
        }

        let pending_outbox = crate::scm::gl::outbox::Entity::find().count(&self.db).await? as i64;

        let has_accounting = self
            .db
            .query_one(Statement::from_string(
                DbBackend::Postgres,
                "SELECT to_regclass('accounting_postings') IS NOT NULL AS present",
            ))
            .await?
            .map(|r| r.try_get::<bool>("", "present").unwrap_or(false))
            .unwrap_or(false);
        let ar_account_balance = if has_accounting {
            let row = self
                .db
                .query_one(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT COALESCE(SUM(p.debit - p.credit), 0)::numeric AS v
                     FROM accounting_postings p
                     JOIN accounting_journal_entries e ON e.id = p.entry_id
                     JOIN accounting_accounts a ON a.id = p.account_id
                     WHERE e.status IN ('posted', 'reversed') AND a.system_key = $1",
                    [AR_ROLE.into()],
                ))
                .await?;
            Some(
                row.map(|r| r.try_get::<Decimal>("", "v").unwrap_or(Decimal::ZERO))
                    .unwrap_or(Decimal::ZERO),
            )
        } else {
            None
        };

        Ok(ArReconciliationView {
            ar_open,
            ar_account_balance,
            ar_gap: ar_account_balance.map(|b| ar_open - b),
            pending_outbox,
        })
    }

    async fn customers_by_id<I: Iterator<Item = Uuid>>(
        &self,
        ids: I,
    ) -> Result<HashMap<Uuid, customer::Model>> {
        let ids: Vec<Uuid> = ids.collect();
        Ok(customer::Entity::find()
            .filter(customer::Column::Id.is_in(ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|c| (c.id, c))
            .collect())
    }

    async fn items_by_id<I: Iterator<Item = Uuid>>(
        &self,
        ids: I,
    ) -> Result<HashMap<Uuid, item::Model>> {
        let ids: Vec<Uuid> = ids.collect();
        Ok(item::Entity::find()
            .filter(item::Column::Id.is_in(ids))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|i| (i.id, i))
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Framework reports
// ---------------------------------------------------------------------------

fn money(amount: Decimal) -> String {
    if amount.is_zero() {
        String::new()
    } else {
        format!("{:.2}", amount)
    }
}

fn qty(v: Decimal) -> String {
    if v.is_zero() {
        String::new()
    } else {
        v.normalize().to_string()
    }
}

pub struct ArAgingDataSource;
#[async_trait::async_trait]
impl ReportDataSource for ArAgingDataSource {
    fn key(&self) -> &'static str {
        AR_AGING_KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = SalesQueries::new(db.clone())
            .ar_aging(chrono::Utc::now().date_naive())
            .await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct DnbDataSource;
#[async_trait::async_trait]
impl ReportDataSource for DnbDataSource {
    fn key(&self) -> &'static str {
        DNB_KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = SalesQueries::new(db.clone()).delivered_not_billed().await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct RegisterDataSource;
#[async_trait::async_trait]
impl ReportDataSource for RegisterDataSource {
    fn key(&self) -> &'static str {
        REGISTER_KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = SalesQueries::new(db.clone()).register(None, None, None).await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct MarginsDataSource;
#[async_trait::async_trait]
impl ReportDataSource for MarginsDataSource {
    fn key(&self) -> &'static str {
        MARGINS_KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = SalesQueries::new(db.clone()).margins(None, None).await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct ArReconDataSource;
#[async_trait::async_trait]
impl ReportDataSource for ArReconDataSource {
    fn key(&self) -> &'static str {
        AR_RECON_KEY
    }
    async fn load(&self, cx: &DataCx<'_>) -> Result<serde_json::Value> {
        let db = cx.require_db()?;
        let view = SalesQueries::new(db.clone()).ar_reconciliation().await?;
        serde_json::to_value(view).map_err(|e| Error::internal(e.to_string()))
    }
}

pub struct ArAgingReport;
impl ReportDefinition for ArAgingReport {
    fn name(&self) -> &'static str {
        "ar-aging"
    }
    fn title(&self) -> &'static str {
        "AR Aging"
    }
    fn group(&self) -> &'static str {
        "Sales"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf, ReportOutput::Excel, ReportOutput::Table]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(ArAgingDataSource)]
    }
    fn build(&self, data: &ReportData) -> Result<Report> {
        let view: ArAgingView = data.get(AR_AGING_KEY)?;
        let mut table = Table::new(vec![
            ReportColumn::new("Code"),
            ReportColumn::new("Customer"),
            ReportColumn::number("Current"),
            ReportColumn::number("1–30"),
            ReportColumn::number("31–60"),
            ReportColumn::number("61–90"),
            ReportColumn::number("90+"),
            ReportColumn::number("Total"),
        ]);
        for r in &view.rows {
            table = table.row([
                r.code.clone(),
                r.name.clone(),
                money(r.current),
                money(r.d1_30),
                money(r.d31_60),
                money(r.d61_90),
                money(r.d90_plus),
                money(r.total),
            ]);
        }
        table = table.totals([
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            "Total".to_string(),
            money(view.total),
        ]);
        Ok(Report::new("AR Aging")
            .subtitle("Open customer balances by age of the due date")
            .with(table.into_widget()))
    }
}

pub struct DeliveredNotBilledReport;
impl ReportDefinition for DeliveredNotBilledReport {
    fn name(&self) -> &'static str {
        "delivered-not-billed"
    }
    fn title(&self) -> &'static str {
        "Delivered Not Billed"
    }
    fn group(&self) -> &'static str {
        "Sales"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf, ReportOutput::Excel, ReportOutput::Table]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(DnbDataSource)]
    }
    fn build(&self, data: &ReportData) -> Result<Report> {
        let view: DnbView = data.get(DNB_KEY)?;
        let mut table = Table::new(vec![
            ReportColumn::new("Customer"),
            ReportColumn::new("Order"),
            ReportColumn::new("SKU"),
            ReportColumn::new("Item"),
            ReportColumn::number("Qty"),
            ReportColumn::number("Unit price"),
            ReportColumn::number("Value"),
        ]);
        for r in &view.rows {
            table = table.row([
                r.customer_name.clone(),
                r.order_number.clone().unwrap_or_default(),
                r.sku.clone(),
                r.item_name.clone(),
                qty(r.qty),
                money(r.unit_price),
                money(r.value),
            ]);
        }
        table = table.totals([
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            "Total".to_string(),
            money(view.total),
        ]);
        Ok(Report::new("Delivered Not Billed")
            .subtitle("Order lines shipped but not yet invoiced")
            .with(table.into_widget()))
    }
}

pub struct SalesRegisterReport;
impl ReportDefinition for SalesRegisterReport {
    fn name(&self) -> &'static str {
        "sales-register"
    }
    fn title(&self) -> &'static str {
        "Sales Register"
    }
    fn group(&self) -> &'static str {
        "Sales"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf, ReportOutput::Excel, ReportOutput::Table]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(RegisterDataSource)]
    }
    fn build(&self, data: &ReportData) -> Result<Report> {
        let view: RegisterView = data.get(REGISTER_KEY)?;
        let mut table = Table::new(vec![
            ReportColumn::new("Number"),
            ReportColumn::new("Date"),
            ReportColumn::new("Customer"),
            ReportColumn::number("Net"),
            ReportColumn::number("Tax"),
            ReportColumn::number("Total"),
        ]);
        for r in &view.rows {
            table = table.row([
                r.number.clone().unwrap_or_default(),
                r.invoice_date.to_string(),
                r.customer_name.clone(),
                money(r.net),
                money(r.tax),
                money(r.total),
            ]);
        }
        table = table.totals([
            String::new(),
            String::new(),
            "Total".to_string(),
            money(view.net),
            money(view.tax),
            money(view.total),
        ]);
        Ok(Report::new("Sales Register")
            .subtitle("Posted sales invoices")
            .with(table.into_widget()))
    }
}

pub struct SalesMarginsReport;
impl ReportDefinition for SalesMarginsReport {
    fn name(&self) -> &'static str {
        "sales-margins"
    }
    fn title(&self) -> &'static str {
        "Sales Margins"
    }
    fn group(&self) -> &'static str {
        "Sales"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf, ReportOutput::Excel, ReportOutput::Table]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(MarginsDataSource)]
    }
    fn build(&self, data: &ReportData) -> Result<Report> {
        let view: MarginsView = data.get(MARGINS_KEY)?;
        let mut table = Table::new(vec![
            ReportColumn::new("SKU"),
            ReportColumn::new("Item"),
            ReportColumn::number("Revenue"),
            ReportColumn::number("COGS"),
            ReportColumn::number("Margin"),
            ReportColumn::number("Margin %"),
        ]);
        for r in &view.rows {
            table = table.row([
                r.sku.clone(),
                r.item_name.clone(),
                money(r.revenue),
                money(r.cogs),
                money(r.margin),
                r.margin_pct.map(|v| format!("{:.2}", v)).unwrap_or_default(),
            ]);
        }
        table = table.totals([
            String::new(),
            "Total".to_string(),
            money(view.revenue),
            money(view.cogs),
            money(view.margin),
            String::new(),
        ]);
        Ok(Report::new("Sales Margins")
            .subtitle("Invoiced revenue against the moving-average COGS deliveries booked")
            .with(table.into_widget()))
    }
}

pub struct ArReconciliationReport;
impl ReportDefinition for ArReconciliationReport {
    fn name(&self) -> &'static str {
        "ar-reconciliation"
    }
    fn title(&self) -> &'static str {
        "AR Reconciliation"
    }
    fn group(&self) -> &'static str {
        "Sales"
    }
    fn outputs(&self) -> &'static [ReportOutput] {
        &[ReportOutput::Pdf, ReportOutput::Excel, ReportOutput::Table]
    }
    fn permission(&self) -> Option<&'static str> {
        Some(names::REPORTS_VIEW)
    }
    fn data_sources(&self) -> Vec<Arc<dyn ReportDataSource>> {
        vec![Arc::new(ArReconDataSource)]
    }
    fn build(&self, data: &ReportData) -> Result<Report> {
        let view: ArReconciliationView = data.get(AR_RECON_KEY)?;
        let opt = |v: Option<Decimal>| v.map(|v| format!("{:.2}", v)).unwrap_or_else(|| "—".into());
        let table = Table::new(vec![
            ReportColumn::new("Measure"),
            ReportColumn::number("Operational"),
            ReportColumn::number("GL balance"),
            ReportColumn::number("Gap"),
        ])
        .row([
            "Accounts receivable".to_string(),
            format!("{:.2}", view.ar_open),
            opt(view.ar_account_balance),
            opt(view.ar_gap),
        ])
        .row([
            "Pending GL requests".to_string(),
            view.pending_outbox.to_string(),
            String::new(),
            String::new(),
        ]);
        Ok(Report::new("AR Reconciliation")
            .subtitle("Open invoice balances against the AR control account, base currency")
            .with(table.into_widget()))
    }
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AsOfQuery {
    /// The aging cut-off; defaults to today.
    pub as_of: Option<chrono::NaiveDate>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RegisterQuery {
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
    pub customer_id: Option<Uuid>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct WindowQuery {
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StatementQuery {
    pub from: chrono::NaiveDate,
    pub to: chrono::NaiveDate,
}

pub(crate) fn routes() -> Router {
    Router::new()
        .route("/sales/reports/ar-aging", get(ar_aging_json))
        .route("/sales/reports/delivered-not-billed", get(dnb_json))
        .route("/sales/reports/register", get(register_json))
        .route("/sales/reports/margins", get(margins_json))
        .route("/sales/reports/ar-reconciliation", get(ar_recon_json))
        .route("/sales/customers/{id}/statement", get(statement_json))
}

pub(crate) fn api() -> utoipa::openapi::OpenApi {
    nebula::module::build_openapi(|| <ApiDoc as utoipa::OpenApi>::openapi())
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(
    ar_aging_json,
    dnb_json,
    register_json,
    margins_json,
    ar_recon_json,
    statement_json
))]
struct ApiDoc;

#[utoipa::path(get, path = "/sales/reports/ar-aging", tag = "sales",
    params(AsOfQuery), responses((status = 200, body = ArAgingView)))]
async fn ar_aging_json(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<AsOfQuery>,
) -> Result<Json<ArAgingView>> {
    authz.require(names::REPORTS_VIEW).await?;
    let as_of = q.as_of.unwrap_or_else(|| chrono::Utc::now().date_naive());
    SalesQueries::new(db).ar_aging(as_of).await.map(Json)
}

#[utoipa::path(get, path = "/sales/reports/delivered-not-billed", tag = "sales",
    responses((status = 200, body = DnbView)))]
async fn dnb_json(authz: Authz, TenantDb(db): TenantDb) -> Result<Json<DnbView>> {
    authz.require(names::REPORTS_VIEW).await?;
    SalesQueries::new(db).delivered_not_billed().await.map(Json)
}

#[utoipa::path(get, path = "/sales/reports/register", tag = "sales",
    params(RegisterQuery), responses((status = 200, body = RegisterView)))]
async fn register_json(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<RegisterQuery>,
) -> Result<Json<RegisterView>> {
    authz.require(names::REPORTS_VIEW).await?;
    SalesQueries::new(db)
        .register(q.from, q.to, q.customer_id)
        .await
        .map(Json)
}

#[utoipa::path(get, path = "/sales/reports/margins", tag = "sales",
    params(WindowQuery), responses((status = 200, body = MarginsView)))]
async fn margins_json(
    authz: Authz,
    TenantDb(db): TenantDb,
    Query(q): Query<WindowQuery>,
) -> Result<Json<MarginsView>> {
    authz.require(names::REPORTS_VIEW).await?;
    SalesQueries::new(db).margins(q.from, q.to).await.map(Json)
}

#[utoipa::path(get, path = "/sales/reports/ar-reconciliation", tag = "sales",
    responses((status = 200, body = ArReconciliationView)))]
async fn ar_recon_json(
    authz: Authz,
    TenantDb(db): TenantDb,
) -> Result<Json<ArReconciliationView>> {
    authz.require(names::REPORTS_VIEW).await?;
    SalesQueries::new(db).ar_reconciliation().await.map(Json)
}

#[utoipa::path(get, path = "/sales/customers/{id}/statement", tag = "sales",
    params(("id" = Uuid, Path, description = "Customer id"), StatementQuery),
    responses((status = 200, body = StatementView)))]
async fn statement_json(
    authz: Authz,
    TenantDb(db): TenantDb,
    Path(id): Path<Uuid>,
    Query(q): Query<StatementQuery>,
) -> Result<Json<StatementView>> {
    authz.require(names::REPORTS_VIEW).await?;
    SalesQueries::new(db)
        .customer_statement(id, q.from, q.to)
        .await
        .map(Json)
}
