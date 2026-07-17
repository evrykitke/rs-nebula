//! Inventory reports: the stock position, read back.
//!
//! All four read what the stock engine maintains, via the same [`StockQueries`]
//! that serve the JSON endpoints — so PDF, Excel and the on-screen table always
//! agree with the API. The engine's data sources carry no parameters, so these
//! are whole-position reports; filtered views (one item's ledger, one
//! warehouse) live on the interactive `/inventory/stock/*` endpoints.
//!
//! - **Stock Balance** — every item × warehouse position with stock or value.
//! - **Stock Ledger** — the most recent movements, in posting order.
//! - **Stock Valuation** — moving-average value by warehouse.
//! - **Reorder Advice** — positions at or below their reorder level.
//!
//! One report per file, as with the SCM documents. What they share lives below.
//!
//! [`StockQueries`]: crate::scm::inventory::levels::StockQueries

pub mod reorder;
pub mod stock_balance;
pub mod stock_ledger;
pub mod valuation_summary;

pub use reorder::ReorderReport;
pub use stock_balance::StockBalanceReport;
pub use stock_ledger::StockLedgerReport;
pub use valuation_summary::ValuationSummaryReport;

use rust_decimal::Decimal;

/// Quantities print trimmed (10, not 10.000000); zero prints blank.
pub(crate) fn qty(v: Decimal) -> String {
    if v.is_zero() {
        String::new()
    } else {
        v.normalize().to_string()
    }
}

/// Blank for zero, otherwise two decimals — the accounting convention.
pub(crate) fn money(amount: Decimal) -> String {
    if amount.is_zero() {
        String::new()
    } else {
        format!("{:.2}", amount)
    }
}
