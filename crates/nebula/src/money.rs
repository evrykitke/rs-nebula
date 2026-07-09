//! Money: an exact decimal amount bound to a currency.
//!
//! Currencies are not hardcoded — the application defines its currency
//! table in configuration (`currencies:` in `{env}.yaml`), and the kernel
//! builds a [`CurrencyRegistry`] from it at boot.
//!
//! The rules that prevent the classic ERP money bugs:
//!
//! - amounts are [`Decimal`], never floats;
//! - arithmetic across currencies returns an error instead of silent
//!   nonsense, so only `checked_add`/`checked_sub` exist — no `+`/`-`;
//! - rounding is explicit, to the currency's minor units, using banker's
//!   rounding (midpoint-to-even) so bias does not accumulate;
//! - splitting an amount ([`Money::allocate`]) never loses or invents a
//!   sub-unit: parts differ by at most one minor unit and always sum
//!   back to the whole.

use crate::config::CurrencyConfig;
use crate::error::{Error, Result};
use rust_decimal::{Decimal, RoundingStrategy};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use std::fmt;

/// ISO 4217 code plus minor-unit count (2 for KES/USD, 0 for JPY, 3 for BHD).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Currency {
    code: [u8; 3],
    minor_units: u8,
}

impl Currency {
    pub fn new(code: &str, minor_units: u8) -> Result<Currency> {
        let bytes = code.as_bytes();
        if bytes.len() != 3 || !bytes.iter().all(|b| b.is_ascii_uppercase()) {
            return Err(Error::Validation(format!(
                "currency code must be three ASCII uppercase letters, got {code:?}"
            )));
        }
        Ok(Currency {
            code: [bytes[0], bytes[1], bytes[2]],
            minor_units,
        })
    }

    pub fn code(&self) -> &str {
        std::str::from_utf8(&self.code).expect("constructor guarantees ASCII")
    }

    pub fn minor_units(&self) -> u8 {
        self.minor_units
    }
}

impl fmt::Debug for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

#[derive(Serialize, Deserialize)]
struct CurrencyRepr {
    code: String,
    minor_units: u8,
}

impl Serialize for Currency {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        CurrencyRepr {
            code: self.code().to_string(),
            minor_units: self.minor_units,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Currency {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let repr = CurrencyRepr::deserialize(deserializer)?;
        Currency::new(&repr.code, repr.minor_units).map_err(D::Error::custom)
    }
}

/// The application's configured currencies, built by the kernel from
/// the `currencies:` section and shared with modules.
#[derive(Debug, Clone, Default)]
pub struct CurrencyRegistry {
    by_code: HashMap<String, Currency>,
}

impl CurrencyRegistry {
    pub fn from_config(entries: &[CurrencyConfig]) -> Result<Self> {
        let mut by_code = HashMap::new();
        for entry in entries {
            let currency = Currency::new(&entry.code, entry.minor_units)?;
            if by_code.insert(entry.code.clone(), currency).is_some() {
                return Err(Error::Validation(format!(
                    "currency {:?} is configured twice",
                    entry.code
                )));
            }
        }
        Ok(Self { by_code })
    }

    pub fn get(&self, code: &str) -> Result<Currency> {
        self.by_code
            .get(code)
            .copied()
            .ok_or_else(|| Error::Validation(format!("currency {code:?} is not configured")))
    }

    pub fn codes(&self) -> impl Iterator<Item = &str> {
        self.by_code.keys().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.by_code.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_code.is_empty()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Money {
    amount: Decimal,
    currency: Currency,
}

impl Money {
    pub fn new(amount: Decimal, currency: Currency) -> Self {
        Self { amount, currency }
    }

    pub fn zero(currency: Currency) -> Self {
        Self::new(Decimal::ZERO, currency)
    }

    pub fn amount(&self) -> Decimal {
        self.amount
    }

    pub fn currency(&self) -> Currency {
        self.currency
    }

    fn same_currency(&self, other: &Money, op: &str) -> Result<()> {
        if self.currency != other.currency {
            return Err(Error::Validation(format!(
                "cannot {op} {} and {}: currencies differ",
                self.currency, other.currency
            )));
        }
        Ok(())
    }

    pub fn checked_add(self, other: Money) -> Result<Money> {
        self.same_currency(&other, "add")?;
        Ok(Money::new(self.amount + other.amount, self.currency))
    }

    pub fn checked_sub(self, other: Money) -> Result<Money> {
        self.same_currency(&other, "subtract")?;
        Ok(Money::new(self.amount - other.amount, self.currency))
    }

    /// Keeps full precision; call [`Money::rounded`] when presenting or posting.
    pub fn mul(self, factor: Decimal) -> Money {
        Money::new(self.amount * factor, self.currency)
    }

    pub fn rounded(self) -> Money {
        Money::new(
            self.amount.round_dp_with_strategy(
                self.currency.minor_units as u32,
                RoundingStrategy::MidpointNearestEven,
            ),
            self.currency,
        )
    }

    /// The amount must already be exact in minor units
    /// (call [`Money::rounded`] first if needed).
    pub fn allocate(self, parts: u32) -> Result<Vec<Money>> {
        if parts == 0 {
            return Err(Error::Validation(
                "cannot allocate money into 0 parts".into(),
            ));
        }
        let scale = Decimal::from(10u64.pow(self.currency.minor_units as u32));
        let in_minor = self.amount * scale;
        if in_minor.fract() != Decimal::ZERO {
            return Err(Error::Validation(format!(
                "{self} has sub-minor-unit precision; round before allocating"
            )));
        }

        let total: i128 = in_minor
            .try_into()
            .map_err(|_| Error::Validation(format!("{self} is out of allocatable range")))?;
        let parts = parts as i128;
        let base = total.div_euclid(parts);
        let remainder = total.rem_euclid(parts);

        let make = |minor: i128| Money::new(Decimal::from(minor) / scale, self.currency);
        Ok((0..parts)
            .map(|i| make(if i < remainder { base + 1 } else { base }))
            .collect())
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.amount, self.currency)
    }
}
