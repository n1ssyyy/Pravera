//! Machines this one has connected to before.
//!
//! A connect code is fifty-two characters of Crockford base32, and typing it
//! twice is one time too many. Once a session has actually started — not when
//! the code was typed, but when the far end accepted it — the code is kept,
//! along with the name that machine calls itself and the account that was used.
//! Reconnecting is then a password.
//!
//! ## What is not kept
//!
//! The password. There is no keychain integration here yet, and writing a
//! credential to a JSON file so a remote-desktop session can start without one
//! is precisely the arrangement that turns one compromised laptop into every
//! machine it could reach. The code is a public key and the username is not a
//! secret; the password is the only thing between someone holding this file and
//! the desktops it names, and it stays in a person's head.
//!
//! ## Why remembering is safe
//!
//! The stored code *is* the peer's public key, so it is what gets dialled — not
//! the name, not the address. Matching a machine on the Devices list to a
//! remembered one by hostname therefore cannot connect anybody to the wrong
//! machine: if the thing answering at that name is not the machine whose key
//! was saved, the QUIC handshake fails and no password is ever sent. The worst
//! a hostname collision can do is offer a fill that then does not connect.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use pravera_core::DeviceId;
use pravera_transport::PeerKey;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// How many machines are remembered.
///
/// A list this is meant to be glanced at, not scrolled. Past a few dozen the
/// screen stops being a shortcut and becomes a filing cabinet, and the oldest
/// entries are the ones nobody has used in months.
const LIMIT: usize = 24;

/// A machine a session has successfully been made to.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    /// The connect code, which is the peer's public key. This is what a dial
    /// uses, and the only field here that establishes anything.
    pub code: String,
    /// What the host called itself. A display hint the host chose, so it is
    /// shown but never trusted — see [`crate::net::link`].
    pub name: String,
    /// The account signed in with last time. Not a secret, and not a claim: the
    /// host checks it again on every connection.
    pub username: String,
    /// Seconds since the Unix epoch. Wall clock rather than an `Instant`
    /// because it has to survive the process that recorded it.
    pub last_used: u64,
}

impl Machine {
    /// The fingerprint shown in the interface, or `None` if the stored code no
    /// longer parses — which means a hand-edited file, and is not worth
    /// refusing to start over.
    pub fn device_id(&self) -> Option<DeviceId> {
        PeerKey::from_code(&self.code)
            .ok()
            .map(|key| key.device_id())
    }

    /// Whether a discovered peer plausibly refers to this machine.
    ///
    /// Hostnames only, because that is all discovery reports. A match is an
    /// offer to fill the form, never an identity: see the note at the top of
    /// this file for why that is safe.
    pub fn answers_to(&self, name: &str) -> bool {
        !self.name.trim().is_empty() && label(&self.name) == label(name)
    }
}

impl std::fmt::Debug for Machine {
    /// Prints the fingerprint rather than the key.
    ///
    /// The code is not a secret, but sixty-odd characters of base32 in a log
    /// line is unreadable, and `PeerKey` already made the same choice.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Machine")
            .field("name", &self.name)
            .field("username", &self.username)
            .field(
                "device",
                &self
                    .device_id()
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "unreadable".into()),
            )
            .finish()
    }
}

/// The first label of a hostname, lowercased.
///
/// A machine appears as `Evercore` to itself and `evercore.example-tailnet.ts.net`
/// on the tailnet. Comparing the whole string would miss every match that
/// matters.
fn label(name: &str) -> String {
    name.trim()
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// Everything remembered, newest first.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Known {
    machines: Vec<Machine>,
}

impl Known {
    pub fn machines(&self) -> &[Machine] {
        &self.machines
    }

    pub fn is_empty(&self) -> bool {
        self.machines.is_empty()
    }

