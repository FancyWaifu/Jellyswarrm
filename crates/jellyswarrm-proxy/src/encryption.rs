//! Encryption utilities for securing server mapping passwords
//!
//! This module provides functions to encrypt and decrypt server mapping passwords
//! using the user's master password with AES-GCM encryption.

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose, Engine as _};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::string::ToString;

/// A wrapper type for plaintext passwords.
#[derive(Clone, PartialEq, Eq, Hash, sqlx::Type, Serialize, Deserialize)]
#[sqlx(transparent)]
pub struct Password(String);

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Password").field(&"***").finish()
    }
}

impl std::fmt::Display for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "***")
    }
}

impl Password {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl From<String> for Password {
    fn from(password: String) -> Self {
        Self(password)
    }
}

impl From<&str> for Password {
    fn from(password: &str) -> Self {
        Self(password.to_string())
    }
}

/// Hash a password using SHA-256
pub fn hash_password(password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    hex::encode(hasher.finalize())
}

/// A wrapper type for hashed passwords.
/// This does not contain the plaintext password, only the hashed version.
#[derive(Clone, PartialEq, Eq, Debug, Hash, sqlx::Type, Serialize, Deserialize)]
#[sqlx(transparent)]
pub struct HashedPassword(String);

impl HashedPassword {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    pub fn from_password(password: &str) -> Self {
        Self(hash_password(password))
    }

    #[cfg(test)]
    pub fn from_hashed(hashed: String) -> Self {
        Self(hashed)
    }
}

impl From<Password> for HashedPassword {
    fn from(password: Password) -> Self {
        Self::from_password(password.as_str())
    }
}

impl From<&Password> for HashedPassword {
    fn from(password: &Password) -> Self {
        Self::from_password(password.as_str())
    }
}

/// A stored local-password verifier: an Argon2id PHC string, or a legacy
/// unsalted SHA-256 hex digest that is upgraded on the next successful login.
/// Verification only; never used as key material.
#[derive(Clone, PartialEq, Eq, Hash, sqlx::Type, Serialize, Deserialize)]
#[sqlx(transparent)]
pub struct PasswordVerifier(String);

impl std::fmt::Debug for PasswordVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PasswordVerifier([redacted])")
    }
}

/// Verified against when the user doesn't exist, so a miss costs the same
/// Argon2 work as a hit and response time doesn't reveal valid usernames.
static DUMMY_VERIFIER: std::sync::LazyLock<PasswordVerifier> =
    std::sync::LazyLock::new(|| PasswordVerifier::hash(&Password::from("dummy")));

impl PasswordVerifier {
    /// Argon2id with the crate defaults (19 MiB, t=2, p=1; OWASP minimum).
    pub fn hash(password: &Password) -> Self {
        use argon2::password_hash::{PasswordHasher, SaltString};
        let mut salt = [0u8; 16];
        rand::rng().fill_bytes(&mut salt);
        let salt = SaltString::encode_b64(&salt).expect("16-byte salt encodes");
        Self(
            argon2::Argon2::default()
                .hash_password(password.as_str().as_bytes(), &salt)
                .expect("argon2 with default params cannot fail")
                .to_string(),
        )
    }

    pub fn verify(&self, password: &str) -> bool {
        use argon2::password_hash::{PasswordHash, PasswordVerifier as _};
        if self.is_legacy() {
            // Fixed-length digests compared without early exit.
            let given = hash_password(password);
            return given.len() == self.0.len()
                && given
                    .bytes()
                    .zip(self.0.bytes())
                    .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                    == 0;
        }
        PasswordHash::new(&self.0).is_ok_and(|h| {
            argon2::Argon2::default()
                .verify_password(password.as_bytes(), &h)
                .is_ok()
        })
    }

    /// True for a pre-Argon2 SHA-256 verifier.
    pub fn is_legacy(&self) -> bool {
        !self.0.starts_with("$argon2")
    }

    /// Argon2 is deliberately slow (~tens of ms): keep it off async workers.
    pub async fn hash_async(password: &Password) -> Self {
        let password = password.clone();
        tokio::task::spawn_blocking(move || Self::hash(&password))
            .await
            .expect("argon2 hashing task panicked")
    }

    pub async fn verify_async(&self, password: &Password) -> bool {
        let (this, password) = (self.clone(), password.clone());
        tokio::task::spawn_blocking(move || this.verify(password.as_str()))
            .await
            .unwrap_or(false)
    }

