//! Time primitives. The framework's rule: **store and compute in UTC,
//! convert to the user's time zone only at the presentation edge.**
//!
//! Code that needs "now" takes a [`Clock`] instead of calling
//! `Utc::now()` directly, so time-dependent logic (due dates, posting
//! periods, token expiry) is testable with a fixed clock.

use chrono::{DateTime, Utc};
use std::sync::Arc;

pub use chrono_tz::Tz;

/// Source of the current instant. Inject `SystemClock` in production and
/// [`FixedClock`] in tests.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// The real wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock frozen at a known instant, for deterministic tests.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub DateTime<Utc>);

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

/// Shared clock handle as it will typically be injected.
pub type SharedClock = Arc<dyn Clock>;

/// Convert a UTC instant to a wall-clock time in `tz` for display.
pub fn to_local(instant: DateTime<Utc>, tz: Tz) -> DateTime<Tz> {
    instant.with_timezone(&tz)
}
