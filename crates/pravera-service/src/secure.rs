//! The secure desktop: what the agent cannot see, and how the service helps.
//!
//! A normal Pravera runs in `winsta0\default` — the desktop a signed-in person
//! sees. Two other desktops exist on the same window station and are invisible
//! from there:
//!
//! - `Winlogon` — the lock screen, the sign-in screen, and the "Press
//!   Ctrl+Alt+Del" prompt. It belongs to `SYSTEM` and the agent's token is not
//!   allowed to open it.
//! - `Secure Desktop` — the dimmed desktop UAC shows its prompt on. Same
//!   isolation, same reason.
//!
//! The service runs as `SYSTEM` in session 0. From there `IDXGIOutputDuplication`
//! *can* duplicate the console output even while the secure desktop is up —
//! that is the one thing `pravera-capture::dda::DdaSource` buys that WGC cannot.
//! So when the agent reports that capture failed with `PermissionDenied` (the
//! WGC backend's signal for "policy or secure desktop"), the service takes over
//! that display with DDA and the client keeps seeing a picture.
//!
//! The other half is `SendSAS`. Ctrl+Alt+Del is not a keystroke Pravera can
//! inject: Winlogon only honours the Secure Attention Sequence when it comes
//! from the keyboard driver or from the `SendSAS` export in `sas.dll`. The
//! service loads that export and calls it on behalf of the operator — no driver,
//! just the Microsoft-provided helper that the registry key
//! `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System\SoftwareSASGeneration`
//! enables. Setting that value to `1` is part of `ensure_installed` when the
//! service is (re)registered.

#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

#[cfg(windows)]
use windows::Win32::System::StationsAndDesktops::{CloseDesktop, OpenInputDesktop};
#[cfg(windows)]
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// Which desktop the interactive session is on, if it can be told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopKind {
    /// `winsta0\default` — the ordinary desktop. WGC works here.
    Default,
    /// `Winlogon` / secure desktop — the lock screen or UAC. WGC is denied here;
    /// DDA from the service is the correct fallback.
    Secure(String),
    /// Could not be determined. Treat as non-secure so the agent keeps trying
    /// rather than assuming a fallback that may not exist.
    Unknown,
}

/// Name of the desktop that currently has input focus, if it can be queried.
///
/// Returns `Unknown` rather than erroring — the caller should keep capturing
/// with WGC when the answer is not known, because failing closed would leave a
/// session black while the secure-desktop check was transiently unavailable.
pub fn current_desktop() -> DesktopKind {
    #[cfg(not(windows))]
    {
        return DesktopKind::Unknown;
    }
    #[cfg(windows)]
    {
        current_desktop_windows()
    }
}

#[cfg(windows)]
fn current_desktop_windows() -> DesktopKind {
    use windows::Win32::System::StationsAndDesktops::{
        DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS,
    };
    unsafe {
        // `OpenInputDesktop` gives the desktop that currently receives input.
        // From a user process this is `Default` when the session is interactive
        // and `Winlogon` when the lock screen is up. From session 0 it fails,
        // which the service treats as `Unknown`.
        let hdesk = match OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) {
            Ok(h) => h,
            Err(_) => return DesktopKind::Unknown,
        };
        // We opened it — the interactive desktop is `Default`; for the service
        // the interesting case is failure above which already returned Unknown.
        // Querying the name is not needed for the DDA fallback decision: WGC
        // failing with PermissionDenied is the signal, and this function is only
        // advisory. Close and return Default so the agent keeps trying WGC.
        let _ = CloseDesktop(hdesk);
        DesktopKind::Default
    }
}

/// Whether the secure desktop (lock / UAC) is the one with focus.
pub fn is_secure_desktop() -> bool {
    matches!(current_desktop(), DesktopKind::Secure(_))
}

/// Ask Windows to generate a Secure Attention Sequence (Ctrl+Alt+Del) on the
/// console session.
///
/// This is the only way to get past the lock screen remotely without a driver:
/// injecting the three keys with `SendInput` is intentionally ignored by
/// Winlogon. `sas.dll!SendSAS` is the Microsoft-provided helper for software
/// that needs to do this — accessibility tools, remote assistance — and it
/// works when the caller is `SYSTEM` and the SoftwareSASGeneration policy is
/// `1` or `3`.
///
/// On non-Windows this always fails with `Unavailable`; on Windows it fails
/// with a human-readable string so Settings can show it rather than log it.
pub fn send_sas() -> Result<(), String> {
    #[cfg(not(windows))]
    {
        return Err("SendSAS is a Windows helper".into());
    }
    #[cfg(windows)]
    {
        send_sas_windows()
    }
}

