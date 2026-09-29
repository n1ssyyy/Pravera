//! Keeping an installed Pravera current, on its own.
//!
//! The mechanics — the feed, the download, the checks, the swap — are in
//! [`crate::install::release`]. This is only the state the application keeps
//! about them and the policy for when to act:
//!
//! - **Check** at startup and every [`CHECK_EVERY`] after. A check is one
//!   small request, and a machine that hosts unattended for months is exactly
//!   the one that most needs fixes it will never be told about.
//! - **Download** as soon as a newer release is seen. It is verified before it
//!   is kept, and nothing is replaced until it is.
//! - **Apply** without asking only when nobody would notice: no session or
//!   shell open, nobody connected to this machine, and no window on screen.
//!   Otherwise Settings offers "Restart to update" and waits for somebody to
//!   press it. An update that dropped a live session would be worse than no
//!   update at all.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::install::release::{self, Release};

/// How often an idle Pravera asks whether there is something newer.
pub const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// How long to wait after a failed check before the next. Short enough that a
/// machine that booted before its network recovers the same morning, long
/// enough never to trouble GitHub's limits.
pub const RETRY_AFTER: Duration = Duration::from_secs(30 * 60);
/// How often the clock that decides both is read.
pub const TICK: Duration = Duration::from_secs(60);
/// A first check waits this long after startup: the window, the endpoint and
/// the discovery pass all want the network first.
pub const FIRST_CHECK_AFTER: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
pub enum Phase {
    /// Not checked yet this run.
    Idle,
    Checking,
    /// The newest release is this one, or older.
    Current,
    /// The last attempt failed, with the reason in words.
    Failed(String),
    Downloading {
        release: Release,
        done: u64,
        total: u64,
    },
    /// Downloaded, checked, and waiting to be put in place.
    Ready {
        release: Release,
        executable: PathBuf,
    },
    /// Swapping and restarting; the process is on its way out.
    Applying,
}

#[derive(Debug, Clone)]
pub struct Updater {
    pub phase: Phase,
    /// When the last check finished, whatever it found.
    checked_at: Option<Instant>,
    /// When the last check failed, for the shorter retry.
    failed_at: Option<Instant>,
    started_at: Instant,
    /// Updating is possible at all in this process. A debug build and a
    /// preview are not, see [`release::enabled`].
    pub enabled: bool,
}

impl Updater {
    pub fn new(now: Instant, enabled: bool) -> Updater {
        Updater {
            phase: Phase::Idle,
            checked_at: None,
            failed_at: None,
            started_at: now,
            enabled,
        }
    }

    /// Whether a check should start now.
    pub fn due(&self, now: Instant) -> bool {
        if !self.enabled {
            return false;
        }
        match &self.phase {
            Phase::Checking | Phase::Downloading { .. } | Phase::Ready { .. } | Phase::Applying => false,
            Phase::Idle => now.saturating_duration_since(self.started_at) >= FIRST_CHECK_AFTER,
            Phase::Failed(_) => self
                .failed_at
                .is_none_or(|at| now.saturating_duration_since(at) >= RETRY_AFTER),
            Phase::Current => self
                .checked_at
                .is_none_or(|at| now.saturating_duration_since(at) >= CHECK_EVERY),
        }
    }

    /// Whether a check or download is in flight, so a button can say so.
    pub fn busy(&self) -> bool {
        matches!(
            self.phase,
            Phase::Checking | Phase::Downloading { .. } | Phase::Applying
        )
    }

    pub fn checking(&mut self) {
        self.phase = Phase::Checking;
    }

    /// What the feed said. `Some` is a release worth downloading.
    pub fn checked(&mut self, result: Result<Release, String>, now: Instant) -> Option<Release> {
        self.checked_at = Some(now);
        match result {
            Ok(found) if found.version > release::running_version() => {
                tracing::info!(version = %found.version, "a newer Pravera is published");
                self.phase = Phase::Downloading {
                    release: found.clone(),
                    done: 0,
                    total: found.asset.size,
                };
                Some(found)
            }
            Ok(found) => {
                tracing::debug!(latest = %found.version, "Pravera is current");
                self.phase = Phase::Current;
                None
            }
            Err(reason) => {
                tracing::info!(%reason, "the update check did not finish");
                self.failed_at = Some(now);
                self.phase = Phase::Failed(reason);
                None
            }
        }
    }

    pub fn progress(&mut self, now_done: u64, now_total: u64) {
        if let Phase::Downloading { done, total, .. } = &mut self.phase {
            *done = now_done;
            if now_total > 0 {
                *total = now_total;
            }
        }
    }

    pub fn downloaded(&mut self, result: Result<PathBuf, String>, now: Instant) {
        let Phase::Downloading { release, .. } = &self.phase else {
            return;
        };
        match result {
            Ok(executable) => {
                tracing::info!(version = %release.version, "update downloaded and verified");
                self.phase = Phase::Ready {
                    release: release.clone(),
                    executable,
                };
            }
            Err(reason) => {
                tracing::warn!(%reason, "the update download failed");
                self.failed_at = Some(now);
                self.phase = Phase::Failed(reason);
            }
        }
    }

