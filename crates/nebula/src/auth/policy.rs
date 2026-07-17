//! The password policy: the rules a password is held to, and how long it
//! stays valid.
//!
//! Two layers. The deployment sets `auth.*` in config; a tenant admin may
//! then tighten any of it from company settings. A tenant that has chosen
//! nothing inherits the deployment's values, which is why every override
//! column is nullable — the alternative, copying defaults into each tenant
//! at creation, would freeze them there and a policy tightened in config
//! would never reach the tenants already running.
//!
//! Tenants may tighten but not loosen: `auth.*` is the floor for the whole
//! deployment, mirroring how `audit.retention_max_days` bounds a tenant's
//! retention override. [`PasswordPolicy::check_override`] refuses a weaker
//! setting rather than silently clamping it, so an admin who asks for
//! something they cannot have is told.

use crate::config::AuthConfig;
use crate::error::{Error, Result};
use crate::tenancy::tenant;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// The rules actually enforced for one tenant, after resolving overrides
/// over the deployment defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PasswordPolicy {
    pub min_length: usize,
    pub require_uppercase: bool,
    pub require_lowercase: bool,
    pub require_digit: bool,
    pub require_symbol: bool,
    /// Force a change this many days after the last one. `0` never expires.
    pub expiry_days: u32,
    /// Refuse a password matching any of the last N. `0` allows reuse.
    pub history_count: u32,
    pub lockout_max_failed: i32,
    pub lockout_secs: u64,
}

impl PasswordPolicy {
    /// The deployment's policy, before any tenant override.
    pub fn from_config(config: &AuthConfig) -> Self {
        Self {
            min_length: config.password_min_length,
            require_uppercase: config.password_require_uppercase,
            require_lowercase: config.password_require_lowercase,
            require_digit: config.password_require_digit,
            require_symbol: config.password_require_symbol,
            expiry_days: config.password_expiry_days,
            history_count: config.password_history_count,
            lockout_max_failed: config.lockout_max_failed,
            lockout_secs: config.lockout_secs,
        }
    }

    /// The deployment's policy with the tenant's choices laid over it.
    /// No tenant (single-tenant mode, or a host user) means the config
    /// policy stands as-is.
    pub fn resolve(config: &AuthConfig, tenant: Option<&tenant::Model>) -> Self {
        let mut policy = Self::from_config(config);
        let Some(t) = tenant else { return policy };
        if let Some(v) = t.password_min_length {
            policy.min_length = v.max(0) as usize;
        }
        if let Some(v) = t.password_require_uppercase {
            policy.require_uppercase = v;
        }
        if let Some(v) = t.password_require_lowercase {
            policy.require_lowercase = v;
        }
        if let Some(v) = t.password_require_digit {
            policy.require_digit = v;
        }
        if let Some(v) = t.password_require_symbol {
            policy.require_symbol = v;
        }
        if let Some(v) = t.password_expiry_days {
            policy.expiry_days = v.max(0) as u32;
        }
        if let Some(v) = t.password_history_count {
            policy.history_count = v.max(0) as u32;
        }
        if let Some(v) = t.lockout_max_failed {
            policy.lockout_max_failed = v;
        }
        if let Some(v) = t.lockout_secs {
            policy.lockout_secs = v.max(0) as u64;
        }
        policy
    }

    /// Hold a candidate password to the policy. The message names every
    /// rule it broke at once: telling someone their password is too short,
    /// then that it needs a digit, then that it needs a symbol, is three
    /// round trips to learn one thing.
    pub fn check(&self, password: &str) -> Result<()> {
        let mut failures: Vec<String> = Vec::new();
        if password.chars().count() < self.min_length {
            failures.push(format!("be at least {} characters", self.min_length));
        }
        if self.require_uppercase && !password.chars().any(char::is_uppercase) {
            failures.push("contain an uppercase letter".into());
        }
        if self.require_lowercase && !password.chars().any(char::is_lowercase) {
            failures.push("contain a lowercase letter".into());
        }
        if self.require_digit && !password.chars().any(|c| c.is_ascii_digit()) {
            failures.push("contain a digit".into());
        }
        if self.require_symbol && !password.chars().any(|c| !c.is_alphanumeric()) {
            failures.push("contain a symbol".into());
        }
        if failures.is_empty() {
            return Ok(());
        }
        Err(Error::Validation(format!(
            "password must {}",
            join_and(&failures)
        )))
    }

    /// Whether a password set at this time must now be changed. A user
    /// with no recorded change date is not held to an expiry they have no
    /// clock for.
    pub fn expired(&self, changed_at: Option<DateTime<Utc>>) -> bool {
        if self.expiry_days == 0 {
            return false;
        }
        let Some(changed_at) = changed_at else {
            return false;
        };
        Utc::now() - changed_at > Duration::days(self.expiry_days as i64)
    }