    /// A session was made. Record it, or update what was already known.
    ///
    /// Keyed by the code, because that is the identity. A machine that was
    /// renamed keeps its place in the list rather than appearing twice.
    pub fn remember(&mut self, code: &str, name: &str, username: &str) {
        let code = code.trim();
        if PeerKey::from_code(code).is_err() {
            // Refusing rather than storing it: an entry that cannot be dialled
            // is a row that fills the form with something that will not work.
            warn!("declined to remember a machine whose connect code does not parse");
            return;
        }

        let machine = Machine {
            code: code.to_string(),
            name: name.trim().to_string(),
            username: username.trim().to_string(),
            last_used: now(),
        };

        self.machines.retain(|held| held.code != machine.code);
        self.machines.insert(0, machine);
        self.machines.truncate(LIMIT);
    }

    /// Stop remembering one machine.
    pub fn forget(&mut self, code: &str) {
        self.machines.retain(|held| held.code != code);
    }

    /// The machine a discovered peer refers to, if one is remembered.
    pub fn matching(&self, name: &str) -> Option<&Machine> {
        self.machines.iter().find(|held| held.answers_to(name))
    }

    /// Read the list at `path`, falling back to an empty one.
    ///
    /// An unreadable file is never fatal. This is a convenience, and a machine
    /// that cannot start because a shortcut list is malformed is worse than a
    /// machine that asks for a connect code.
    pub fn load(path: &Path) -> Known {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(known) => known,
                Err(error) => {
                    warn!(
                        path = %path.display(),
                        %error,
                        "the known machines could not be read; starting with none"
                    );
                    Known::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                debug!(path = %path.display(), "no known machines yet");
                Known::default()
            }
            Err(error) => {
                warn!(path = %path.display(), %error, "the known machines could not be opened");
                Known::default()
            }
        }
    }

    /// Write the list out. Failure is logged and otherwise ignored: losing a
    /// shortcut is not worth interrupting anyone over.
    pub fn save(&self, path: &Path) {
        let Ok(text) = serde_json::to_string_pretty(self) else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                warn!(path = %parent.display(), %error, "the data directory is unreachable");
                return;
            }
        }
        if let Err(error) = write_private(path, &text) {
            warn!(path = %path.display(), %error, "the known machines could not be saved");
        }
    }

    pub fn path_in(data_dir: &Path) -> PathBuf {
        data_dir.join("known_machines.json")
    }
}

