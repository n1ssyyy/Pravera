//! Accounts that survive a restart.
//!
//! [`MemoryStore`](crate::MemoryStore) is set up by hand each time the process
//! starts, which is fine for a machine somebody is sitting at and useless for
//! one that boots with nobody there. A homelab box has no keyboard to type a
//! password into, so the account has to be on disk before it is needed.
//!
//! ## What is on disk, and what is not
//!
//! The password is not. What is stored is the Argon2id PHC string — the same
//! thing [`MemoryStore`](crate::MemoryStore) holds in memory and the same thing
//! the SQLite store will hold in P3 — plus the username, the role, and whether
//! the account is enabled. Reading this file tells an attacker who may log in
//! and lets them mount an offline attack against the hash. It does not tell
//! them the password, and it is not a credential they can replay.
//!
//! That is still worth protecting, so the file is written `0600` on Unix. On
//! Windows it inherits the ACL of the per-user application data directory,
//! which is already restricted to the account that owns it. Neither is a
//! defence against someone who is already administrator on the machine.
//!
//! ## Why JSON and not the SQLite the plan names
//!
//! The database arrives with the rest of user management, and everything that
//! makes a database worth having — queries, migrations, concurrent writers —
//! is worth nothing to a file holding one or two accounts. A person can read
//! this one, and delete it to lock everybody out, which on a machine with no
//! monitor is a genuinely useful property.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pravera_auth::{validate_username, Permission, Role, StoredCredential};
use pravera_core::{Error, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::store::UserStore;

/// The on-disk shape. Named fields with defaults so a file written by an older
/// build still loads: an unknown field is ignored and a missing one takes its
/// default, rather than the whole file failing to parse and locking everyone
/// out of a machine nobody can walk up to.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Document {
    #[serde(default)]
    users: BTreeMap<String, StoredCredential>,
    /// Roles this machine was told about, beyond the three built in.
    ///
    /// Stored by name rather than merged into the built-in list, so a future
    /// change to what `operator` means reaches every machine instead of being
    /// frozen into whatever was written the first time somebody opened the
    /// screen.
    #[serde(default)]
    roles: BTreeMap<String, Permission>,
}

/// One account, as a screen needs to show it.
///
/// Owned strings rather than borrows: this crosses into the interface, is held
/// across frames, and is a handful of accounts rather than a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub username: String,
    pub role: String,
    pub enabled: bool,
    /// What the role resolves to right now. An account naming a role this
    /// machine no longer knows resolves to nothing, which is what it would
    /// actually get at the login prompt — so that is what gets shown.
    pub permissions: Permission,
}

impl Account {
    /// Whether this account could start a session as things stand.
    ///
    /// Disabled accounts and accounts whose role grants no view both answer
    /// `false`, and both are worth flagging: the first is deliberate, the
    /// second is almost always a mistake.
    pub fn can_connect(&self) -> bool {
        self.enabled && self.permissions.is_usable_session()
    }
}

/// Accounts kept in a file.
#[derive(Debug, Clone)]
pub struct FileStore {
    path: PathBuf,
    users: BTreeMap<String, StoredCredential>,
    roles: Vec<Role>,
}

impl FileStore {
    /// Read the accounts at `path`, or start empty if there are none yet.
    ///
    /// A file that cannot be parsed is a problem worth reporting rather than
    /// silently replacing: overwriting it would destroy the only record of who
    /// may reach this machine.
    pub fn load(path: impl Into<PathBuf>) -> Result<FileStore> {
        let path = path.into();

        let document = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<Document>(&text).map_err(|error| {
                Error::Config(format!(
                    "the account file at {} could not be read: {error}",
                    path.display()
                ))
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                debug!(path = %path.display(), "no accounts yet");
                Document::default()
            }
            Err(error) => {
                return Err(Error::Config(format!(
                    "the account file at {} could not be opened: {error}",
                    path.display()
                )))
            }
        };

        let mut store = FileStore {
            path,
            users: document.users,
            roles: Role::builtins(),
        };
        for (name, permissions) in document.roles {
            store.remember_role(name, permissions);
        }
        Ok(store)
    }

    /// Put a custom role into the in-memory list without writing anything.
    ///
    /// A file naming a role that is also built in does not get to redefine it:
    /// `admin` means everything on every machine, and a file that could quietly
    /// narrow it would turn "you are an administrator here" into a claim that
    /// depends on a JSON file nobody reads.
    fn remember_role(&mut self, name: String, permissions: Permission) {
        if Role::builtins().iter().any(|builtin| builtin.name == name) {
            warn!(
                role = %name,
                "ignoring a stored role that shares a name with a built-in one"
            );
            return;
        }
        self.roles.push(Role::custom(name, permissions));
    }