    /// Burn the same work as a real check for a nonexistent user.
    pub async fn dummy_verify(password: &Password) {
        DUMMY_VERIFIER.verify_async(password).await;
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn from_raw(raw: String) -> Self {
        Self(raw)
    }
}

/// A wrapper type for encrypted passwords.
#[derive(Clone, PartialEq, Eq, Debug, Hash, sqlx::Type, Serialize, Deserialize)]
#[sqlx(transparent)]
pub struct EncryptedPassword(String);

impl EncryptedPassword {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    pub fn from_raw(raw: String) -> Self {
        Self(raw)
    }
}

/// A purpose-separated key derived (HKDF) from the persistent `session_key`.
/// Unlike the legacy scheme it does not depend on any password, so stored
/// backend credentials survive password-hash changes. Never stored anywhere.
#[derive(Clone)]
pub struct MappingEncryptionKey([u8; 32]);

impl std::fmt::Debug for MappingEncryptionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MappingEncryptionKey([redacted])")
    }
}

/// Prefix marking an encrypted backend token; Jellyfin tokens are hex, so a
/// stored value without it is a legacy plaintext token.
const TOKEN_PREFIX: &str = "enc1:";

impl MappingEncryptionKey {
    pub fn for_server_mappings(session_key: &[u8]) -> Result<Self, EncryptionError> {
        Self::derive(session_key, b"jellyswarrm/server-mapping/v1")
    }

    pub fn for_authorization_sessions(session_key: &[u8]) -> Result<Self, EncryptionError> {
        Self::derive(session_key, b"jellyswarrm/authorization-session/v1")
    }

    fn derive(session_key: &[u8], purpose: &[u8]) -> Result<Self, EncryptionError> {
        if session_key.len() < 32 {
            return Err(EncryptionError::EncryptionFailed(
                "session key is too short".into(),
            ));
        }
        let mut key = [0u8; 32];
        hkdf::Hkdf::<Sha256>::new(None, session_key)
            .expand(purpose, &mut key)
            .map_err(|_| EncryptionError::EncryptionFailed("key derivation failed".into()))?;
        Ok(Self(key))
    }

    pub fn encrypt(&self, value: &Password) -> Result<EncryptedPassword, EncryptionError> {
        let mut nonce = [0u8; 12];
        rand::rng().fill_bytes(&mut nonce);
        let ciphertext = Aes256Gcm::new(&self.0.into())
            .encrypt(Nonce::from_slice(&nonce), value.as_str().as_bytes())
            .map_err(|e| EncryptionError::EncryptionFailed(e.to_string()))?;
        Ok(EncryptedPassword(
            general_purpose::STANDARD.encode([&nonce[..], &ciphertext].concat()),
        ))
    }

    pub fn decrypt(&self, value: &EncryptedPassword) -> Result<Password, EncryptionError> {
        let data = general_purpose::STANDARD.decode(value.as_str())?;
        if data.len() < 28 {
            return Err(EncryptionError::InvalidNonceSize);
        }
        let (nonce, ciphertext) = data.split_at(12);
        let plaintext = Aes256Gcm::new(&self.0.into())
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| EncryptionError::PasswordDecryptionFailed)?;
        String::from_utf8(plaintext)
            .map(Password)
            .map_err(|e| EncryptionError::DecryptionFailed(e.to_string()))
    }

    pub fn encrypt_token(&self, token: &str) -> Result<String, EncryptionError> {
        Ok(format!("{TOKEN_PREFIX}{}", self.encrypt(&token.into())?.0))
    }

    /// Decrypt a stored token; a value without the prefix is legacy plaintext.
    pub fn decrypt_token(&self, stored: &str) -> Result<String, EncryptionError> {
        match stored.strip_prefix(TOKEN_PREFIX) {
            Some(enc) => Ok(self
                .decrypt(&EncryptedPassword(enc.to_string()))?
                .into_inner()),
            None => Ok(stored.to_string()),
        }
    }

    pub fn is_encrypted_token(stored: &str) -> bool {
        stored.starts_with(TOKEN_PREFIX)
    }
}

/// True when a legacy mapping value cannot be an AES-GCM blob (not base64, or
/// shorter than nonce + tag), i.e. it was stored as plaintext. `false` is
/// ambiguous: a plaintext password can also be valid base64 of that length.
pub fn is_legacy_plaintext(value: &EncryptedPassword) -> bool {
    general_purpose::STANDARD
        .decode(value.as_str())
        .map_or(true, |bytes| bytes.len() < 28)
}

