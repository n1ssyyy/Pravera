//! Putting Pravera into the session that has a desktop.
//!
//! The service runs in session 0, which has no desktop and never will. So it
//! finds whichever session is currently on the physical console, borrows that
//! user's token, and starts `pravera.exe --hidden` as them. That process is
//! the one that captures the screen and injects input, because it is the one
//! sitting in front of a screen.
//!
//! # The four calls, and why each is needed
//!
//! `WTSGetActiveConsoleSessionId` says which session owns the console right
//! now. It changes on login, on logout, on fast user switching, and on lock —
//! which is why the service listens for session changes rather than launching
//! once and hoping.
//!
//! `WTSQueryUserToken` gets that session's user token. It requires
//! `SE_TCB_NAME`, which LocalSystem has and nothing else does; this is the
//! reason the service runs as LocalSystem rather than as a lesser account.
//!
//! `DuplicateTokenEx` turns the impersonation token into a primary token,
//! because `CreateProcessAsUserW` will not take the other kind.
//!
//! `CreateEnvironmentBlock` builds the user's environment. Skipping it starts
//! a process with LocalSystem's `%APPDATA%`, which would put Pravera's device
//! key and account file somewhere the interactive Pravera would never look —
//! two identities on one machine, and no obvious reason why.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use windows::core::{HSTRING, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, SecurityImpersonation, TokenLinkedToken, TokenPrimary,
    TOKEN_ALL_ACCESS, TOKEN_LINKED_TOKEN,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetExitCodeProcess, CREATE_UNICODE_ENVIRONMENT, NORMAL_PRIORITY_CLASS,
    PROCESS_INFORMATION, STARTUPINFOW,
};

/// A handle that closes itself, so an early return cannot leak one.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe { CloseHandle(self.0) }.ok();
        }
    }
}

/// No session owns the console. Windows reports this as `0xFFFFFFFF`.
const NO_SESSION: u32 = u32::MAX;

/// The Pravera the service starts in a desktop.
///
/// This same file. One executable is three programs, told apart by how it was
/// started, which is what lets the whole thing be a single file somebody can
/// copy to a machine and run. Read fresh each time rather than cached, so a
/// Pravera replaced on disk is the one that gets started next.
pub fn executable() -> Result<PathBuf> {
    std::env::current_exe().context("Pravera could not find its own executable")
}

/// Which session is on the physical console, or `None` if nobody is signed in.
pub fn console_session() -> Option<u32> {
    match unsafe { WTSGetActiveConsoleSessionId() } {
        NO_SESSION => None,
        // Session 0 is the service session. It never has a desktop, so being
        // told the console is session 0 means the same thing as nobody being
        // there.
        0 => None,
        id => Some(id),
    }
}

/// The elevated half of a split administrator token, if this one has a half.
///
/// `None` on a standard-user account, which has no second token, and on any
/// account where UAC is off — in both cases the token that came back from the
/// session is already the only one there is.
fn linked_token(token: HANDLE) -> Option<Owned> {
    let mut linked = TOKEN_LINKED_TOKEN::default();
    let mut returned = 0u32;
    unsafe {
        GetTokenInformation(
            token,
            TokenLinkedToken,
            Some(std::ptr::from_mut(&mut linked).cast()),
            u32::try_from(std::mem::size_of::<TOKEN_LINKED_TOKEN>()).ok()?,
            &mut returned,
        )
    }
    .ok()?;

    (!linked.LinkedToken.is_invalid()).then_some(Owned(linked.LinkedToken))
}

/// A Pravera the service started and is responsible for.
pub struct Agent {
    process: Owned,
    /// Kept so the service can tell "the session changed" from "the same
    /// session, and the agent died".
    pub session: u32,
}

impl Agent {
    /// Whether the process is still running.
    pub fn is_alive(&self) -> bool {
        self.exit_code() == Some(259)
    }

