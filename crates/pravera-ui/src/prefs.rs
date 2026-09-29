//! The handful of choices that have to outlive the process.
//!
//! Deliberately small. Anything that can be worked out at startup is worked
//! out at startup rather than remembered — a settings file that records what
//! the app could have asked the system is a settings file that goes stale.
//! Whether Pravera starts at sign-in is not in here, for exactly that reason:
//! the registry entry is the truth and [`crate::autostart`] reads it.
//!
//! What is here is the one thing nothing else knows: whether a machine that
//! starts by itself should also start accepting sessions by itself.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// Choices that survive a restart.
///
/// Every field has a default, and an unreadable file falls back to those
/// rather than refusing to start: a machine with no monitor cannot be told
/// about a broken settings file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Start accepting sessions as soon as Pravera runs.
    ///
    /// Off by default, and it stays off until there is a saved account: a
    /// machine that opens itself to the network the first time it is launched
    /// is not something to arrive at by accident.
    pub host_at_launch: bool,
    /// Which saved account hosting should use. `None` means the only one.
    pub host_account: Option<String>,
    /// Ask a host for its sound as well as its screen.
    ///
    /// On by default, unlike the two above. Those decide whether this machine
    /// makes itself reachable, which nobody should arrive at by accident;
    /// this only decides what an outgoing session asks for, and a remote
    /// desktop that is silent until you find a setting is a remote desktop
    /// most people conclude has no sound.
    pub hear_the_host: bool,
    /// The sidebar folded down to its icon rail. A layout choice, kept
    /// because somebody who folded it away once wanted it out of the way.
    pub sidebar_collapsed: bool,
    /// Apply a downloaded update by itself when nothing is in use. On by
    /// default: the machines that most need fixes are the ones nobody sits
    /// at, and an update never lands under a live session either way.
    pub auto_update: bool,
}

impl Default for Prefs {
    fn default() -> Prefs {
        Prefs {
            host_at_launch: false,
            host_account: None,
            hear_the_host: true,
            sidebar_collapsed: false,
            auto_update: true,
        }
    }
}

impl Prefs {
    /// Read the settings at `path`, falling back to the defaults.
    pub fn load(path: &Path) -> Prefs {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(prefs) => prefs,
                Err(error) => {
                    // Not fatal, and not silently overwritten either: the file
                    // is left where it is so someone can look at it, and the
                    // next `save` will replace it.
                    warn!(
                        path = %path.display(),
                        %error,
                        "the settings could not be read; using the defaults"
                    );
                    Prefs::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                debug!(path = %path.display(), "no settings yet");
                Prefs::default()
            }
            Err(error) => {
                warn!(path = %path.display(), %error, "the settings could not be opened");
                Prefs::default()
            }
        }
    }

    /// Write the settings out. Failure is logged and otherwise ignored: losing
    /// a preference is not worth interrupting anyone over.
    pub fn save(&self, path: &Path) {
        let Ok(text) = serde_json::to_string_pretty(self) else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                warn!(path = %parent.display(), %error, "the settings directory is unreachable");
                return;
            }
        }
        if let Err(error) = std::fs::write(path, text) {
            warn!(path = %path.display(), %error, "the settings could not be saved");
        }
    }

    /// Where the settings live, given the data directory.
    pub fn path_in(data_dir: &Path) -> PathBuf {
        data_dir.join("settings.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("pravera-prefs-tests");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{name}.json"));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn a_choice_survives_a_restart() {
        let path = scratch("round-trip");
        let prefs = Prefs {
            host_at_launch: true,
            host_account: Some("operator".into()),
            hear_the_host: false,
            sidebar_collapsed: true,
            auto_update: false,
        };
        prefs.save(&path);

        assert_eq!(Prefs::load(&path), prefs);
    }

    #[test]
    fn a_machine_does_not_open_itself_to_the_network_by_default() {
        // The first launch on a new machine must not be the launch that makes
        // its screen reachable.
        assert!(!Prefs::default().host_at_launch);
    }

    #[test]
    fn a_missing_file_gives_the_defaults_rather_than_a_failure() {
        assert_eq!(Prefs::load(&scratch("absent")), Prefs::default());
    }

    #[test]
    fn an_unreadable_file_still_lets_the_app_start() {
        // A machine with no monitor cannot be told its settings file is
        // broken, so refusing to start over one would strand it.
        let path = scratch("corrupt");
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(Prefs::load(&path), Prefs::default());
    }

    #[test]
    fn a_file_written_by_an_older_build_keeps_what_it_knew() {
        // `serde(default)` on the struct means a field added later takes its
        // default instead of the whole file failing to parse.
        let path = scratch("partial");
        std::fs::write(&path, r#"{"host_at_launch": true}"#).unwrap();

        let prefs = Prefs::load(&path);
        assert!(prefs.host_at_launch);
        assert_eq!(prefs.host_account, None);
        // The field the older build had never heard of takes its default,
        // rather than the whole file failing to parse.
        assert!(prefs.hear_the_host);
    }

    #[test]
    fn a_session_has_sound_without_anybody_turning_it_on() {
        // The opposite default to the two hosting switches, deliberately: this
        // one only decides what an outgoing session asks for.
        assert!(Prefs::default().hear_the_host);
    }

    #[test]
    fn the_settings_sit_beside_the_rest_of_the_state() {
        let path = Prefs::path_in(Path::new("/somewhere"));
        assert_eq!(path.file_name().unwrap(), "settings.json");
        assert_eq!(path.parent().unwrap(), Path::new("/somewhere"));
    }
}
