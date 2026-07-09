//! Money: an exact decimal amount bound to a currency.
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

use crate::error::{Error, Result};
use rust_decimal::{Decimal, RoundingStrategy};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// ISO 4217 code plus minor-unit count (2 for KES/USD, 0 for JPY, 3 for BHD).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Currency {
    code: [u8; 3],
    minor_units: u8,
}

macro_rules! currencies {
    ($($name:ident => $code:literal, $minor:literal;)+) => {
        impl Currency {
            $(pub const $name: Currency = Currency { code: *$code, minor_units: $minor };)+

            pub fn from_code(code: &str) -> Result<Currency> {
                match code.as_bytes() {
                    $($code => Ok(Currency::$name),)+
                    _ => Err(Error::Validation(format!("unknown currency code {code:?}"))),
                }
            }
        }
    };
}

currencies! {
    KES => b"KES", 2;
    TZS => b"TZS", 2;
    UGX => b"UGX", 0;
    RWF => b"RWF", 0;
    ETB => b"ETB", 2;
    ZAR => b"ZAR", 2;
    NGN => b"NGN", 2;
    GHS => b"GHS", 2;
    USD => b"USD", 2;
    EUR => b"EUR", 2;
    GBP => b"GBP", 2;
    CHF => b"CHF", 2;
    CAD => b"CAD", 2;
    AUD => b"AUD", 2;
    JPY => b"JPY", 0;
    CNY => b"CNY", 2;
    INR => b"INR", 2;
    AED => b"AED", 2;
    SAR => b"SAR", 2;
    BHD => b"BHD", 3;
}

impl Currency {
    /// For codes outside the built-in table (e.g. loyalty points).
    pub fn custom(code: &str, minor_units: u8) -> Result<Currency> {
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
        std::str::from_utf8(&self.code).expect("constructors guarantee ASCII")
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

impl Serialize for Currency {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.code())
    }
}

impl<'de> Deserialize<'de> for Currency {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let code = String::deserialize(deserializer)?;
        Currency::from_code(&code).map_err(D::Error::custom)
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
            return Err(Error::Validation("cannot allocate money into 0 parts".into()));
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
