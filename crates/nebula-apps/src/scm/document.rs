//! The shape of an SCM document — what a purchase order, delivery note or
//! invoice looks like on paper.
//!
//! Every SCM document is the same object with different words: who it is
//! with, what identifies it, the lines, what it comes to, and the terms. So
//! the layout lives here once and each document supplies the words, rather
//! than a dozen `build` methods each arranging their own header and drifting
//! apart. The company letterhead is not here — the renderer adds that from
//! the tenant's profile, so a document never carries its own chrome.
//!
//! This is the layout the framework's widget model was made for: a `Columns`
//! of party and meta blocks above a `Table`, closed by totals, terms and a
//! signature band.

use nebula::{
    Align, Callout, CalloutStyle, Column, Group, KeyValue, Orientation, Report, Row, Signature,
    SpaceSize, Table, TextStyle, Widget,
};
use rust_decimal::Decimal;

/// One document, in the terms the layout needs. Anything optional that is
/// empty simply does not appear — a document should not print an empty
/// "Incoterms:" label because the field exists in the schema.
/// Whether a document has a number, and why not when it hasn't.
///
/// Not an `Option`: "no number yet" and "never has one" print differently, and
/// conflating them puts `(unissued)` on a statement of account, which is not a
/// thing that gets issued.
pub enum DocumentNumber {
    Issued(String),
    /// A draft of something that will be numbered when it is issued.
    Unissued,
    /// A document that never carries a number — a statement is a view of an
    /// account, not an instrument.
    Unnumbered,
}

impl From<Option<String>> for DocumentNumber {
    fn from(value: Option<String>) -> Self {
        match value {
            Some(number) => Self::Issued(number),
            None => Self::Unissued,
        }
    }
}

pub struct Document {
    /// What the document is, e.g. "Purchase Order".
    pub title: String,
    /// The document's own number.
    pub number: DocumentNumber,
    /// The lifecycle state, shown next to the number. Drafts and cancelled
    /// documents must say so on the page: a printed draft that looks issued
    /// is how a supplier ends up shipping against nothing.
    pub status: String,
    /// The heading over the party block, e.g. "Supplier" or "Bill to".
    pub party_label: &'static str,
    /// The party's name and address lines.
    pub party: Vec<String>,
    /// A second block beside the party, e.g. "Deliver to".
    pub second_label: Option<&'static str>,
    pub second: Vec<String>,
    /// Identifying fields: date, reference, currency, terms.
    pub meta: Vec<KeyValue>,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<String>>,
    /// Subtotal, discounts, tax, total — labelled, right-aligned under the
    /// lines.
    pub totals: Vec<KeyValue>,
    /// Terms and conditions, printed as a boxed note.
    pub terms: Option<String>,
    /// A free-text memo.
    pub memo: Option<String>,
    /// The sign-off band. Empty for documents nobody signs.
    pub signatures: Vec<Signature>,
    /// The small print at the foot: bank details to remit to, a delivery
    /// disclaimer, "E&OE".
    ///
    /// Its own field rather than more `memo`, because it reads differently and
    /// belongs somewhere else. A memo is about *this* record — why the price
    /// was agreed, what to do with the pallet. A footer note is standing text
    /// the business puts on every copy of the document, and it sits under the
    /// signatures where small print goes, not above them where it would
    /// interrupt.
    pub footer_notes: Vec<String>,
}