/// Write a file only its owner can read.
///
/// This names every machine the person reaches and the account they use on
/// each. None of it is a credential, and all of it is a map worth having if you
/// are looking for one.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
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
            .open(path)?;
        file.write_all(text.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, text)
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two distinct, real connect codes.
    fn code(seed: u8) -> String {
        let mut key = [0u8; 32];
        for (index, byte) in key.iter_mut().enumerate() {
            *byte = (index as u8).wrapping_mul(37).wrapping_add(seed);
        }
        pravera_core::connect_code::grouped(&key)
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("pravera-known-tests");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{name}.json"));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn a_machine_connected_to_once_does_not_have_to_be_typed_again() {
        let mut known = Known::default();
        known.remember(&code(1), "Evercore", "driver");

        let held = &known.machines()[0];
        assert_eq!(held.code, code(1));
        assert_eq!(held.username, "driver");
        assert_eq!(
            held.device_id(),
            PeerKey::from_code(&code(1)).ok().map(|k| k.device_id())
        );
    }

    #[test]
    fn connecting_again_updates_the_entry_rather_than_adding_a_second() {
        // Otherwise the list fills with the same machine and the newest
        // username is buried under every older one.
        let mut known = Known::default();
        known.remember(&code(1), "Evercore", "driver");
        known.remember(&code(1), "Evercore", "admin");

        assert_eq!(known.machines().len(), 1);
        assert_eq!(known.machines()[0].username, "admin");
    }

    #[test]
    fn a_renamed_machine_keeps_its_place_because_the_key_is_the_identity() {
        let mut known = Known::default();
        known.remember(&code(1), "old-name", "driver");
        known.remember(&code(1), "new-name", "driver");

        assert_eq!(known.machines().len(), 1);
        assert_eq!(known.machines()[0].name, "new-name");
    }

    #[test]
    fn the_newest_machine_is_first() {
        let mut known = Known::default();
        known.remember(&code(1), "first", "driver");
        known.remember(&code(2), "second", "driver");

        assert_eq!(known.machines()[0].name, "second");
        assert_eq!(known.machines()[1].name, "first");
    }

    #[test]
    fn the_list_does_not_grow_without_end() {
        let mut known = Known::default();
        for seed in 0..40u8 {
            known.remember(&code(seed), &format!("machine-{seed}"), "driver");
        }
        assert_eq!(known.machines().len(), LIMIT);
        // The ones kept are the ones most recently used.
        assert_eq!(known.machines()[0].name, "machine-39");
    }

    #[test]
    fn a_code_that_does_not_parse_is_never_remembered() {
        // A row that fills the form with something undialable is worse than no
        // row: the person presses Connect and waits for a failure the app
        // already knew about.
        let mut known = Known::default();
        known.remember("PRV-NOT-A-CODE", "Evercore", "driver");
        assert!(known.is_empty());
    }

    #[test]
    fn a_machine_can_be_forgotten() {
        let mut known = Known::default();
        known.remember(&code(1), "Evercore", "driver");
        known.forget(&code(1));
        assert!(known.is_empty());
    }

    #[test]
    fn a_tailnet_name_matches_the_machine_that_calls_itself_by_its_first_label() {
        // The Devices list shows `evercore.example-tailnet.ts.net`; the host
        // reported `Evercore`. Comparing the whole string would match nothing
        // anyone actually has.
        let mut known = Known::default();
        known.remember(&code(1), "Evercore", "driver");

        assert!(known.matching("evercore.example-tailnet.ts.net").is_some());
        assert!(known.matching("EVERCORE").is_some());
        assert!(known.matching("  Evercore  ").is_some());
        assert!(known.matching("something-else").is_none());
    }

    #[test]
    fn a_machine_with_no_name_matches_nothing() {
        // Otherwise every unnamed peer on the list would claim to be it.
        let mut known = Known::default();
        known.remember(&code(1), "", "driver");

        assert!(known.matching("").is_none());
        assert!(known.matching("anything").is_none());
    }

    #[test]
    fn the_list_survives_a_restart() {
        let path = scratch("round-trip");
        let mut known = Known::default();
        known.remember(&code(1), "Evercore", "driver");
        known.save(&path);

        let read = Known::load(&path);
        assert_eq!(read.machines().len(), 1);
        assert_eq!(read.machines()[0].code, code(1));
        assert_eq!(read.machines()[0].username, "driver");
    }

    #[test]
    fn a_missing_or_broken_file_leaves_the_app_working() {
        assert!(Known::load(&scratch("absent")).is_empty());

        let path = scratch("corrupt");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(Known::load(&path).is_empty());
    }

    #[test]
    fn nothing_written_to_disk_resembles_a_password() {
        // The one invariant this file has to hold. There is no field for a
        // password, and this fails the moment somebody adds one.
        let path = scratch("no-password");
        let mut known = Known::default();
        known.remember(&code(1), "Evercore", "driver");
        known.save(&path);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.to_lowercase().contains("password"), "{text}");
        assert!(!text.to_lowercase().contains("secret"), "{text}");
    }

    #[test]
    fn debug_prints_the_fingerprint_rather_than_the_key() {
        let mut known = Known::default();
        known.remember(&code(1), "Evercore", "driver");

        let shown = format!("{:?}", known.machines()[0]);
        assert!(!shown.contains(&code(1)), "{shown}");
        assert!(shown.contains("Evercore"), "{shown}");
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;

        let path = scratch("permissions");
        let mut known = Known::default();
        known.remember(&code(1), "Evercore", "driver");
        known.save(&path);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }
}