/// Custom error type for encryption/decryption operations
#[derive(Debug, thiserror::Error)]
pub enum EncryptionError {
    #[error("Encryption failed: {0}")]
    EncryptionFailed(String),
    #[error("Decryption failed: {0}")]
    DecryptionFailed(String),
    #[error("Base64 decoding failed: {0}")]
    Base64DecodeFailed(#[from] base64::DecodeError),
    #[error("Invalid nonce size")]
    InvalidNonceSize,
    #[error("Password decryption failed - possibly incorrect password")]
    PasswordDecryptionFailed,
}

/// Encrypts a password using the provided master password
///
/// # Arguments
/// * `plaintext` - The password to encrypt
/// * `master_password` - The hashed master password used as encryption key
///
/// # Returns
/// Base64-encoded string containing the nonce and encrypted data
pub fn encrypt_password(
    plaintext: &Password,
    master_password: &HashedPassword,
) -> Result<EncryptedPassword, EncryptionError> {
    tracing::debug!("Encrypting password with master password");

    // Derive a 32-byte key from the master password using SHA-256
    let key = derive_key(master_password.as_str());
    let cipher = Aes256Gcm::new(&key.into());

    // Generate a random 12-byte nonce
    let mut nonce_bytes = [0u8; 12];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    // Encrypt the plaintext
    let plaintext_bytes = plaintext.as_str().as_bytes();
    let ciphertext = cipher.encrypt(nonce, plaintext_bytes).map_err(|e| {
        tracing::error!("Encryption failed: {}", e);
        EncryptionError::EncryptionFailed(e.to_string())
    })?;

    // Combine nonce and ciphertext
    let mut combined = Vec::with_capacity(nonce_bytes.len() + ciphertext.len());
    combined.extend_from_slice(&nonce_bytes);
    combined.extend_from_slice(&ciphertext);

    // Encode as base64 for storage
    let encoded = EncryptedPassword(general_purpose::STANDARD.encode(&combined));
    tracing::debug!("Password encrypted successfully");
    Ok(encoded)
}

/// Decrypts a password using the provided master password
///
/// # Arguments
/// * `encrypted_data` - Base64-encoded string containing nonce and encrypted data
/// * `master_password` - The hashed master password used as decryption key
///
/// # Returns
/// The decrypted password as a String
pub fn decrypt_password(
    encrypted_data: &EncryptedPassword,
    master_password: &HashedPassword,
) -> Result<Password, EncryptionError> {
    decrypt_password_with_key_material(encrypted_data, master_password.as_str())
}

/// Decrypts a password using arbitrary key material.
///
/// This is used for backward compatibility with legacy records that may have
/// been encrypted using raw password strings as key material.
pub fn decrypt_password_with_key_material(
    encrypted_data: &EncryptedPassword,
    key_material: &str,
) -> Result<Password, EncryptionError> {
    tracing::debug!("Decrypting password with master password");

    // Decode the base64 data
    let combined = general_purpose::STANDARD
        .decode(encrypted_data.as_str())
        .map_err(|e| {
            tracing::debug!("Base64 decoding failed: {}", e);
            EncryptionError::Base64DecodeFailed(e)
        })?;

    // Extract nonce (first 12 bytes) and ciphertext (remaining bytes)
    if combined.len() < 12 {
        tracing::debug!(
            "Invalid nonce size: expected at least 12 bytes, got {}",
            combined.len()
        );
        return Err(EncryptionError::InvalidNonceSize);
    }

    let (nonce_bytes, ciphertext) = combined.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);

    // Derive the same key from the provided key material
    let key = derive_key(key_material);
    let cipher = Aes256Gcm::new(&key.into());

    // Decrypt the ciphertext
    let plaintext_bytes = cipher.decrypt(nonce, ciphertext).map_err(|e| {
        tracing::debug!("Decryption failed: {}", e);
        EncryptionError::PasswordDecryptionFailed
    })?;

    // Convert to string
    let result = Password(String::from_utf8(plaintext_bytes).map_err(|e| {
        tracing::error!("Invalid UTF-8 in decrypted data: {}", e);
        EncryptionError::DecryptionFailed(format!("Invalid UTF-8: {}", e))
    })?);

    tracing::debug!("Password decrypted successfully");
    Ok(result)
}

