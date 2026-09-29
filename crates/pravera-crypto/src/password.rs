//! Password hashing for Pravera user accounts.
//!
//! Argon2id with the parameters recommended by the OWASP password storage
//! guidance. Verification is deliberately slow, which is the point: it is the
//! only thing standing between a stolen `users.db` and every password in it.

use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::{Algorithm, Argon2, Params, Version};
use pravera_core::{Error, Result};

/// 19 MiB of memory, 2 passes, 1 lane. Chosen to stay comfortable on a laptop
/// while making large-scale offline cracking expensive.
const MEMORY_KIB: u32 = 19 * 1024;
const ITERATIONS: u32 = 2;
const PARALLELISM: u32 = 1;

fn argon2() -> Result<Argon2<'static>> {
    let params = Params::new(MEMORY_KIB, ITERATIONS, PARALLELISM, None)
        .map_err(|e| Error::other(format!("invalid Argon2 parameters: {e}")))?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// Hashes a password, returning a PHC string that carries its own salt and
/// parameters, so the parameters above can be raised later without invalidating
/// existing hashes.
pub fn hash(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = argon2()?
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| Error::other(format!("could not hash password: {e}")))?;
    Ok(hash.to_string())
}

/// Checks a password against a stored PHC hash.
///
/// Returns `Ok(())` only on a match. A malformed stored hash is an error rather
/// than a silent failure, because it means the database is corrupt and quietly
/// rejecting every login would be the wrong story to tell the user.
pub fn verify(password: &str, stored: &str) -> Result<()> {
    let parsed = PasswordHash::new(stored)
        .map_err(|e| Error::other(format!("stored password hash is malformed: {e}")))?;

    argon2()?
        .verify_password(password.as_bytes(), &parsed)
        .map_err(|_| Error::AuthFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_right_password_verifies() {
        let stored = hash("correct horse battery staple").unwrap();
        assert!(verify("correct horse battery staple", &stored).is_ok());
    }

    #[test]
    fn the_wrong_password_is_rejected() {
        let stored = hash("correct horse battery staple").unwrap();
        assert!(matches!(verify("hunter2", &stored), Err(Error::AuthFailed)));
    }

    #[test]
    fn hashing_is_salted_so_equal_passwords_differ_on_disk() {
        let a = hash("same password").unwrap();
        let b = hash("same password").unwrap();
        assert_ne!(
            a, b,
            "a shared salt would let one crack break every matching account"
        );
        assert!(verify("same password", &a).is_ok());
        assert!(verify("same password", &b).is_ok());
    }

    #[test]
    fn the_stored_hash_never_contains_the_password() {
        let stored = hash("plaintext-should-not-appear").unwrap();
        assert!(!stored.contains("plaintext-should-not-appear"));
    }

    #[test]
    fn the_hash_records_argon2id_so_parameters_can_be_raised_later() {
        let stored = hash("whatever").unwrap();
        assert!(stored.starts_with("$argon2id$"), "got: {stored}");
    }

    #[test]
    fn a_corrupt_stored_hash_reports_corruption_not_a_failed_login() {
        let err = verify("whatever", "not a PHC string").unwrap_err();
        assert!(
            !matches!(err, Error::AuthFailed),
            "a corrupt database must not masquerade as a wrong password"
        );
    }

    #[test]
    fn an_empty_password_still_hashes_and_verifies() {
        // Policy about weak passwords belongs in the auth layer, not here.
        let stored = hash("").unwrap();
        assert!(verify("", &stored).is_ok());
        assert!(verify("x", &stored).is_err());
    }
}
