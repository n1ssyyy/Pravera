//! Where the host looks up an account.
//!
//! A trait rather than a concrete type, because the session state machine has
//! no business knowing whether accounts live in SQLite, and because a test that
//! needs one user should not need a database file.

use std::collections::HashMap;

use pravera_auth::{validate_username, Role, StoredCredential};
use pravera_core::{Error, Result};

/// The host's account directory.
///
/// Implementations are consulted during authentication, which happens on the
/// connection task, so they must be safe to share.
pub trait UserStore: Send + Sync {
    /// The stored credential for a username, or `None` if there is no such
    /// account.
    ///
    /// Returning `None` must not be faster than returning `Some`. The caller
    /// defends the timing by verifying against a dummy hash either way, but a
    /// store whose miss path is dramatically cheaper (an index probe versus a
    /// row read) narrows that defence.
    fn credential(&self, username: &str) -> Option<StoredCredential>;

    /// Every role this host knows. An account naming a role that is not in
    /// here gets no permissions at all.
    fn roles(&self) -> Vec<Role>;
}

/// An in-memory store, for development and tests.
///
/// **Not the production store.** Nothing here survives a restart, and the
/// SQLite-backed store that does arrives with the rest of user management in
/// P3. It exists so the session state machine can be exercised without a
/// database, which is the only way its permission logic gets tested properly.
#[derive(Debug, Clone)]
pub struct MemoryStore {
    users: HashMap<String, StoredCredential>,
    roles: Vec<Role>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    /// An empty store with the three built-in roles.
    pub fn new() -> Self {
        MemoryStore {
            users: HashMap::new(),
            roles: Role::builtins(),
        }
    }

    pub fn with_roles(roles: Vec<Role>) -> Self {
        MemoryStore {
            users: HashMap::new(),
            roles,
        }
    }

    /// Add an enabled account, hashing the password with Argon2id.
    ///
    /// Refuses an account whose role this store does not know, because such an
    /// account can never log in: an unknown role resolves to no permissions,
    /// and a session with no permissions is refused. Failing at creation time
    /// turns a confusing login failure into an obvious typo.
    pub fn add(&mut self, username: &str, password: &str, role: &str) -> Result<()> {
        validate_username(username)?;
        if password.is_empty() {
            return Err(Error::Config("password cannot be empty".into()));
        }
        if !self.roles.iter().any(|known| known.name == role) {
            return Err(Error::Config(format!("no such role: {role}")));
        }

        self.users.insert(
            username.to_string(),
            StoredCredential {
                password_hash: pravera_crypto::password::hash(password)?,
                role: role.to_string(),
                enabled: true,
            },
        );
        Ok(())
    }

    /// Turn an account off without deleting it. Returns whether it existed.
    pub fn set_enabled(&mut self, username: &str, enabled: bool) -> bool {
        match self.users.get_mut(username) {
            Some(credential) => {
                credential.enabled = enabled;
                true
            }
            None => false,
        }
    }

    pub fn remove(&mut self, username: &str) -> bool {
        self.users.remove(username).is_some()
    }

    pub fn len(&self) -> usize {
        self.users.len()
    }

    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }
}

/// A store shared between concurrent sessions.
///
/// A host serves several peers at once and they all consult the same accounts,
/// so the shared case is the normal one rather than the exception. Lookups are
/// read-only, which is what makes sharing safe without a lock.
impl<S: UserStore> UserStore for std::sync::Arc<S> {
    fn credential(&self, username: &str) -> Option<StoredCredential> {
        (**self).credential(username)
    }

    fn roles(&self) -> Vec<Role> {
        (**self).roles()
    }
}

impl UserStore for MemoryStore {
    fn credential(&self, username: &str) -> Option<StoredCredential> {
        self.users.get(username).cloned()
    }

    fn roles(&self) -> Vec<Role> {
        self.roles.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_added_account_can_be_looked_up_and_its_password_is_hashed() {
        let mut store = MemoryStore::new();
        store.add("alice", "s3cret", "operator").unwrap();

        let credential = store.credential("alice").expect("alice should exist");
        assert_eq!(credential.role, "operator");
        assert!(credential.enabled);
        assert!(
            !credential.password_hash.contains("s3cret"),
            "the password was stored in the clear"
        );
        assert!(credential.password_hash.starts_with("$argon2id$"));
    }

    #[test]
    fn an_unknown_account_is_simply_absent() {
        let store = MemoryStore::new();
        assert!(store.credential("nobody").is_none());
    }

    #[test]
    fn an_account_naming_a_role_that_does_not_exist_is_refused_at_creation() {
        // Such an account resolves to no permissions and could never connect.
        // Better a clear error now than an unexplained login failure later.
        let mut store = MemoryStore::new();
        assert!(store.add("alice", "s3cret", "superuser").is_err());
        assert!(store.is_empty());
    }

    #[test]
    fn an_invalid_username_is_refused_at_creation() {
        let mut store = MemoryStore::new();
        for bad in ["", "Alice", "alice bob", "-alice", &"a".repeat(64)] {
            assert!(
                store.add(bad, "s3cret", "viewer").is_err(),
                "accepted {bad:?}"
            );
        }
        assert!(store.is_empty());
    }

    #[test]
    fn an_empty_password_is_refused() {
        let mut store = MemoryStore::new();
        assert!(store.add("alice", "", "viewer").is_err());
    }

    #[test]
    fn disabling_keeps_the_account_but_marks_it_unusable() {
        let mut store = MemoryStore::new();
        store.add("alice", "s3cret", "admin").unwrap();
        assert!(store.set_enabled("alice", false));

        let credential = store.credential("alice").unwrap();
        assert!(!credential.enabled);
        assert_eq!(store.len(), 1, "the account should still be there");
        assert!(!store.set_enabled("nobody", false));
    }

    #[test]
    fn the_builtin_roles_are_available_by_default() {
        let names: Vec<String> = MemoryStore::new()
            .roles()
            .into_iter()
            .map(|role| role.name)
            .collect();
        assert!(names.contains(&"viewer".to_string()));
        assert!(names.contains(&"operator".to_string()));
        assert!(names.contains(&"admin".to_string()));
    }
}