    /// The process's exit code, if it has exited.
    ///
    /// `None` while still running (`STILL_ACTIVE`) or if the handle is gone.
    /// `Some(7)` is [`crate::lifecycle::EXIT_ALREADY_RUNNING`] — "an instance already covers this session; do not restart me".
    pub fn exit_code(&self) -> Option<u32> {
        let mut code = 0u32;
        match unsafe { GetExitCodeProcess(self.process.0, &mut code) } {
            Ok(()) if code == 259 => None,
            Ok(()) => Some(code),
            Err(_) => None,
        }
    }
}

/// Start Pravera in the given session, as that session's user.
pub fn launch(session: u32) -> Result<Agent> {
    let exe = executable()?;

    let mut token = HANDLE::default();
    unsafe { WTSQueryUserToken(session, &mut token) }.map_err(|error| {
        anyhow!("Could not borrow session {session}'s sign-in to start Pravera: {error}")
    })?;
    let token = Owned(token);

    // On an administrator account, the token the session hands back is the
    // *filtered* one — the standard-user half of the split UAC pair. Pravera
    // asks for elevation in its manifest, so launching with the filtered token
    // fails outright with `ERROR_ELEVATION_REQUIRED`, and there is no UAC
    // prompt to answer on a machine nobody is sitting at. The elevated
    // counterpart is reachable from the filtered one, so take it when it is
    // there and carry on with the plain token when it is not, which is what a
    // standard-user account looks like.
    let elevated = linked_token(token.0);
    let source = elevated.as_ref().map_or(token.0, |handle| handle.0);

    // `CreateProcessAsUserW` refuses an impersonation token, which is what
    // `WTSQueryUserToken` hands back.
    let mut primary = HANDLE::default();
    unsafe {
        DuplicateTokenEx(
            source,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
    }
    .map_err(|error| anyhow!("Could not prepare session {session}'s sign-in: {error}"))?;
    let primary = Owned(primary);

    let mut environment = std::ptr::null_mut();
    // `false` means "do not inherit the service's own environment", which is
    // the point: LocalSystem's environment is not the one Pravera's files live
    // in.
    let has_environment =
        unsafe { CreateEnvironmentBlock(&mut environment, Some(primary.0), false) }.is_ok();

    let mut startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    // The interactive desktop of that session. Without this the process is
    // started on no desktop at all and its window never appears — which for a
    // `--hidden` start looks like success right up until somebody unhides it.
    let desktop = HSTRING::from("winsta0\\default");
    startup.lpDesktop = PWSTR(desktop.as_ptr() as *mut u16);

    // Started hidden: this is a machine with no monitor, and on the rare
    // occasion it has one, a window appearing by itself at boot is not what
    // anybody wants.
    let mut command = HSTRING::from(format!(
        "\"{}\" --hidden {}",
        exe.display(),
        crate::AGENT_FLAG
    ))
    .to_string()
    .encode_utf16()
    .chain(std::iter::once(0))
    .collect::<Vec<u16>>();

    let mut info = PROCESS_INFORMATION::default();
    let started = unsafe {
        CreateProcessAsUserW(
            Some(primary.0),
            None,
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT | NORMAL_PRIORITY_CLASS,
            if has_environment {
                Some(environment)
            } else {
                None
            },
            None,
            &startup,
            &mut info,
        )
    };

    if has_environment {
        unsafe { DestroyEnvironmentBlock(environment) }.ok();
    }

    started.map_err(|error| anyhow!("Could not start Pravera in session {session}: {error}"))?;

    // The thread handle is of no use to anyone here; the process handle is
    // what tells us whether the agent is still up.
    unsafe { CloseHandle(info.hThread) }.ok();

    Ok(Agent {
        process: Owned(info.hProcess),
        session,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_console_session_is_never_session_zero() {
        // Session 0 has no desktop. Reporting it as the console session would
        // send the service into a loop launching agents that can capture
        // nothing.
        if let Some(session) = console_session() {
            assert_ne!(session, 0);
        }
    }

    #[test]
    fn the_agent_started_is_this_very_file() {
        // One executable, three programs. Launching anything else would mean a
        // Pravera copied to a new machine could start a stale one sitting
        // beside it.
        let started = executable().expect("this executable exists");
        assert_eq!(started, std::env::current_exe().unwrap());
    }
}
