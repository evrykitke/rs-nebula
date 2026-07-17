//! Symmetric encryption for secrets the system must be able to *read
//! back* — an SMTP password has to be replayed to the mail server, so it
//! cannot be hashed like a user password.
//!
//! AES-256-GCM with a random nonce per encryption, keyed by
//! `security.encryption_key`. The stored form is `nonce || ciphertext`,
//! hex-encoded, so a column holds one self-contained string.
//!
//! Rotating the key strands everything encrypted under the old one:
//! ciphertext fails to decrypt and the setting reads as unset, which for
//! mail settings means an admin re-enters the password.

use crate::config::Secret;
use crate::error::{Error, Result};
use aes_gcm::aead::{Aead, KeyInit, OsRng, rand_core::RngCore};
use aes_gcm::{Aes256Gcm, Key, Nonce};

const NONCE_LEN: usize = 12;

/// A key derived from the configured secret. Built once and reused;
/// construction is where a bad key is caught.
#[derive(Clone)]
pub struct Cipher(Aes256Gcm);

impl Cipher {
    /// The key is the SHA-256 of the configured secret, so any passphrase
    /// length works while the cipher still gets its required 32 bytes.
    pub fn new(key: &Secret) -> Result<Self> {
        if key.is_empty() {
            return Err(Error::internal(
                "security.encryption_key is required to store secrets at rest; \
                 set it in config/{env}.local.yaml or NEBULA__SECURITY__ENCRYPTION_KEY",
            ));
        }
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(key.expose().as_bytes());
        let key: &Key<Aes256Gcm> = (&digest).into();
        Ok(Self(Aes256Gcm::new(key)))
    }

    /// Encrypt to the stored form: hex of `nonce || ciphertext`.
    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = self
            .0
            .encrypt(nonce, plaintext.as_bytes())
            .map_err(|_| Error::internal("could not encrypt the secret"))?;
        let mut out = nonce_bytes.to_vec();
        out.extend_from_slice(&ciphertext);
        Ok(hex::encode(out))
    }

    /// Decrypt the stored form. Errors on anything that is not
    /// ciphertext this key produced — a rotated key included.
    pub fn decrypt(&self, stored: &str) -> Result<Secret> {
        let raw = hex::decode(stored).map_err(|_| Error::internal("stored secret is not hex"))?;
        if raw.len() <= NONCE_LEN {
            return Err(Error::internal("stored secret is too short to be valid"));
        }
        let (nonce_bytes, ciphertext) = raw.split_at(NONCE_LEN);
        let plaintext = self
            .0
            .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
            .map_err(|_| {
                Error::internal(
                    "could not decrypt the secret; it may have been encrypted \
                     under a different security.encryption_key",
                )
            })?;
        String::from_utf8(plaintext)
            .map(Secret::new)
            .map_err(|_| Error::internal("decrypted secret is not valid UTF-8"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher() -> Cipher {
        Cipher::new(&Secret::new("a-test-encryption-key")).unwrap()
    }

    #[test]
    fn round_trips() {
        let c = cipher();
        let stored = c.encrypt("hunter2").unwrap();
        assert_eq!(c.decrypt(&stored).unwrap().expose(), "hunter2");
    }

    /// A fresh nonce each time, so the same password never produces the
    /// same column value — otherwise the database leaks which tenants
    /// share a mail password.
    #[test]
    fn same_plaintext_encrypts_differently() {
        let c = cipher();
        assert_ne!(c.encrypt("hunter2").unwrap(), c.encrypt("hunter2").unwrap());
    }

    #[test]
    fn another_key_cannot_decrypt() {
        let stored = cipher().encrypt("hunter2").unwrap();
        let other = Cipher::new(&Secret::new("a-different-key")).unwrap();
        assert!(other.decrypt(&stored).is_err());
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let c = cipher();
        let mut stored = c.encrypt("hunter2").unwrap();
        // Flip the last hex digit: GCM authenticates, so this must fail
        // rather than decrypt to garbage.
        let last = stored.pop().unwrap();
        stored.push(if last == '0' { '1' } else { '0' });
        assert!(c.decrypt(&stored).is_err());
    }

    #[test]
    fn an_empty_key_is_refused() {
        assert!(Cipher::new(&Secret::default()).is_err());
    }
}
