//! What every app's dashboard widgets share: display formatting for stat
//! cards and the small date arithmetic behind "this month" and "last N
//! months" windows. The widgets themselves live with their modules
//! (`accounting::widgets`, `scm::*::widgets`) — this is only the common
//! presentation vocabulary, so every dashboard says "12,340.50" and
//! "+12.5% vs last month" the same way.

use chrono::Datelike;
use nebula::TrendDirection;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

/// Money for a stat card or list value: thousands-grouped, two decimals.
/// Unlike report cells, zero prints as `0.00` — a dashboard tile showing
/// nothing looks broken, not calm.
pub(crate) fn money(v: Decimal) -> String {
    let negative = v.is_sign_negative() && !v.is_zero();
    let rounded = v.abs().round_dp(2);
    let s = format!("{rounded:.2}");
    let (int, frac) = s.split_once('.').unwrap_or((s.as_str(), "00"));
    let grouped = group_thousands(int);
    if negative {
        format!("-{grouped}.{frac}")
    } else {
        format!("{grouped}.{frac}")
    }
}

/// Counts for a stat card: thousands-grouped, no decimals.
pub(crate) fn count(n: i64) -> String {
    let negative = n < 0;
    let grouped = group_thousands(&n.unsigned_abs().to_string());
    if negative { format!("-{grouped}") } else { grouped }
}

fn group_thousands(digits: &str) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Chart values are `f64` by contract; exactness belongs to the books,
/// a chart only draws proportions.
pub(crate) fn chart_value(v: Decimal) -> f64 {
    v.to_f64().unwrap_or(0.0)
}

/// A period-over-period comparison line and its direction, e.g.
/// `("+12.5% vs last month", Up)`. No delta when there is no previous
/// period to compare against.
pub(crate) fn delta_vs(
    current: Decimal,
    previous: Decimal,
    period: &str,
) -> (Option<String>, Option<TrendDirection>) {
    if previous.is_zero() {
        return (None, None);
    }
    let pct = ((current - previous) / previous.abs() * Decimal::ONE_HUNDRED).round_dp(1);
    let trend = if pct.is_zero() {
        TrendDirection::Flat
    } else if pct.is_sign_positive() {
        TrendDirection::Up
    } else {
        TrendDirection::Down
    };
    let sign = if pct.is_sign_positive() { "+" } else { "" };
    (Some(format!("{sign}{pct}% vs {period}")), Some(trend))
}

/// The first day of the month `today` falls in.
pub(crate) fn month_start(today: chrono::NaiveDate) -> chrono::NaiveDate {
    today.with_day(1).expect("day 1 exists in every month")
}

/// The previous calendar month as an inclusive date range.
pub(crate) fn previous_month(
    today: chrono::NaiveDate,
) -> (chrono::NaiveDate, chrono::NaiveDate) {
    let this_first = month_start(today);
    let prev_last = this_first.pred_opt().expect("a day precedes any month start");
    (month_start(prev_last), prev_last)
}

/// The last `n` calendar months up to (and including) the current one:
/// `(first-day, "YYYY-MM" key, short label)` — the frame a monthly chart
/// fills its buckets into, so months with no postings still appear.
pub(crate) fn last_months(
    today: chrono::NaiveDate,
    n: usize,
) -> Vec<(chrono::NaiveDate, String, String)> {
    let mut firsts = Vec::with_capacity(n);
    let mut cursor = month_start(today);
    for _ in 0..n {
        firsts.push(cursor);
        let prev_last = cursor.pred_opt().expect("a day precedes any month start");
        cursor = month_start(prev_last);
    }
    firsts.reverse();
    firsts
        .into_iter()
        .map(|d| (d, d.format("%Y-%m").to_string(), d.format("%b").to_string()))
        .collect()
}