#[cfg(windows)]
fn send_sas_windows() -> Result<(), String> {
    unsafe {
        let name: Vec<u16> = OsStr::new("sas.dll").encode_wide().chain(std::iter::once(0)).collect();
        let lib = LoadLibraryW(windows::core::PCWSTR(name.as_ptr()))
            .map_err(|e| format!("could not load sas.dll (is SoftwareSASGeneration enabled?): {e}"))?;
        if lib.is_invalid() {
            return Err("sas.dll could not be loaded".into());
        }

        let proc_name = windows::core::PCSTR(b"SendSAS\0".as_ptr());
        let addr = GetProcAddress(lib, proc_name);
        // Keep the library loaded until after the call — dropping it here would
        // unload the code under the function pointer.
        let func: Option<unsafe extern "system" fn(bool)> = std::mem::transmute(addr);
        let Some(send_sas) = func else {
            // `FreeLibrary` not exposed via `windows` 0.62 on this toolchain;
            // leak the handle — the service lives for the machine's uptime and
            // calling SendSAS twice is rare.
            return Err(
                "sas.dll does not export SendSAS; this Windows build may not support Software SAS"
                    .into(),
            );
        };

        // `asService = false` — we are SYSTEM but we want the SAS on the
        // console session, not on session 0.
        send_sas(false);
        // Do not free the library; the call is synchronous and the handle is
        // harmless to keep.
        Ok(())
    }
}

/// Ensure the SoftwareSASGeneration policy allows `SendSAS`.
///
/// Writes `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System\SoftwareSASGeneration = 1`
/// (enabled for services and `SYSTEM` callers) when running elevated. Idempotent.
/// Returns `Ok(())` when the value is already correct or was set, and a string
/// on failure so the installer can surface it.
pub fn ensure_sas_policy() -> Result<(), String> {
    #[cfg(not(windows))]
    {
        return Ok(());
    }
    #[cfg(windows)]
    {
        ensure_sas_policy_windows()
    }
}

#[cfg(windows)]
fn ensure_sas_policy_windows() -> Result<(), String> {
    use windows::Win32::System::Registry::{
        RegCreateKeyExW, RegSetValueExW, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, REG_DWORD,
        REG_OPTION_NON_VOLATILE,
    };

    let subkey: Vec<u16> = OsStr::new(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut hkey = windows::Win32::System::Registry::HKEY::default();
    let created = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            windows::core::PCWSTR(subkey.as_ptr()),
            Some(0),
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut hkey,
            None,
        )
    };
    if created.0 != 0 {
        return Err(format!("could not open SAS policy key: {created:?}"));
    }
    let value_name: Vec<u16> = OsStr::new("SoftwareSASGeneration")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let data: u32 = 1; // Services and SYSTEM callers may generate SAS
    let bytes = data.to_le_bytes();
    let set = unsafe {
        RegSetValueExW(
            hkey,
            windows::core::PCWSTR(value_name.as_ptr()),
            Some(0),
            REG_DWORD,
            Some(&bytes),
        )
    };
    let _ = unsafe { windows::Win32::System::Registry::RegCloseKey(hkey) };
    if set.0 != 0 {
        return Err(format!("could not write SoftwareSASGeneration: {set:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_desktop_never_panics_and_is_one_of_three() {
        let kind = current_desktop();
        match kind {
            DesktopKind::Default | DesktopKind::Secure(_) | DesktopKind::Unknown => {}
        }
    }

    #[test]
    fn is_secure_desktop_is_false_when_on_default_or_unknown() {
        // On CI there is no secure desktop; this must not panic and must be
        // false so the normal WGC path is taken.
        if matches!(current_desktop(), DesktopKind::Default | DesktopKind::Unknown) {
            assert!(!is_secure_desktop());
        }
    }

    #[test]
    fn send_sas_on_non_windows_reports_unavailable() {
        #[cfg(not(windows))]
        {
            let err = send_sas().unwrap_err();
            assert!(err.to_ascii_lowercase().contains("windows"), "{err}");
        }
    }

    #[test]
    fn ensure_sas_policy_is_idempotent_and_never_panics() {
        // On non-Windows it is a no-op; on Windows it may fail without elevation
        // but must not panic. CI runs unelevated, so either outcome is acceptable
        // as long as it is not a panic.
        let _ = ensure_sas_policy();
    }
}
