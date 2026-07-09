//! Proof of concept: deterministic time and exact decimal arithmetic.

use chrono::{TimeZone, Utc};
use nebula::Decimal;
use nebula::time::{Clock, FixedClock, Tz, to_local};
use std::str::FromStr;

#[test]
fn fixed_clock_makes_time_dependent_logic_deterministic() {
    let instant = Utc.with_ymd_and_hms(2026, 7, 9, 12, 0, 0).unwrap();
    let clock = FixedClock(instant);
    assert_eq!(clock.now(), instant);
    assert_eq!(clock.now(), clock.now(), "a fixed clock never drifts");
}

#[test]
fn utc_converts_to_wall_clock_only_at_the_edge() {
    let instant = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
    let nairobi = to_local(instant, Tz::Africa__Nairobi);
    assert_eq!(nairobi.to_string(), "2026-01-15 15:00:00 EAT");
}

#[test]
fn decimal_money_does_not_accumulate_float_errors() {
    // The classic float trap: 0.1 + 0.2 != 0.3 in f64.
    let a = Decimal::from_str("0.1").unwrap();
    let b = Decimal::from_str("0.2").unwrap();
    assert_eq!(a + b, Decimal::from_str("0.3").unwrap());

    // Summing a cent a million times stays exact.
    let cent = Decimal::from_str("0.01").unwrap();
    let total: Decimal = std::iter::repeat_n(cent, 1_000_000).sum();
    assert_eq!(total, Decimal::from_str("10000.00").unwrap());
}
