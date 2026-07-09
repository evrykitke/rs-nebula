//! Proof of concept: configured currencies and Money arithmetic that
//! cannot silently mix currencies, drift under rounding, or lose
//! sub-units when splitting.

use nebula::config::CurrencyConfig;
use nebula::{Currency, CurrencyRegistry, Money};
use rust_decimal::Decimal;
use std::str::FromStr;

fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

fn kes(s: &str) -> Money {
    Money::new(dec(s), Currency::new("KES", 2).unwrap())
}

fn jpy(s: &str) -> Money {
    Money::new(dec(s), Currency::new("JPY", 0).unwrap())
}

fn entry(code: &str, minor_units: u8) -> CurrencyConfig {
    CurrencyConfig {
        code: code.into(),
        minor_units,
    }
}

#[test]
fn registry_is_built_from_configuration() {
    let registry =
        CurrencyRegistry::from_config(&[entry("KES", 2), entry("JPY", 0)]).unwrap();
    assert_eq!(registry.len(), 2);
    assert_eq!(registry.get("KES").unwrap().minor_units(), 2);
    assert_eq!(registry.get("JPY").unwrap().minor_units(), 0);

    let err = registry.get("XXX").unwrap_err().to_string();
    assert!(err.contains("not configured"), "got: {err}");
}

#[test]
fn registry_rejects_duplicates_and_bad_codes() {
    assert!(CurrencyRegistry::from_config(&[entry("KES", 2), entry("KES", 2)]).is_err());
    assert!(CurrencyRegistry::from_config(&[entry("kes", 2)]).is_err());
    assert!(CurrencyRegistry::from_config(&[entry("MONEY", 2)]).is_err());
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
    let usd = Money::new(dec("100"), Currency::new("USD", 2).unwrap());
    let err = kes("100").checked_add(usd);
    assert!(err.is_err());
    let msg = err.unwrap_err().to_string();
    assert!(msg.contains("KES") && msg.contains("USD"), "got: {msg}");
}

#[test]
fn rounding_is_bankers_to_minor_units() {
    assert_eq!(kes("2.345").rounded(), kes("2.34"));
    assert_eq!(kes("2.355").rounded(), kes("2.36"));
    assert_eq!(kes("2.344999").rounded(), kes("2.34"));
    assert_eq!(jpy("100.5").rounded(), jpy("100"));
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
        .try_fold(Money::zero(kes("0").currency()), Money::checked_add)
        .unwrap();
    assert_eq!(sum, kes("100.00"));
}

#[test]
fn allocation_respects_zero_minor_unit_currencies() {
    let parts = jpy("100").allocate(3).unwrap();
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
    assert_eq!(
        json,
        r#"{"amount":"1234.50","currency":{"code":"KES","minor_units":2}}"#
    );
    let back: Money = serde_json::from_str(&json).unwrap();
    assert_eq!(back, money);

    let bad = r#"{"amount":"1","currency":{"code":"xxx","minor_units":2}}"#;
    assert!(serde_json::from_str::<Money>(bad).is_err());
}
