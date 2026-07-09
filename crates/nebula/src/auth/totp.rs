//! Time-based one-time passwords (RFC 6238) for authenticator apps:
//! 6 digits, 30-second step, SHA-1 — the parameters every authenticator
//! supports. Verification accepts one step of clock skew either way.
//!
//! Recovery codes are the offline fallback: ten single-use codes shown
//! once at setup, stored only as SHA-256 hashes.

use crate::error::{Error, Result};
use rand::Rng;
use sha2::{Digest, Sha256};
use totp_rs::{Algorithm, Secret, TOTP};

pub const RECOVERY_CODE_COUNT: usize = 10;

/// A freshly generated base32 secret for an authenticator app.
pub fn generate_secret() -> String {
    let bytes: [u8; 20] = rand::thread_rng().r#gen();
    Secret::Raw(bytes.to_vec()).to_encoded().to_string()
}

fn totp(secret_base32: &str, issuer: &str, account: &str) -> Result<TOTP> {
    let bytes = Secret::Encoded(secret_base32.to_string())
        .to_bytes()
        .map_err(|e| Error::internal(format!("invalid TOTP secret: {e:?}")))?;
    TOTP::new(
        Algorithm::SHA1,
        6,
        1,
        30,
        bytes,
        Some(issuer.to_string()),
        account.to_string(),
    )
    .map_err(|e| Error::internal(format!("invalid TOTP parameters: {e}")))
}

/// The `otpauth://` URI encoded into the QR code an authenticator scans.
pub fn provisioning_url(secret_base32: &str, issuer: &str, account: &str) -> Result<String> {
    Ok(totp(secret_base32, issuer, account)?.get_url())
}

pub fn verify_code(secret_base32: &str, code: &str) -> Result<bool> {
    let totp = totp(secret_base32, "verify", "verify")?;
    totp.check_current(code.trim())
        .map_err(|e| Error::internal(format!("system clock error: {e}")))
}

/// The current valid code — for tests and provisioning previews.
pub fn current_code(secret_base32: &str) -> Result<String> {
    let totp = totp(secret_base32, "verify", "verify")?;
    totp.generate_current()
        .map_err(|e| Error::internal(format!("system clock error: {e}")))
}

pub fn generate_recovery_codes() -> Vec<String> {
    let mut rng = rand::thread_rng();
    (0..RECOVERY_CODE_COUNT)
        .map(|_| {
            let a: u32 = rng.gen_range(0..100_000);
            let b: u32 = rng.gen_range(0..100_000);
            format!("{a:05}-{b:05}")
        })
        .collect()
}

pub fn hash_recovery_code(code: &str) -> String {
    hex::encode(Sha256::digest(code.trim().as_bytes()))
}

/// Hashes ready for the `users.recovery_codes` column.
pub fn hash_recovery_codes(codes: &[String]) -> String {
    let hashes: Vec<String> = codes.iter().map(|c| hash_recovery_code(c)).collect();
    serde_json::to_string(&hashes).expect("a vec of strings always serializes")
}

/// Consume `code` from the stored hash list; `None` means it did not
/// match, `Some(remaining)` is the updated list to persist.
pub fn consume_recovery_code(stored_json: &str, code: &str) -> Option<String> {
    let mut hashes: Vec<String> = serde_json::from_str(stored_json).ok()?;
    let needle = hash_recovery_code(code);
    let before = hashes.len();
    hashes.retain(|h| h != &needle);
    if hashes.len() == before {
        return None;
    }
    Some(serde_json::to_string(&hashes).expect("a vec of strings always serializes"))
}
