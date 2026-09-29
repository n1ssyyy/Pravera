//! Pravera user accounts and the SQLite store that holds them.
//!
//! Accounts are local to each host and independent of OS accounts, so a machine
//! can grant someone view-only access without also giving them a Windows or
//! Linux login. The store lives in the service's machine-wide data directory
//! because the service must reach it before anyone has signed in.

use pravera_core::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::permission::{Permission, Role};

/// A user as the rest of the app sees it. The password hash never leaves this
/// module's storage layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub username: String,
    pub role: String,
    /// Resolved from the role at load time, so callers never have to join.
    pub permissions: Permission,
    /// A disabled account keeps its history and settings but cannot log in.
    pub enabled: bool,
}

impl User {
    /// Whether this account may currently start a session at all.
    pub fn can_connect(&self) -> bool {
        self.enabled && self.permissions.is_usable_session()
    }
}

/// Rules a username must satisfy.
///
/// Deliberately strict: usernames are typed at a login prompt, appear in audit
/// logs, and are compared for equality, so anything that could render
/// ambiguously is refused up front.
pub fn validate_username(name: &str) -> Result<()> {
    const MAX: usize = 32;

    if name.is_empty() {
        return Err(Error::Config("username cannot be empty".into()));
    }
    if name.len() > MAX {
        return Err(Error::Config(format!(
            "username cannot be longer than {MAX} characters"
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(Error::Config(
            "username may only contain lowercase letters, digits, hyphens and underscores".into(),
        ));
    }
    if name.starts_with('-') || name.starts_with('_') {
        return Err(Error::Config(
            "username must start with a letter or digit".into(),
        ));
    }
    Ok(())
}

/// The outcome of an authentication attempt.
///
/// Deliberately coarse on the failure side: a caller cannot distinguish "no
/// such user" from "wrong password", so the login prompt cannot be used to
/// enumerate accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthOutcome {
    Granted(User),
    Denied,
    /// The account exists and the password matched, but it has been disabled.
    /// Distinct from `Denied` because the honest message is more useful than a
    /// misleading one, and it reveals nothing to someone without the password.
    Disabled,
}

/// Resolves the effective permissions for a role name.
///
/// An unknown role grants nothing rather than defaulting to something
/// permissive, so a typo in the database cannot silently widen access.
pub fn permissions_for(role_name: &str, roles: &[Role]) -> Permission {
    roles
        .iter()
        .find(|r| r.name == role_name)
        .map(|r| r.permissions)
        .unwrap_or(Permission::empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_usernames_are_accepted() {
        for name in ["alice", "bob-2", "svc_backup", "a", "user123"] {
            assert!(validate_username(name).is_ok(), "rejected: {name}");
        }
    }

    #[test]
    fn ambiguous_or_oversized_usernames_are_refused() {
        for name in [
            "",
            "Alice",
            "has space",
            "café",
            "日本",
            "-leading",
            "_leading",
            "user@host",
        ] {
            assert!(validate_username(name).is_err(), "accepted: {name:?}");
        }
        assert!(validate_username(&"a".repeat(33)).is_err());
        assert!(validate_username(&"a".repeat(32)).is_ok());
    }

    #[test]
    fn an_unknown_role_grants_nothing() {
        let roles = Role::builtins();
        assert_eq!(permissions_for("typo", &roles), Permission::empty());
        assert_eq!(permissions_for("viewer", &roles), Permission::VIEW);
    }

    #[test]
    fn a_disabled_account_cannot_connect_even_with_full_permissions() {
        let user = User {
            username: "alice".into(),
            role: "admin".into(),
            permissions: Permission::all(),
            enabled: false,
        };
        assert!(!user.can_connect());
    }

    #[test]
    fn an_enabled_account_without_view_cannot_connect() {
        let user = User {
            username: "bob".into(),
            role: "broken".into(),
            permissions: Permission::CONTROL,
            enabled: true,
        };
        assert!(
            !user.can_connect(),
            "a session that shows nothing is not a session"
        );
    }

    #[test]
    fn an_ordinary_enabled_account_can_connect() {
        let user = User {
            username: "carol".into(),
            role: "operator".into(),
            permissions: Role::operator().permissions,
            enabled: true,
        };
        assert!(user.can_connect());
    }
}