impl Document {
    /// Assemble the report. Wide line tables go landscape, since a squeezed
    /// description column is worse than a turned page.
    pub fn into_report(self) -> Report {
        let orientation = if self.columns.len() > 7 {
            Orientation::Landscape
        } else {
            Orientation::Portrait
        };

        // An unissued document must not look like it has an identity.
        let heading = match &self.number {
            DocumentNumber::Unissued => format!("{} (unissued)", self.title),
            _ => self.title.clone(),
        };

        let mut report = Report::new(heading)
            .subtitle(self.status)
            .orientation(orientation);
        if let DocumentNumber::Issued(number) = &self.number {
            report = report.number(number.clone());
        }

        // The file is named after the document, not the report: someone
        // filing three invoices needs three distinct files, and the number is
        // what they will look for. An unissued document has no number to be
        // filed under, so it falls back to the report's name.
        if let DocumentNumber::Issued(number) = &self.number {
            report = report.file_name(number.clone());
        }

        // Party and meta side by side, each in its own box: who it is with on
        // the left, what identifies it on the right — how every trade
        // document reads, and boxing them makes the two blocks share a top
        // edge instead of drifting apart.
        let mut left: Vec<Widget> = Vec::new();
        if !self.party.is_empty() {
            left.push(block(self.party_label, lines_widget(&self.party)));
        }
        if let (Some(label), false) = (self.second_label, self.second.is_empty()) {
            left.push(Widget::spacer(SpaceSize::Small));
            left.push(block(label, lines_widget(&self.second)));
        }

        let right: Vec<Widget> = if self.meta.is_empty() {
            Vec::new()
        } else {
            vec![
                Group::new(vec![Widget::KeyValues {
                    title: None,
                    items: self.meta,
                    columns: 1,
                }])
                .boxed()
                .into_widget(),
            ]
        };

        if !left.is_empty() || !right.is_empty() {
            report = report.with(Widget::Columns {
                columns: vec![left, right],
                widths: vec![3, 2],
            });
            report = report.with(Widget::spacer(SpaceSize::Small));
        }

        // The totals ride in the table's own footer when there is exactly one
        // — a grand-total row across the foot of the lines, as on a printed
        // order. Several totals (subtotal, discount, tax) cannot fit one row,
        // so they follow as their own right-aligned block instead.
        let width = self.columns.len();
        let single_total = self.totals.len() == 1;
        let footer = single_total.then(|| {
            let t = &self.totals[0];
            let mut row = vec![t.label.clone()];
            row.resize(width.saturating_sub(1), String::new());
            row.push(t.value.clone());
            row
        });

        report = report.with(Widget::Table(Table {
            title: None,
            columns: self.columns,
            // A document's lines are all the same kind of thing — the goods on
            // one order — so none is coloured apart from the others.
            rows: self.rows.into_iter().map(Row::new).collect(),
            totals: footer,
        }));

        if !self.totals.is_empty() && !single_total {
            // Right, under the amounts they sum — a ruled block, so the
            // figures line up with the column above them.
            let mut totals = Table::new(vec![Column::wide(""), Column::number("")]);
            for kv in &self.totals {
                totals = totals.row([kv.label.clone(), kv.value.clone()]);
            }
            // Two thirds empty, so the block sits under the amount columns it
            // sums rather than under the descriptions.
            report = report.with(Widget::Columns {
                columns: vec![Vec::new(), vec![Widget::Table(totals)]],
                widths: vec![2, 1],
            });
        }

        if let Some(memo) = self.memo.filter(|m| !m.trim().is_empty()) {
            report = report.with(Widget::spacer(SpaceSize::Small));
            report = report.with(Widget::styled(memo, TextStyle::Small));
        }

        if let Some(terms) = self.terms.filter(|t| !t.trim().is_empty()) {
            report = report.with(Widget::spacer(SpaceSize::Small));
            report = report.with(
                Callout::new(CalloutStyle::Muted, terms)
                    .title("Terms and conditions")
                    .into_widget(),
            );
        }

        if !self.signatures.is_empty() {
            report = report.with(Widget::spacer(SpaceSize::Medium));
            report = report.with(Widget::Signatures {
                items: self.signatures,
            });
        }

        // Last on the page, under the signatures: small print is read when it
        // is looked for, and a bank account someone has to key in is worth its
        // own line rather than a run-on paragraph.
        let notes: Vec<String> = self
            .footer_notes
            .into_iter()
            .filter(|n| !n.trim().is_empty())
            .collect();
        if !notes.is_empty() {
            report = report.with(Widget::spacer(SpaceSize::Small));
            report = report.with(Widget::Divider);
            report = report.with(Widget::styled(notes.join("\n"), TextStyle::Small));
        }

        report
    }
}