    /// The update waiting to be applied, if there is one.
    pub fn ready(&self) -> Option<(&Release, &PathBuf)> {
        match &self.phase {
            Phase::Ready { release, executable } => Some((release, executable)),
            _ => None,
        }
    }

    /// A newer version is known about, whether or not it has arrived yet.
    /// What puts a mark on the Settings icon.
    pub fn pending(&self) -> bool {
        matches!(self.phase, Phase::Downloading { .. } | Phase::Ready { .. })
    }

    /// The state in a few words, for the Settings list.
    pub fn brief(&self) -> String {
        match &self.phase {
            _ if !self.enabled => "Off".into(),
            Phase::Idle => "Not checked".into(),
            Phase::Checking => "Checking".into(),
            Phase::Current => "Up to date".into(),
            Phase::Failed(_) => "Check failed".into(),
            Phase::Downloading { release, .. } => format!("{} on its way", release.version),
            Phase::Ready { release, .. } => format!("{} ready", release.version),
            Phase::Applying => "Restarting".into(),
        }
    }

    /// The Settings row: a headline and a line under it.
    pub fn describe(&self) -> (String, String) {
        let current = format!("Pravera {}", crate::install::VERSION);
        match &self.phase {
            _ if !self.enabled => (
                current,
                "This build does not update itself: it was not installed from a release.".into(),
            ),
            Phase::Idle => (current, "Checks for a newer release shortly after starting.".into()),
            Phase::Checking => (current, "Checking for a newer release…".into()),
            Phase::Current => (current, "This is the newest release.".into()),
            Phase::Failed(reason) => (current, format!("The last check did not finish. {reason}")),
            Phase::Downloading { release, done, total } => {
                let share = if *total > 0 {
                    format!(", {}%", (done * 100 / total).min(100))
                } else {
                    String::new()
                };
                (
                    format!("Pravera {} is downloading", release.version),
                    format!("From the release page, checked before it is used{share}."),
                )
            }
            Phase::Ready { release, .. } => (
                format!("Pravera {} is ready", release.version),
                "Downloaded and checked. Restarting takes a couple of seconds; it happens by \
                 itself the next time nothing is connected and the window is closed."
                    .into(),
            ),
            Phase::Applying => (current, "Restarting into the new version…".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::release::Asset;

    fn release(version: &str) -> Release {
        Release {
            version: semver::Version::parse(version).unwrap(),
            page: String::new(),
            asset: Asset {
                name: crate::install::ASSET.into(),
                url: "https://example.test/x".into(),
                size: 10,
                sha256: None,
            },
        }
    }

    #[test]
    fn the_first_check_waits_for_startup_to_settle() {
        let start = Instant::now();
        let updater = Updater::new(start, true);
        assert!(!updater.due(start));
        assert!(updater.due(start + FIRST_CHECK_AFTER));
    }

    #[test]
    fn a_disabled_updater_never_checks() {
        let start = Instant::now();
        assert!(!Updater::new(start, false).due(start + CHECK_EVERY * 2));
    }

    #[test]
    fn a_newer_release_is_fetched_and_an_older_one_is_not() {
        let now = Instant::now();
        let mut updater = Updater::new(now, true);
        assert!(updater.checked(Ok(release("999.0.0")), now).is_some());
        assert!(updater.pending());

        let mut updater = Updater::new(now, true);
        assert!(updater.checked(Ok(release("0.0.1")), now).is_none());
        assert!(!updater.pending());
        assert!(!updater.due(now + Duration::from_secs(60)));
        assert!(updater.due(now + CHECK_EVERY));
    }

    #[test]
    fn the_state_in_a_few_words_follows_the_phase() {
        let now = Instant::now();
        assert_eq!(Updater::new(now, false).brief(), "Off");
        let mut updater = Updater::new(now, true);
        assert_eq!(updater.brief(), "Not checked");
        updater.checking();
        assert_eq!(updater.brief(), "Checking");
        updater.checked(Ok(release("0.0.1")), now);
        assert_eq!(updater.brief(), "Up to date");
        updater.checked(Err("offline".into()), now);
        assert_eq!(updater.brief(), "Check failed");
        updater.checked(Ok(release("999.0.0")), now);
        updater.progress(5, 10);
        assert!(updater.brief().ends_with("on its way"));
        updater.downloaded(Ok(PathBuf::from("x")), now);
        assert_eq!(updater.brief(), "999.0.0 ready");
    }

    #[test]
    fn a_failure_retries_sooner_than_a_success() {
        let now = Instant::now();
        let mut updater = Updater::new(now, true);
        updater.checked(Err("offline".into()), now);
        assert!(!updater.due(now + Duration::from_secs(60)));
        assert!(updater.due(now + RETRY_AFTER));
    }

    #[test]
    fn a_verified_download_is_ready_and_a_bad_one_is_not() {
        let now = Instant::now();
        let mut updater = Updater::new(now, true);
        updater.checked(Ok(release("999.0.0")), now);
        updater.progress(5, 10);
        updater.downloaded(Ok(PathBuf::from("x")), now);
        assert!(updater.ready().is_some());

        let mut updater = Updater::new(now, true);
        updater.checked(Ok(release("999.0.0")), now);
        updater.downloaded(Err("checksum".into()), now);
        assert!(updater.ready().is_none());
        assert!(matches!(updater.phase, Phase::Failed(_)));
    }
}
