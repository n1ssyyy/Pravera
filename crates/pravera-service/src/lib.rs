//! The Windows service that keeps Pravera running on a machine nobody is
//! sitting at.
//!
//! # One file
//!
//! There is no separate service executable. `pravera.exe` is the service, the
//! agent and the interface, and which one it is depends on how it was started:
//!
//! | started as | what it is |
//! |---|---|
//! | `pravera.exe` | the interface, which registers the service if it can |
//! | `pravera.exe --service` | the service body, called by Windows |
//! | `pravera.exe --hidden` | the agent, started by the service in a desktop |
//!
//! That is what makes it portable: one file to copy, and copying it somewhere
//! else is enough — [`ensure_installed`] notices the registered path no longer
//! matches and repoints the service at wherever the file is now.
//!
//! # What this is for
//!
//! A scheduled task at logon covers the ordinary case, and Pravera already
//! installs one. It cannot cover the case this exists for: a box with no
//! monitor that reboots at three in the morning and has to come back by
//! itself, before and regardless of anybody signing in.
//!
//! # Why it launches an agent instead of hosting directly
//!
//! A service runs in session 0, which has no desktop. Nothing in session 0 can
//! capture a screen or inject a keystroke, no matter what privileges it holds
//! — the isolation is the point of session 0 and has been since Vista. So the
//! part that captures has to run in the interactive session, and the service's
//! job is to put it there and keep it there:
//!
//! - at start, and whenever the console session changes, it launches
//!   `pravera.exe --hidden` into whichever session is on the physical console,
//!   as that session's user;
//! - when that session goes away — logout, fast user switch — the agent goes
//!   with it, and the service launches a new one for the session that took
//!   over;
//! - when nobody is logged in there is no console session to launch into, and
//!   the service waits. That is not a gap being hidden: with no session there
//!   is no desktop, and a desktop is the thing being shared.
//!
//! The alternative — the service owning the endpoint and talking to a thin
//! agent over a named pipe — buys the ability to serve the login screen
//! itself, and costs a whole IPC protocol and a second copy of the session
//! state. It is the right destination and this is not it yet; what is here now
//! is the part that makes a headless machine come back after a reboot, which
//! is what the box is for.
//!
//! # What it deliberately does not do
//!
//! It holds no accounts, verifies no passwords, and makes no decision about
//! who may do what. Every one of those stays in `pravera-host`, reached the
//! same way whether Pravera was started by this service or by somebody
//! double-clicking it. A service that duplicated the permission checks would
//! be a second place for them to drift.

#![cfg_attr(not(windows), allow(dead_code))]

#[cfg(windows)]
mod agent;
#[cfg(windows)]
mod scm;
pub mod secure;
#[cfg(windows)]
mod service;

/// What the service is registered as. Not shown to anyone.
pub const SERVICE_NAME: &str = "Pravera";
/// What the Services list shows.
pub const DISPLAY_NAME: &str = "Pravera Remote Desktop";
pub const DESCRIPTION: &str = "Keeps Pravera reachable on this machine, including after a \
     restart and before anybody signs in.";

/// The argument that makes this executable the service rather than the
/// interface. Windows passes it, nobody types it.
pub const SERVICE_FLAG: &str = "--service";

/// The argument that marks a Pravera the *service* started, as opposed to one
/// a person started or one the logon task started.
///
/// All three run the same interface. Only this one is owned by something else:
/// stopping it does nothing, because the service starts it again within
/// seconds. That is why it needs telling apart — an interface that offered to
/// quit a process it cannot quit would be lying, and two Praveras in one
/// session put two icons in the notification area for one program.
pub const AGENT_FLAG: &str = "--agent";

/// Whether this process was started by the service control manager.
pub fn is_service_launch() -> bool {
    std::env::args().any(|arg| arg == SERVICE_FLAG)
}

/// Whether this process is the agent the service put into a desktop.
pub fn is_agent_launch() -> bool {
    std::env::args().any(|arg| arg == AGENT_FLAG)
}

/// What [`ensure_installed`] did, so the interface can say so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installed {
    /// Registered for the first time.
    Registered,
    /// Was registered against a different path — this file moved — and now
    /// points here. The reason a portable executable stays working after
    /// somebody drags it to another folder.
    Repointed,
    /// Already registered against this file. Nothing to do.
    Unchanged,
    /// Not registered and cannot be, because this process is not elevated.
    /// Not an error: Pravera runs perfectly well without it, just not before
    /// somebody signs in.
    NotElevated,
    /// Not registered. Only ever said by a read that changed nothing, so it
    /// says nothing about whether a change would be allowed.
    NotRegistered,
    /// Registered, but for a different copy of Pravera: this file moved, or
    /// another one set the service up. It starts at boot, just not this file.
    Elsewhere,
    /// Registered, but Windows refused for some other reason, in its words.
    Refused(String),
}