    /// Where these accounts are kept.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }

    pub fn len(&self) -> usize {
        self.users.len()
    }

    /// The usernames, in a stable order.
    pub fn usernames(&self) -> Vec<&str> {
        self.users.keys().map(String::as_str).collect()
    }

    /// The role an account holds, for showing back what was set up.
    pub fn role_of(&self, username: &str) -> Option<&str> {
        self.users.get(username).map(|user| user.role.as_str())
    }

    /// Add or replace an account, hashing the password with Argon2id, and
    /// write the file.
    ///
    /// Refuses a role this store does not know, because such an account can
    /// never log in — an unknown role resolves to no permissions and a session
    /// with none is refused. Failing here turns a login that mysteriously does
    /// not work into an obvious typo, which matters more than usual when the
    /// machine it is on has no screen.
    pub fn set(&mut self, username: &str, password: &str, role: &str) -> Result<()> {
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
        self.save()
    }

    /// Remove an account and write the file. `false` if there was no such
    /// account, which is not an error.
    pub fn remove(&mut self, username: &str) -> Result<bool> {
        let removed = self.users.remove(username).is_some();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Every account, in a stable order, with its role resolved.
    pub fn accounts(&self) -> Vec<Account> {
        self.users
            .iter()
            .map(|(username, stored)| self.describe(username, stored))
            .collect()
    }

    /// One account, or `None` if there is no such name.
    pub fn account(&self, username: &str) -> Option<Account> {
        self.users
            .get(username)
            .map(|stored| self.describe(username, stored))
    }

    fn describe(&self, username: &str, stored: &StoredCredential) -> Account {
        Account {
            username: username.to_string(),
            role: stored.role.clone(),
            enabled: stored.enabled,
            permissions: pravera_auth::permissions_for(&stored.role, &self.roles),
        }
    }

    /// Turn an account on or off and write the file.
    ///
    /// Disabling is the reversible half of removing: the account keeps its
    /// password and its role and simply stops being accepted. On a machine
    /// nobody can walk up to, that difference matters — re-enabling is a click,
    /// while recreating a removed account means knowing what its password was.
    pub fn set_enabled(&mut self, username: &str, enabled: bool) -> Result<bool> {
        let Some(stored) = self.users.get_mut(username) else {
            return Ok(false);
        };
        if stored.enabled == enabled {
            return Ok(true);
        }
        stored.enabled = enabled;
        self.save()?;
        Ok(true)
    }

    /// Move an account to a different role and write the file.
    ///
    /// Refuses a role this machine does not know, for the same reason [`set`]
    /// does: the account would still exist, still take its password, and grant
    /// nothing.
    pub fn set_role(&mut self, username: &str, role: &str) -> Result<bool> {
        if !self.roles.iter().any(|known| known.name == role) {
            return Err(Error::Config(format!("no such role: {role}")));
        }
        let Some(stored) = self.users.get_mut(username) else {
            return Ok(false);
        };
        if stored.role == role {
            return Ok(true);
        }
        stored.role = role.to_string();
        self.save()?;
        Ok(true)
    }

    /// Replace an account's password, keeping its role and its enabled state.
    ///
    /// Separate from [`set`] because changing a password should not require
    /// re-stating a role, and re-stating one is how a role gets changed by
    /// accident.
    pub fn set_password(&mut self, username: &str, password: &str) -> Result<bool> {
        if password.is_empty() {
            return Err(Error::Config("password cannot be empty".into()));
        }
        let hash = pravera_crypto::password::hash(password)?;
        let Some(stored) = self.users.get_mut(username) else {
            return Ok(false);
        };
        stored.password_hash = hash;
        self.save()?;
        Ok(true)
    }

    /// Every role this machine knows, built-in ones first.
    ///
    /// Not called `roles`: [`UserStore`] already has a method by that name
    /// returning an owned list, and an inherent method that quietly shadowed it
    /// would make `store.roles()` mean two different things depending on
    /// whether the trait happened to be in scope.
    pub fn known_roles(&self) -> &[Role] {
        &self.roles
    }

    /// Define or redefine a custom role and write the file.
    ///
    /// A role with no [`Permission::VIEW`] is refused. It would be accepted
    /// everywhere, grant a session, and then show nothing — a failure that
    /// looks like a broken connection rather than a settings mistake, which is
    /// the worst way for it to present.
    pub fn define_role(&mut self, name: &str, permissions: Permission) -> Result<()> {
        let name = name.trim();
        validate_role_name(name)?;
        if Role::builtins().iter().any(|builtin| builtin.name == name) {
            return Err(Error::Config(format!(
                "{name} is a built-in role and cannot be redefined"
            )));
        }
        if !permissions.is_usable_session() {
            return Err(Error::Config(
                "a role has to be able to see the screen to be worth having".into(),
            ));
        }

        match self.roles.iter_mut().find(|role| role.name == name) {
            Some(existing) => existing.permissions = permissions,
            None => self.roles.push(Role::custom(name, permissions)),
        }
        self.save()
    }

    /// Remove a custom role and write the file.
    ///
    /// Refused while an account still holds it. Removing it anyway would leave
    /// that account naming a role nothing knows, which resolves to no
    /// permissions — the account would keep taking its password and keep being
    /// refused, with nothing on screen explaining why.
    pub fn remove_role(&mut self, name: &str) -> Result<bool> {
        if Role::builtins().iter().any(|builtin| builtin.name == name) {
            return Err(Error::Config(format!("{name} is a built-in role")));
        }
        let held_by: Vec<&str> = self
            .users
            .iter()
            .filter(|(_, stored)| stored.role == name)
            .map(|(username, _)| username.as_str())
            .collect();
        if !held_by.is_empty() {
            return Err(Error::Config(format!(
                "{name} is still held by {}",
                held_by.join(", ")
            )));
        }

        let before = self.roles.len();
        self.roles.retain(|role| role.name != name);
        if self.roles.len() == before {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }

    /// Write the accounts out.
    ///
    /// Through a temporary file and a rename, so a crash or a full disk during
    /// the write leaves the previous accounts intact rather than a half-written
    /// file that parses as nothing. Losing the last write is recoverable;
    /// losing every account on a machine with no monitor is not.
    fn save(&self) -> Result<()> {
        let document = Document {
            users: self.users.clone(),
            // Built-ins are not written. They are code, and writing them would
            // freeze today's definition of `operator` into every machine.
            roles: self
                .roles
                .iter()
                .filter(|role| !role.builtin)
                .map(|role| (role.name.clone(), role.permissions))
                .collect(),
        };
        let text = serde_json::to_string_pretty(&document).map_err(|error| {
            Error::Config(format!("the accounts could not be written: {error}"))
        })?;

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                Error::Config(format!(
                    "{} could not be created: {error}",
                    parent.display()
                ))
            })?;
        }

