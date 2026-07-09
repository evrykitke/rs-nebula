//! Proof of concept: Money arithmetic that cannot silently mix
//! currencies, drift under rounding, or lose sub-units when splitting.

use nebula::{Currency, Money};
use rust_decimal::Decimal;
use std::str::FromStr;

fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

fn kes(s: &str) -> Money {
    Money::new(dec(s), Currency::KES)
}

#[test]
fn same_currency_arithmetic_works() {
    let total = kes("100.50").checked_add(kes("49.50")).unwrap();
    assert_eq!(total, kes("150.00"));
    let rest = total.checked_sub(kes("0.01")).unwrap();
    assert_eq!(rest, kes("149.99"));
}

#[test]
fn mixing_currencies_is_an_error_not_a_bug() {
    let err = kes("100").checked_add(Money::new(dec("100"), Currency::USD));
    assert!(err.is_err());
    let msg = err.unwrap_err().to_string();
    assert!(msg.contains("KES") && msg.contains("USD"), "got: {msg}");
}

#[test]
fn rounding_is_bankers_to_minor_units() {
    assert_eq!(kes("2.345").rounded(), kes("2.34"));
    assert_eq!(kes("2.355").rounded(), kes("2.36"));
    assert_eq!(kes("2.344999").rounded(), kes("2.34"));

    let yen = Money::new(dec("100.5"), Currency::JPY).rounded();
    assert_eq!(yen.amount(), dec("100"));
}

#[test]
fn vat_style_calculation_stays_exact() {
    let net = kes("1234.56");
    let vat = net.mul(dec("0.16")).rounded();
    assert_eq!(vat, kes("197.53"));
}

#[test]
fn allocation_never_loses_a_cent() {
    let parts = kes("100.00").allocate(3).unwrap();
    let amounts: Vec<_> = parts.iter().map(|m| m.amount()).collect();
    assert_eq!(amounts, vec![dec("33.34"), dec("33.33"), dec("33.33")]);

    let sum = parts
        .into_iter()
        .try_fold(Money::zero(Currency::KES), Money::checked_add)
        .unwrap();
    assert_eq!(sum, kes("100.00"));
}

#[test]
fn allocation_respects_zero_minor_unit_currencies() {
    let parts = Money::new(dec("100"), Currency::JPY).allocate(3).unwrap();
    let amounts: Vec<_> = parts.iter().map(|m| m.amount()).collect();
    assert_eq!(amounts, vec![dec("34"), dec("33"), dec("33")]);
}

#[test]
fn allocation_requires_rounded_input() {
    assert!(kes("10.005").allocate(2).is_err());
    assert!(kes("10.00").allocate(0).is_err());
}

#[test]
fn serde_round_trip() {
    let money = kes("1234.50");
    let json = serde_json::to_string(&money).unwrap();
    assert_eq!(json, r#"{"amount":"1234.50","currency":"KES"}"#);
    let back: Money = serde_json::from_str(&json).unwrap();
    assert_eq!(back, money);

    assert!(serde_json::from_str::<Money>(r#"{"amount":"1","currency":"XXX"}"#).is_err());
}
