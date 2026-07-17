//! Accounting reports: the ledger, read back.
//!
//! The three statements every set of books owes its owner. Each reads the
//! request's tenant ledger through [`LedgerQueries`], so the same report serves
//! PDF, Excel and the interactive on-screen table:
//!
//! - **Trial Balance** — every account's ending balance in its natural debit or
//!   credit column, the two footing to the same total.
//! - **Balance Sheet** — assets against liabilities and equity, as of a date.
//! - **Income Statement** — revenue against expenses over a window.
//!
//! One report per file, as with the SCM documents. What varies between them is
//! the columns and the wording, which is what a reader comes here to check — so
//! a file holds one report's decisions and nothing else. What they share lives
//! below — including the colour every statement gives an account type, which
//! has to be the same colour on all three or it teaches the reader nothing.

pub mod balance_sheet;
pub mod income_statement;
pub mod trial_balance;

pub use balance_sheet::BalanceSheetReport;
pub use income_statement::IncomeStatementReport;
pub use trial_balance::TrialBalanceReport;

use crate::accounting::account::AccountType;
use crate::accounting::ledger::StatementSection;
use nebula::{Row, RowTone, Table};
use rust_decimal::Decimal;

/// The colour an account type wears, everywhere it appears.
///
/// The five types are what a reader navigates a set of books by, and on a trial
/// balance they are interleaved — sorted by code, so an asset sits above a
/// liability above income, and the only thing saying which is which is a word
/// in the middle of the row. Colour carries that across the page.
///
/// The pairings follow the meaning rather than a palette: what you own and what
/// you owe are opposed, so blue against amber; money in and money out likewise,
/// green against red. Equity is neither, and takes violet. Kept here, in one
/// function, because the trial balance and the two statements must agree — an
/// asset that is blue on one page and green on the next is worse than no colour
/// at all.
pub(crate) fn tone_of(t: AccountType) -> RowTone {
    match t {
        AccountType::Asset => RowTone::Blue,
        AccountType::Liability => RowTone::Amber,
        AccountType::Equity => RowTone::Violet,
        AccountType::Revenue => RowTone::Green,
        AccountType::Expense => RowTone::Red,
    }
}

/// Render a statement section: a coloured heading, its lines, and a subtotal.
///
/// The heading and the subtotal are set strong and carry the type's ink; the
/// lines between them wear the same colour washed out. So a section reads as
/// one block, and the eye finds "Total liabilities" without reading it.
pub(crate) fn section_rows(mut table: Table, section: &StatementSection, t: AccountType) -> Table {
    let tone = tone_of(t);
    table = table.add(
        Row::new([section.title.clone(), String::new()])
            .tone(tone)
            .strong(),
    );
    for line in &section.lines {
        table = table.add(
            Row::new([format!("  {} {}", line.code, line.name), money(line.amount)]).tone(tone),
        );
    }
    table.add(
        Row::new([
            format!("Total {}", section.title.to_lowercase()),
            money(section.total),
        ])
        .tone(tone)
        .strong(),
    )
}

/// Blank for zero, otherwise the amount to two decimals — the accounting
/// convention that keeps the columns scannable.
pub(crate) fn money(amount: Decimal) -> String {
    if amount.is_zero() {
        String::new()
    } else {
        format!("{:.2}", amount)
    }
}

pub(crate) fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