    /// Refuse a tenant override that is weaker than the deployment floor.
    /// `self` is the requested policy; `config` is the floor.
    pub fn check_override(&self, config: &AuthConfig) -> Result<()> {
        let floor = Self::from_config(config);
        let mut failures: Vec<String> = Vec::new();
        if self.min_length < floor.min_length {
            failures.push(format!(
                "a minimum length below {} characters",
                floor.min_length
            ));
        }
        for (requested, required, what) in [
            (
                self.require_uppercase,
                floor.require_uppercase,
                "an uppercase letter",
            ),
            (
                self.require_lowercase,
                floor.require_lowercase,
                "a lowercase letter",
            ),
            (self.require_digit, floor.require_digit, "a digit"),
            (self.require_symbol, floor.require_symbol, "a symbol"),
        ] {
            if required && !requested {
                failures.push(format!("not requiring {what}"));
            }
        }
        // 0 means "never expires", which is the weakest setting rather
        // than the strongest — so it can only be chosen when the floor
        // does not expire either.
        if floor.expiry_days > 0 && (self.expiry_days == 0 || self.expiry_days > floor.expiry_days) {
            failures.push(format!("an expiry longer than {} days", floor.expiry_days));
        }
        if self.history_count < floor.history_count {
            failures.push(format!(
                "remembering fewer than {} previous passwords",
                floor.history_count
            ));
        }
        if self.lockout_max_failed > floor.lockout_max_failed {
            failures.push(format!(
                "more than {} failed attempts before lockout",
                floor.lockout_max_failed
            ));
        }
        if self.lockout_secs < floor.lockout_secs {
            failures.push(format!(
                "a lockout shorter than {} seconds",
                floor.lockout_secs
            ));
        }
        if failures.is_empty() {
            return Ok(());
        }
        Err(Error::Validation(format!(
            "this deployment's password policy does not allow {}",
            join_and(&failures)
        )))
    }
}

/// "a, b and c" — the message is read by a person, not parsed.
fn join_and(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AuthConfig {
        AuthConfig::default()
    }

    /// A tenant that has chosen nothing must track the deployment, not a
    /// snapshot of it — this is the whole reason the columns are nullable.
    #[test]
    fn no_override_follows_config() {
        let mut cfg = config();
        cfg.password_min_length = 12;
        assert_eq!(PasswordPolicy::resolve(&cfg, None).min_length, 12);
    }

    #[test]
    fn length_is_counted_in_characters_not_bytes() {
        let policy = PasswordPolicy {
            min_length: 8,
            ..PasswordPolicy::from_config(&config())
        };
        // Eight characters, well over eight bytes.
        assert!(policy.check("héllo-wörld").is_ok());
        assert!(policy.check("héllo").is_err());
    }

    #[test]
    fn every_broken_rule_is_reported_at_once() {
        let policy = PasswordPolicy {
            min_length: 10,
            require_uppercase: true,
            require_digit: true,
            require_symbol: true,
            ..PasswordPolicy::from_config(&config())
        };
        let err = policy.check("abc").unwrap_err().to_string();
        assert!(err.contains("at least 10 characters"), "{err}");
        assert!(err.contains("uppercase"), "{err}");
        assert!(err.contains("digit"), "{err}");
        assert!(err.contains("symbol"), "{err}");
    }

    #[test]
    fn a_compliant_password_passes() {
        let policy = PasswordPolicy {
            min_length: 10,
            require_uppercase: true,
            require_lowercase: true,
            require_digit: true,
            require_symbol: true,
            ..PasswordPolicy::from_config(&config())
        };
        assert!(policy.check("Sup3rSecret!").is_ok());
    }

    #[test]
    fn zero_expiry_never_expires() {
        let policy = PasswordPolicy {
            expiry_days: 0,
            ..PasswordPolicy::from_config(&config())
        };
        assert!(!policy.expired(Some(Utc::now() - Duration::days(3650))));
    }

    #[test]
    fn expiry_is_measured_from_the_last_change() {
        let policy = PasswordPolicy {
            expiry_days: 90,
            ..PasswordPolicy::from_config(&config())
        };
        assert!(policy.expired(Some(Utc::now() - Duration::days(91))));
        assert!(!policy.expired(Some(Utc::now() - Duration::days(89))));
    }

    /// A user whose password predates the policy has no recorded change
    /// date; locking them out of their own account over a clock we never
    /// started would be our bug, not their lapse.
    #[test]
    fn a_password_with_no_recorded_change_does_not_expire() {
        let policy = PasswordPolicy {
            expiry_days: 90,
            ..PasswordPolicy::from_config(&config())
        };
        assert!(!policy.expired(None));
    }

    #[test]
    fn a_tenant_may_tighten() {
        let mut cfg = config();
        cfg.password_min_length = 8;
        let tighter = PasswordPolicy {
            min_length: 16,
            require_symbol: true,
            ..PasswordPolicy::from_config(&cfg)
        };
        assert!(tighter.check_override(&cfg).is_ok());
    }

    #[test]
    fn a_tenant_may_not_loosen_below_the_deployment_floor() {
        let mut cfg = config();
        cfg.password_min_length = 12;
        cfg.password_require_digit = true;
        let weaker = PasswordPolicy {
            min_length: 6,
            require_digit: false,
            ..PasswordPolicy::from_config(&cfg)
        };
        let err = weaker.check_override(&cfg).unwrap_err().to_string();
        assert!(err.contains("below 12 characters"), "{err}");
        assert!(err.contains("a digit"), "{err}");
    }

    /// "Never expires" is the weakest expiry, not the strongest, so a
    /// deployment that mandates rotation cannot have it switched off.
    #[test]
    fn expiry_cannot_be_switched_off_under_a_mandate() {
        let mut cfg = config();
        cfg.password_expiry_days = 90;
        let never = PasswordPolicy {
            expiry_days: 0,
            ..PasswordPolicy::from_config(&cfg)
        };
        assert!(never.check_override(&cfg).is_err());

        let sooner = PasswordPolicy {
            expiry_days: 30,
            ..PasswordPolicy::from_config(&cfg)
        };
        assert!(sooner.check_override(&cfg).is_ok());
    }
}