/// A titled, boxed block — the party and meta panels at the head of a
/// document.
fn block(label: &str, body: Widget) -> Widget {
    Group::new(vec![body]).title(label).boxed().into_widget()
}

/// A run of address lines. Carried as one label-less pair so the renderer
/// prints them as lines rather than indenting each against an empty label.
fn lines_widget(lines: &[String]) -> Widget {
    Widget::KeyValues {
        title: None,
        items: vec![KeyValue::new("", lines.join("\n"))],
        columns: 1,
    }
}

/// A money column: right-aligned, because digits only compare when their
/// places line up.
pub fn money_column(label: &str) -> Column {
    Column::number(label)
}

/// Amounts as they appear on a document: always two places, and always a
/// figure — a blank cell in a price column reads as missing, not as zero.
pub fn amount(value: Decimal) -> String {
    format!("{value:.2}")
}

/// A date on a document: `15-Jul-2026`.
///
/// Never all-numeric. Dates decide when goods are due, when an invoice falls
/// overdue and when a quotation dies, and these pages cross borders — a
/// reader seeing `06-07-2026` cannot know whether it means June or July, and
/// both readings are plausible. Naming the month removes the question.
pub fn date(value: chrono::NaiveDate) -> String {
    value.format("%d-%b-%Y").to_string()
}

/// A date that a record may not have set yet.
pub fn date_opt(value: Option<chrono::NaiveDate>) -> String {
    value.map(date).unwrap_or_default()
}

/// Quantities without trailing-zero noise: `10`, not `10.0000`.
pub fn quantity(value: Decimal) -> String {
    value.normalize().to_string()
}

/// A total line, e.g. `Total  1,240.00`.
pub fn total_line(label: &str, value: Decimal) -> KeyValue {
    KeyValue::new(label, amount(value))
}

/// The address block for a party: its name, then whatever contact lines it
/// has, skipping the ones it does not.
pub fn address_block(name: &str, lines: [Option<&str>; 4]) -> Vec<String> {
    let mut out = vec![name.to_string()];
    for line in lines.into_iter().flatten() {
        for part in line.lines().map(str::trim).filter(|l| !l.is_empty()) {
            out.push(part.to_string());
        }
    }
    out
}

/// Where a document's line table ends and its totals begin — the column the
/// totals align under.
pub const AMOUNT_ALIGN: Align = Align::End;

/// A grouped note, for documents that need one under the lines.
pub fn note(title: &str, body: &str) -> Widget {
    Group::new(vec![Widget::styled(body, TextStyle::Small)])
        .title(title)
        .boxed()
        .into_widget()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A document's dates must be unreadable only one way. These pages go to
    /// suppliers and customers in other countries, where an all-numeric date
    /// is a coin toss between June and July.
    #[test]
    fn dates_name_their_month() {
        let d = chrono::NaiveDate::from_ymd_opt(2026, 7, 6).unwrap();
        assert_eq!(date(d), "06-Jul-2026");
        // The reading that an all-numeric format invites must be impossible.
        assert!(!date(d).contains("06-07"));
        assert_eq!(date_opt(Some(d)), "06-Jul-2026");
        assert_eq!(date_opt(None), "");
    }

    /// Money always carries both places: `1240.5` on an order is a price
    /// someone has to read as 1240.50 without hesitating.
    #[test]
    fn amounts_always_show_cents() {
        assert_eq!(amount(Decimal::new(12405, 1)), "1240.50");
        assert_eq!(amount(Decimal::ZERO), "0.00");
    }

    /// Quantities lose the trailing zeros the database keeps: `10`, not
    /// `10.0000`.
    #[test]
    fn quantities_lose_trailing_zeros() {
        assert_eq!(quantity(Decimal::new(100000, 4)), "10");
        assert_eq!(quantity(Decimal::new(15, 1)), "1.5");
    }
}
