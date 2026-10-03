//! Password hashing and opaque session tokens shared by the CLI and web server.

use argon2::password_hash::generate_salt;
use argon2::password_hash::phc::PasswordHash;
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult, ErrorCategory};

pub const MAX_USERNAME_BYTES: usize = 128;
pub const MAX_PASSWORD_BYTES: usize = 1024;

pub fn validate_credentials(username: &str, password: &str) -> AppResult<()> {
    if username.trim().is_empty()
        || username.len() > MAX_USERNAME_BYTES
        || username.chars().any(char::is_control)
    {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "user_credentials",
            "user name must be nonblank, at most 128 bytes, and contain no control characters",
        ));
    }
    if password.is_empty() || password.len() > MAX_PASSWORD_BYTES {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "user_credentials",
            "password must contain between 1 and 1024 bytes",
        ));
    }
    Ok(())
}

/// Argon2id v19, 19 MiB, two iterations, one lane, with a fresh random salt.
/// The PHC string includes the algorithm, parameters, salt, and hash.
pub fn hash_password(password: &str) -> AppResult<String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|_| {
            AppError::new(
                ErrorCategory::Configuration,
                "hash_password",
                "password hashing failed",
            )
        })
}

pub fn verify_password(password: &str, encoded: &str) -> bool {
    let Ok(hash) = PasswordHash::new(encoded) else {
        return false;
    };
    // Reject plaintext and other algorithms inserted out of band.
    hash.algorithm.as_str() == "argon2id"
        && Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
}

/// A 256-bit bearer token. Only its SHA-256 digest is persisted in the database.
pub fn new_session_token() -> String {
    let mut bytes = [0u8; 32];
    // Each salt supplies 16 independent bytes from the operating system's RNG.
    bytes[..16].copy_from_slice(&generate_salt());
    bytes[16..].copy_from_slice(&generate_salt());
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn fingerprint(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(value.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_existing_argon2_05_password_hashes() {
        // A fixed Argon2 0.5 test vector verifies compatibility with stored PHC strings.
        let hash = "$argon2id$v=19$m=65536,t=2,p=1$c29tZXNhbHQ$CTFhFdXPJO1aFaMaO6Mm5c8y7cJHAph8ArZWb2GRPPc";
        assert!(verify_password("password", hash));
        assert!(!verify_password("incorrect", hash));
    }

    #[test]
    fn session_tokens_contain_256_random_bits() {
        let first = new_session_token();
        let second = new_session_token();
        assert_ne!(first, second);
        assert_eq!(URL_SAFE_NO_PAD.decode(first).unwrap().len(), 32);
        assert_eq!(URL_SAFE_NO_PAD.decode(second).unwrap().len(), 32);
    }

    #[test]
    fn passwords_are_salted_and_verified_without_plaintext_storage() {
        let first = hash_password("correct horse battery staple").unwrap();
        let second = hash_password("correct horse battery staple").unwrap();
        assert_ne!(first, second);
        assert!(first.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        assert!(!first.contains("correct horse"));
        assert!(verify_password("correct horse battery staple", &first));
        assert!(!verify_password("incorrect", &first));
        assert!(!verify_password("plaintext", "plaintext"));
        assert!(!verify_password("incorrect", "$argon2id$broken"));
    }
}
