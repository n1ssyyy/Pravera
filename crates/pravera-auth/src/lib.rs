//! Who may connect to this host, and what they may do once they have.
//!
//! Two independent gates guard a session, and both must pass:
//!
//! 1. **Device identity** — the ed25519 handshake proves which machine is
//!    calling. Handled in `pravera-crypto`.
//! 2. **User authentication** — a username and password, checked here, resolve
//!    to a [`Role`] and therefore to a [`Permission`] set.
//!
//! Being reachable is never authentication. A peer arriving over a direct cable
//! or a trusted tailnet still passes both gates, because Tailscale ACLs gate
//! reachability while Pravera roles gate capability.

pub mod permission;
pub mod user;

pub use permission::{Permission, Role};
pub use user::{permissions_for, validate_username, AuthOutcome, User};

/// Checks a login attempt against a stored credential.
///
/// Verification runs even when the user does not exist, using a dummy hash, so
/// the time taken cannot be used to enumerate valid usernames.
pub fn authenticate(
    username: &str,
    password: &str,
    stored: Option<&StoredCredential>,
    roles: &[Role],
) -> AuthOutcome {
    // A precomputed hash of a value no one will guess. Verifying against it
    // costs the same as a real check, so a missing account and a wrong password
    // take indistinguishable time.
    let Some(credential) = stored else {
        let _ = pravera_crypto::password::verify(password, DUMMY_HASH);
        return AuthOutcome::Denied;
    };

    if pravera_crypto::password::verify(password, &credential.password_hash).is_err() {
        return AuthOutcome::Denied;
    }

    if !credential.enabled {
        return AuthOutcome::Disabled;
    }

    let permissions = permissions_for(&credential.role, roles);
    let user = User {
        username: username.to_string(),
        role: credential.role.clone(),
        permissions,
        enabled: true,
    };

    if !user.can_connect() {
        return AuthOutcome::Denied;
    }

    AuthOutcome::Granted(user)
}

/// A stored Argon2id hash of a well-known throwaway string, used purely to keep
/// the timing of a failed lookup indistinguishable from a failed password.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHR2YWx1ZQ$\
7Nn3sYQD3sZ9d1hR1S1lFqZ3qFqZ3qFqZ3qFqZ3qFqY";

/// What the store keeps for one account.
///
/// Serialisable because the unattended store keeps these in a file: a machine
/// that boots with nobody at the keyboard has to already know who may log in.
/// The password is not in here and never has been — `password_hash` is an
/// Argon2id PHC string.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredCredential {
    pub password_hash: String,
    pub role: String,
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(role: &str, password: &str, enabled: bool) -> StoredCredential {
        StoredCredential {
            password_hash: pravera_crypto::password::hash(password).unwrap(),
            role: role.into(),
            enabled,
        }
    }

    #[test]
    fn a_correct_login_yields_the_permissions_of_its_role() {
        let stored = credential("operator", "s3cret", true);
        let outcome = authenticate("alice", "s3cret", Some(&stored), &Role::builtins());

        match outcome {
            AuthOutcome::Granted(user) => {
                assert_eq!(user.username, "alice");
                assert_eq!(user.permissions, Role::operator().permissions);
                assert!(user.permissions.allows(Permission::CONTROL));
                assert!(!user.permissions.allows(Permission::ADMIN));
            }
            other => panic!("expected a grant, got {other:?}"),
        }
    }

    #[test]
    fn a_wrong_password_is_denied() {
        let stored = credential("admin", "s3cret", true);
        assert_eq!(
            authenticate("alice", "wrong", Some(&stored), &Role::builtins()),
            AuthOutcome::Denied
        );
    }

    #[test]
    fn an_unknown_user_is_denied_without_revealing_that_it_is_unknown() {
        // Same variant as a wrong password, so the caller cannot tell them apart.
        assert_eq!(
            authenticate("nobody", "whatever", None, &Role::builtins()),
            AuthOutcome::Denied
        );
    }

    #[test]
    fn the_dummy_hash_costs_what_a_real_verification_costs() {
        // The whole timing defence rests on this constant being a *parseable*
        // Argon2 PHC string. If it were malformed, `verify` would bail on the
        // parse instead of running Argon2, an unknown username would be
        // refused in microseconds where a real one takes tens of milliseconds,
        // and the login prompt would be an account-enumeration oracle again.
        // Nothing else in the code would look wrong.
        //
        // AuthFailed means Argon2 ran and the digest did not match. Any other
        // error means it never ran.
        let outcome = pravera_crypto::password::verify("whatever", DUMMY_HASH);
        assert!(
            matches!(outcome, Err(pravera_core::Error::AuthFailed)),
            "DUMMY_HASH did not reach the Argon2 comparison: {outcome:?}"
        );
    }

    #[test]
    fn a_disabled_account_reports_disabled_only_after_the_password_matched() {
        let stored = credential("admin", "s3cret", false);
        assert_eq!(
            authenticate("alice", "s3cret", Some(&stored), &Role::builtins()),
            AuthOutcome::Disabled
        );
        // Without the right password it must still be an ordinary denial.
        assert_eq!(
            authenticate("alice", "wrong", Some(&stored), &Role::builtins()),
            AuthOutcome::Denied
        );
    }

    #[test]
    fn an_account_whose_role_no_longer_exists_is_denied() {
        let stored = credential("deleted-role", "s3cret", true);
        assert_eq!(
            authenticate("alice", "s3cret", Some(&stored), &Role::builtins()),
            AuthOutcome::Denied,
            "a dangling role must fail closed"
        );
    }

    #[test]
    fn a_viewer_is_granted_but_cannot_control() {
        let stored = credential("viewer", "look-only", true);
        let AuthOutcome::Granted(user) =
            authenticate("guest", "look-only", Some(&stored), &Role::builtins())
        else {
            panic!("viewer should be able to connect");
        };
        assert!(user.permissions.allows(Permission::VIEW));
        assert!(!user.permissions.allows(Permission::CONTROL));
    }
}