        let temporary = self.path.with_extension("json.new");
        write_private(&temporary, &text)?;

        std::fs::rename(&temporary, &self.path).map_err(|error| {
            // Leave the temporary file: it holds the accounts that were meant
            // to be saved, and deleting it would destroy the only copy.
            warn!(
                from = %temporary.display(),
                to = %self.path.display(),
                %error,
                "the account file could not be replaced"
            );
            Error::Config(format!("the accounts could not be saved: {error}"))
        })
    }
}

/// Rules a custom role name must satisfy.
///
/// Looser than a username — a role is chosen once and read, not typed at a
/// prompt — but still bounded, and still refusing anything that would render
/// ambiguously next to the built-in names.
fn validate_role_name(name: &str) -> Result<()> {
    const MAX: usize = 32;

    if name.is_empty() {
        return Err(Error::Config("a role needs a name".into()));
    }
    if name.chars().count() > MAX {
        return Err(Error::Config(format!(
            "a role name cannot be longer than {MAX} characters"
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ' ')
    {
        return Err(Error::Config(
            "a role name may only contain letters, digits, spaces, hyphens and underscores".into(),
        ));
    }
    if name.starts_with(' ') || name.ends_with(' ') {
        return Err(Error::Config(
            "a role name cannot start or end with a space".into(),
        ));
    }
    Ok(())
}

/// Write a file only its owner can read.
fn write_private(path: &Path, text: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        // The mode is set as the file is created, not afterwards: a file that
        // exists world-readable for even a moment has been world-readable.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|error| {
                Error::Config(format!("{} could not be written: {error}", path.display()))
            })?;
        file.write_all(text.as_bytes())
            .map_err(|error| Error::Config(format!("the accounts could not be written: {error}")))
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, text).map_err(|error| {
            Error::Config(format!("{} could not be written: {error}", path.display()))
        })
    }
}