/// Derives a 32-byte key from a password using SHA-256
fn derive_key(password: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    let result = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&result[..32]);
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt() {
        let password = Password("my_secret_password".into());
        let master_password = HashedPassword::from_password("master_key_123");

        let encrypted = encrypt_password(&password, &master_password).unwrap();
        let decrypted = decrypt_password(&encrypted, &master_password).unwrap();

        assert_eq!(password, decrypted);
    }

    #[test]
    fn test_decrypt_with_wrong_key() {
        let password = Password("my_secret_password".into());
        let master_password = HashedPassword::from_password("master_key_123");
        let wrong_password = HashedPassword::from_password("wrong_key_456");

        let encrypted = encrypt_password(&password, &master_password).unwrap();
        let result = decrypt_password(&encrypted, &wrong_password);

        assert!(result.is_err());
        matches!(
            result.unwrap_err(),
            EncryptionError::PasswordDecryptionFailed
        );
    }

    #[test]
    fn test_empty_password() {
        let password = Password("".into());
        let master_password = HashedPassword::from_password("master_key_123");

        let encrypted = encrypt_password(&password, &master_password).unwrap();
        let decrypted = decrypt_password(&encrypted, &master_password).unwrap();

        assert_eq!(password, decrypted);
    }

    #[test]
    fn test_special_characters() {
        let password = Password("p@ssw0rd!#$%^&*()_+-=[]{}|;':\",./<>?".into());
        let master_password = HashedPassword::from_password("m@st3r_k3y!@#$%^&*()");

        let encrypted = encrypt_password(&password, &master_password).unwrap();
        let decrypted = decrypt_password(&encrypted, &master_password).unwrap();

        assert_eq!(password, decrypted);
    }

    #[test]
    fn mapping_key_round_trips_and_is_purpose_and_secret_bound() {
        let key = MappingEncryptionKey::for_server_mappings(&[7u8; 64]).unwrap();
        let ct = key.encrypt(&Password::from("backend-secret")).unwrap();
        assert_eq!(key.decrypt(&ct).unwrap().as_str(), "backend-secret");
        // Different secret, different purpose, and the legacy scheme all fail.
        assert!(MappingEncryptionKey::for_server_mappings(&[8u8; 64])
            .unwrap()
            .decrypt(&ct)
            .is_err());
        assert!(MappingEncryptionKey::for_authorization_sessions(&[7u8; 64])
            .unwrap()
            .decrypt(&ct)
            .is_err());
        assert!(decrypt_password(&ct, &HashedPassword::from_password("x")).is_err());
        assert!(MappingEncryptionKey::for_server_mappings(&[0u8; 31]).is_err());
        // Random nonce: same plaintext, different ciphertext.
        assert_ne!(ct, key.encrypt(&Password::from("backend-secret")).unwrap());
    }

    #[test]
    fn tokens_are_prefixed_and_legacy_plaintext_passes_through() {
        let key = MappingEncryptionKey::for_authorization_sessions(&[7u8; 64]).unwrap();
        let stored = key
            .encrypt_token("0123456789abcdef0123456789abcdef")
            .unwrap();
        assert!(MappingEncryptionKey::is_encrypted_token(&stored));
        assert!(!stored.contains("0123456789abcdef"));
        assert_eq!(
            key.decrypt_token(&stored).unwrap(),
            "0123456789abcdef0123456789abcdef"
        );
        assert_eq!(key.decrypt_token("legacyhex").unwrap(), "legacyhex");
        let other = MappingEncryptionKey::for_authorization_sessions(&[9u8; 64]).unwrap();
        assert!(other.decrypt_token(&stored).is_err());
    }

    #[test]
    fn argon2_verifier_is_salted_argon2id_and_verifies() {
        let a = PasswordVerifier::hash(&"correct horse".into());
        let b = PasswordVerifier::hash(&"correct horse".into());
        assert!(a.as_str().starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        assert_ne!(a, b, "random salt");
        assert!(!a.is_legacy());
        assert!(a.verify("correct horse") && b.verify("correct horse"));
        assert!(!a.verify("correct horse ") && !a.verify(""));
        let empty = PasswordVerifier::hash(&"".into());
        assert!(empty.verify("") && !empty.verify(" "));
        let unicode = PasswordVerifier::hash(&"pässwörd🔑".into());
        assert!(unicode.verify("pässwörd🔑") && !unicode.verify("passwörd🔑"));
    }

    #[test]
    fn legacy_and_garbage_verifiers() {
        let legacy = PasswordVerifier::from_raw(hash_password("old"));
        assert!(legacy.is_legacy());
        assert!(legacy.verify("old") && !legacy.verify("Old"));
        for junk in ["", "hash", "$argon2id$garbage", "0"] {
            let v = PasswordVerifier::from_raw(junk.into());
            assert!(
                !v.verify("") && !v.verify("hash") && !v.verify("0"),
                "{junk}"
            );
        }
        // Debug never prints the hash.
        assert!(!format!("{legacy:?}").contains(&hash_password("old")));
    }

    #[test]
    fn legacy_plaintext_detection() {
        let raw = |s: &str| EncryptedPassword::from_raw(s.into());
        assert!(is_legacy_plaintext(&raw("hunter2")));
        assert!(is_legacy_plaintext(&raw("p@ss word!")));
        let ct = encrypt_password(&"x".into(), &HashedPassword::from_password("k")).unwrap();
        assert!(!is_legacy_plaintext(&ct));
        // 40 hex chars is valid base64 of 30 bytes: ambiguous, not "plaintext".
        assert!(!is_legacy_plaintext(&raw(
            "0123456789abcdef0123456789abcdef01234567"
        )));
    }
}