impl Installed {
    /// Whether the machine will now start Pravera by itself at boot.
    pub const fn is_installed(&self) -> bool {
        matches!(
            self,
            Installed::Registered | Installed::Repointed | Installed::Unchanged
        )
    }
}

/// Register the service, or repoint it at this file if it has moved.
///
/// Called every time the interface starts. It is cheap — three calls to the
/// service control manager and a string comparison — and doing it on every
/// start is what makes the executable portable: the registered command line is
/// reconciled against where the file actually is, rather than being written
/// once at install time and quietly rotting when somebody moves it.
#[cfg(windows)]
pub fn ensure_installed() -> Installed {
    scm::ensure_installed()
}

#[cfg(not(windows))]
pub fn ensure_installed() -> Installed {
    // The same job on Linux is a systemd unit, which is installed by a package
    // rather than by the application, because a program that writes its own
    // unit file into /etc is a program fighting the package manager.
    Installed::NotElevated
}

/// What is registered, read without changing anything and without elevation.
///
/// The interface asks this on every visit to Settings, and on every start. It
/// is [`ensure_installed`] that must not be called for a question: it writes.
pub fn current() -> Installed {
    #[cfg(windows)]
    {
        scm::current()
    }
    #[cfg(not(windows))]
    {
        Installed::NotRegistered
    }
}

/// The start-of-run reconciliation: keep a registration that exists pointed at
/// this copy, and never create one.
///
/// Creating the service used to happen here whenever the process was
/// elevated. That made "Stop starting at boot" undoable by the next elevated
/// start, which is a switch that turns itself back on. Registering is now the
/// person's decision, made in Settings.
pub fn reconcile() -> Installed {
    match current() {
        Installed::Unchanged => {
            // Registered and right. The write is only to start a stopped
            // service and to re-assert the Ctrl+Alt+Del policy, and an
            // ordinary process is refused it, which changes nothing worth
            // reporting.
            let _ = ensure_installed();
            Installed::Unchanged
        }
        Installed::Elsewhere => match ensure_installed() {
            Installed::NotElevated => Installed::Elsewhere,
            repointed => repointed,
        },
        other => other,
    }
}

/// Remove the registration. The interface offers this; nothing calls it by
/// itself, because a Pravera that uninstalled its own service on exit would be
/// a Pravera that never survived a reboot.
#[cfg(windows)]
pub fn uninstall() -> Result<String, String> {
    scm::uninstall().map_err(|error| format!("{error:#}"))
}

#[cfg(not(windows))]
pub fn uninstall() -> Result<String, String> {
    Err("There is no Windows service to remove on this platform.".into())
}

/// Whether the service is registered at all. The uninstaller asks, because a
/// service left pointing at a deleted file restarts nothing but fails every
/// few seconds forever.
#[cfg(windows)]
pub fn is_registered() -> bool {
    scm::is_registered()
}

#[cfg(not(windows))]
pub fn is_registered() -> bool {
    false
}

/// Whether the registered service runs this particular executable.
#[cfg(windows)]
pub fn registered_for(exe: &std::path::Path) -> bool {
    scm::registered_for(exe)
}

#[cfg(not(windows))]
pub fn registered_for(_exe: &std::path::Path) -> bool {
    false
}

/// Whether the service is registered and running, in a sentence.
#[cfg(windows)]
pub fn status() -> String {
    match scm::status() {
        Ok(text) => text,
        Err(error) => format!("{error:#}"),
    }
}

#[cfg(not(windows))]
pub fn status() -> String {
    "There is no Windows service on this platform.".into()
}

/// Hand this process to the service control manager and run until it is
/// stopped. Only ever called when [`is_service_launch`] is true.
#[cfg(windows)]
pub fn run() -> std::process::ExitCode {
    service::run()
}

#[cfg(not(windows))]
pub fn run() -> std::process::ExitCode {
    eprintln!("pravera: there is no Windows service to run on this platform.");
    std::process::ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asking_what_is_registered_is_a_read_that_never_needs_elevation() {
        // Whatever this machine has, the answer is one of the states that
        // describe it; never `NotElevated`, which is the answer to a write.
        let state = current();
        eprintln!("the service on this machine reads as {state:?}");
        assert!(!matches!(state, Installed::NotElevated), "{state:?}");
        // The test binary is not the registered copy, so a machine that has the
        // service registered for some other file reads as elsewhere.
        assert!(!state.is_installed(), "{state:?}");
    }

    #[test]
    fn only_a_real_registration_counts_as_installed() {
        assert!(Installed::Registered.is_installed());
        assert!(Installed::Repointed.is_installed());
        assert!(Installed::Unchanged.is_installed());
        // The two that mean the machine will not come back by itself. Reading
        // either as success would put a reassuring line in the interface on a
        // machine that is not actually covered.
        assert!(!Installed::NotElevated.is_installed());
        assert!(!Installed::NotRegistered.is_installed());
        assert!(!Installed::Elsewhere.is_installed());
        assert!(!Installed::Refused("anything".into()).is_installed());
    }
}
