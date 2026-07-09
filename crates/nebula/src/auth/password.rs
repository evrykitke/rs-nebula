//! Password hashing with Argon2id (explicitly — the hybrid variant
//! resistant to both GPU cracking and side-channel attacks). Hashing is
//! deliberately slow; verification failures are indistinguishable from
//! wrong passwords so nothing leaks about which part failed.

use crate::error::{Error, Result};
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, SaltString};
use argon2::{Algorithm, Argon2, Params, PasswordHasher, PasswordVerifier, Version};

fn argon2id() -> Argon2<'static> {
    Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::default())
}

pub fn hash(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    argon2id()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| Error::internal(format!("password hashing failed: {e}")))
}

pub fn verify(password: &str, stored_hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored_hash) else {
        return false;
    };
    argon2id()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}