impl UserStore for FileStore {
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
    use pravera_auth::{authenticate, AuthOutcome};

    /// A temporary path that does not collide between tests.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("pravera-accounts-tests");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{name}.json"));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn granted(store: &FileStore, username: &str, password: &str) -> bool {
        let stored = store.credential(username);
        matches!(
            authenticate(username, password, stored.as_ref(), &store.roles()),
            AuthOutcome::Granted(_)
        )
    }

    #[test]
    fn an_account_set_up_once_is_there_after_a_restart() {
        // The whole point: a machine with no keyboard cannot be told a
        // password at boot.
        let path = scratch("survives");

        let mut store = FileStore::load(&path).unwrap();
        store.set("operator", "hunter2", "operator").unwrap();

        let reopened = FileStore::load(&path).unwrap();
        assert!(granted(&reopened, "operator", "hunter2"));
        assert!(!granted(&reopened, "operator", "hunter3"));
    }

    #[test]
    fn the_password_itself_is_never_written_down() {
        let path = scratch("no-plaintext");
        let mut store = FileStore::load(&path).unwrap();
        store.set("operator", "correct-horse", "operator").unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            !written.contains("correct-horse"),
            "the password is in the file: {written}"
        );
        assert!(written.contains("$argon2id$"), "{written}");
    }

    #[test]
    fn a_missing_file_is_an_empty_store_rather_than_a_failure() {
        // First run on a fresh machine. Refusing to start because nobody has
        // created an account yet would be circular.
        let store = FileStore::load(scratch("absent")).unwrap();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn a_file_that_cannot_be_parsed_is_reported_rather_than_replaced() {
        // Overwriting it would destroy the only record of who may reach a
        // machine nobody can walk up to.
        let path = scratch("corrupt");
        std::fs::write(&path, "{ this is not json").unwrap();

        let error = FileStore::load(&path).expect_err("a broken file");
        assert!(error.to_string().contains("could not be read"), "{error}");
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("not json"),
            "the broken file was overwritten"
        );
    }

    #[test]
    fn an_unknown_role_is_refused_at_the_point_it_is_typed() {
        let mut store = FileStore::load(scratch("bad-role")).unwrap();
        assert!(store.set("operator", "hunter2", "wizard").is_err());
        assert!(store.is_empty());
    }

    #[test]
    fn an_account_with_no_password_is_refused() {
        // The connect code is public by design, so an account with no password
        // is an open machine.
        let mut store = FileStore::load(scratch("no-password")).unwrap();
        assert!(store.set("operator", "", "operator").is_err());
    }

    #[test]
    fn setting_the_same_username_again_replaces_the_password() {
        let path = scratch("replace");
        let mut store = FileStore::load(&path).unwrap();

        store.set("operator", "first", "operator").unwrap();
        store.set("operator", "second", "operator").unwrap();

        let reopened = FileStore::load(&path).unwrap();
        assert_eq!(reopened.len(), 1);
        assert!(granted(&reopened, "operator", "second"));
        assert!(!granted(&reopened, "operator", "first"));
    }

    #[test]
    fn removing_the_last_account_leaves_nobody_able_to_log_in() {
        let path = scratch("remove");
        let mut store = FileStore::load(&path).unwrap();
        store.set("operator", "hunter2", "operator").unwrap();

        assert!(store.remove("operator").unwrap());
        assert!(!store.remove("operator").unwrap(), "removed twice");

        let reopened = FileStore::load(&path).unwrap();
        assert!(reopened.is_empty());
        assert!(!granted(&reopened, "operator", "hunter2"));
    }

    #[test]
    fn a_role_the_store_does_not_know_grants_nothing() {
        // Fails closed. An account naming a role that has since been removed
        // must not become a free pass.
        let path = scratch("stale-role");
        let mut store = FileStore::load(&path).unwrap();
        store.set("operator", "hunter2", "operator").unwrap();

        let mut narrowed = FileStore::load(&path).unwrap();
        narrowed.roles = Vec::new();
        assert!(!granted(&narrowed, "operator", "hunter2"));
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;

        let path = scratch("permissions");
        let mut store = FileStore::load(&path).unwrap();
        store.set("operator", "hunter2", "operator").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "the accounts are readable by others");
    }

    // ------------------------------------------------------- managing users

    #[test]
    fn a_disabled_account_keeps_its_password_and_stops_being_accepted() {
        // The difference from removing: on a machine nobody can walk up to,
        // re-enabling is a click and recreating means knowing the password.
        let path = scratch("disable");

        let mut store = FileStore::load(&path).unwrap();
        store.set("ops", "hunter2", "operator").unwrap();
        assert!(granted(&store, "ops", "hunter2"));

        assert!(store.set_enabled("ops", false).unwrap());
        assert!(!granted(&store, "ops", "hunter2"));

        // And it is still off after a restart, which is the part that matters.
        let mut store = FileStore::load(&path).unwrap();
        assert!(!granted(&store, "ops", "hunter2"));
        assert!(store.set_enabled("ops", true).unwrap());
        assert!(granted(&store, "ops", "hunter2"));
    }

    #[test]
    fn turning_off_an_account_that_is_not_there_is_not_an_error() {
        let mut store = FileStore::load(scratch("disable-absent")).unwrap();
        assert!(!store.set_enabled("nobody", false).unwrap());
    }

    #[test]
    fn a_new_password_does_not_disturb_the_role_or_the_enabled_flag() {
        let path = scratch("repassword");

        let mut store = FileStore::load(&path).unwrap();
        store.set("ops", "old", "viewer").unwrap();
        store.set_enabled("ops", false).unwrap();
        store.set_password("ops", "new").unwrap();

        let account = store.account("ops").unwrap();
        assert_eq!(account.role, "viewer");
        assert!(!account.enabled);

        // Enabled again, the new password works and the old one does not.
        store.set_enabled("ops", true).unwrap();
        assert!(granted(&store, "ops", "new"));
        assert!(!granted(&store, "ops", "old"));
    }

    #[test]
    fn an_empty_password_is_refused_here_too() {
        let mut store = FileStore::load(scratch("empty-repassword")).unwrap();
        store.set("ops", "real", "operator").unwrap();
        assert!(store.set_password("ops", "").is_err());
        // And the account it refused to change still works.
        assert!(granted(&store, "ops", "real"));
    }

    #[test]
    fn moving_an_account_to_another_role_changes_what_it_may_do() {
        let path = scratch("rerole");

        let mut store = FileStore::load(&path).unwrap();
        store.set("ops", "hunter2", "viewer").unwrap();
        assert!(!store
            .account("ops")
            .unwrap()
            .permissions
            .allows(Permission::CONTROL));

        store.set_role("ops", "operator").unwrap();
        assert!(store
            .account("ops")
            .unwrap()
            .permissions
            .allows(Permission::CONTROL));

        let store = FileStore::load(&path).unwrap();
        assert_eq!(store.account("ops").unwrap().role, "operator");
    }

    #[test]
    fn an_account_cannot_be_moved_to_a_role_that_does_not_exist() {
        // It would still exist, still take its password, and grant nothing.
        let mut store = FileStore::load(scratch("rerole-unknown")).unwrap();
        store.set("ops", "hunter2", "operator").unwrap();
        assert!(store.set_role("ops", "wizard").is_err());
        assert_eq!(store.account("ops").unwrap().role, "operator");
    }

    // ------------------------------------------------------- custom roles

    #[test]
    fn a_custom_role_survives_a_restart_and_can_be_held() {
        let path = scratch("custom-role");

        let mut store = FileStore::load(&path).unwrap();
        store
            .define_role("backup", Permission::VIEW | Permission::FILE_READ)
            .unwrap();
        store.set("archiver", "hunter2", "backup").unwrap();

        let store = FileStore::load(&path).unwrap();
        let account = store.account("archiver").unwrap();
        assert_eq!(account.role, "backup");
        assert!(account.permissions.allows(Permission::FILE_READ));
        assert!(!account.permissions.allows(Permission::CONTROL));
        assert!(granted(&store, "archiver", "hunter2"));
    }

    #[test]
    fn the_built_in_roles_are_not_written_to_the_file() {
        // They are code. Writing them would freeze today's definition of
        // `operator` into every machine that ever opened this screen.
        let path = scratch("no-builtins");

        let mut store = FileStore::load(&path).unwrap();
        store.set("ops", "hunter2", "operator").unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let document: Document = serde_json::from_str(&text).unwrap();
        assert!(
            document.roles.is_empty(),
            "built-in roles were written: {:?}",
            document.roles
        );
    }

    #[test]
    fn a_file_cannot_quietly_redefine_what_admin_means() {
        // Otherwise "you are an administrator here" becomes a claim that
        // depends on a JSON file nobody reads.
        let path = scratch("shadow-admin");
        // Written through serde rather than by hand, so the fixture cannot
        // drift away from how permissions are actually stored.
        let document = Document {
            users: BTreeMap::new(),
            roles: [("admin".to_string(), Permission::VIEW)]
                .into_iter()
                .collect(),
        };
        std::fs::write(&path, serde_json::to_string(&document).unwrap()).unwrap();

        let store = FileStore::load(&path).unwrap();
        let admin = store
            .known_roles()
            .iter()
            .find(|role| role.name == "admin")
            .expect("admin is built in and always present");
        assert_eq!(admin.permissions, Permission::all());
        assert!(admin.builtin);
        assert_eq!(
            store
                .known_roles()
                .iter()
                .filter(|r| r.name == "admin")
                .count(),
            1
        );
    }

    #[test]
    fn a_role_that_cannot_see_the_screen_is_refused() {
        // It would be accepted everywhere, grant a session, and show nothing.
        let mut store = FileStore::load(scratch("blind-role")).unwrap();
        assert!(store.define_role("mute", Permission::AUDIO).is_err());
        assert!(store
            .define_role("listener", Permission::VIEW | Permission::AUDIO)
            .is_ok());
    }

    #[test]
    fn a_built_in_role_cannot_be_redefined_or_removed() {
        let mut store = FileStore::load(scratch("builtin-guard")).unwrap();
        assert!(store.define_role("admin", Permission::VIEW).is_err());
        assert!(store.remove_role("operator").is_err());
    }

    #[test]
    fn a_role_still_held_by_somebody_cannot_be_removed() {
        // Removing it would leave that account naming a role nothing knows,
        // taking its password, and being refused with nothing on screen to say
        // why.
        let mut store = FileStore::load(scratch("role-in-use")).unwrap();
        store
            .define_role("backup", Permission::VIEW | Permission::FILE_READ)
            .unwrap();
        store.set("archiver", "hunter2", "backup").unwrap();

        let error = store.remove_role("backup").unwrap_err().to_string();
        assert!(error.contains("archiver"), "unhelpful message: {error}");

        store.remove("archiver").unwrap();
        assert!(store.remove_role("backup").unwrap());
    }

    #[test]
    fn a_role_name_that_would_read_ambiguously_is_refused() {
        let mut store = FileStore::load(scratch("role-names")).unwrap();
        for bad in ["", "   ", "semi;colon", "new\nline", &"x".repeat(33)] {
            assert!(
                store.define_role(bad, Permission::VIEW).is_err(),
                "accepted: {bad:?}"
            );
        }
        for good in ["backup", "read only", "shift-2", "Night_Ops"] {
            assert!(
                store.define_role(good, Permission::VIEW).is_ok(),
                "refused: {good:?}"
            );
        }
    }

    #[test]
    fn a_role_name_typed_with_a_stray_space_is_tidied_rather_than_refused() {
        // Trailing spaces come from typing, not from intent. Storing one would
        // produce two roles that look identical in a list and compare unequal.
        let mut store = FileStore::load(scratch("role-trim")).unwrap();
        store.define_role("  backup  ", Permission::VIEW).unwrap();

        assert!(store.known_roles().iter().any(|role| role.name == "backup"));
        assert_eq!(
            store
                .known_roles()
                .iter()
                .filter(|role| role.name.trim() == "backup")
                .count(),
            1
        );
    }

    #[test]
    fn an_account_says_whether_it_could_actually_connect() {
        let mut store = FileStore::load(scratch("can-connect")).unwrap();
        store.set("ops", "hunter2", "operator").unwrap();
        assert!(store.account("ops").unwrap().can_connect());

        store.set_enabled("ops", false).unwrap();
        assert!(!store.account("ops").unwrap().can_connect());
    }

    #[test]
    fn an_account_whose_role_vanished_reports_the_nothing_it_would_be_given() {
        // Not the role name as though it still meant something. What the login
        // prompt would hand it is nothing at all, so that is what shows.
        let path = scratch("orphan-role");
        std::fs::write(
            &path,
            r#"{"users":{"ghost":{"password_hash":"x","role":"gone","enabled":true}}}"#,
        )
        .unwrap();

        let store = FileStore::load(&path).unwrap();
        let account = store.account("ghost").unwrap();
        assert_eq!(account.role, "gone");
        assert_eq!(account.permissions, Permission::empty());
        assert!(!account.can_connect());
    }
}
