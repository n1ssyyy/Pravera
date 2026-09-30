//! Registering, removing and prodding the service, through the Service
//! Control Manager.
//!
//! Every one of these needs an elevated prompt. Rather than checking for
//! elevation up front and guessing, each call reports what the SCM actually
//! said — `ERROR_ACCESS_DENIED` is turned into the one sentence that helps,
//! and everything else is passed through with its own wording, because a
//! service that fails to install for an unusual reason should say the unusual
//! reason.

use anyhow::{anyhow, Context, Result};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::ERROR_ACCESS_DENIED;
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, CloseServiceHandle, ControlService, CreateServiceW, DeleteService,
    OpenSCManagerW, OpenServiceW, QueryServiceStatus, StartServiceW, SC_HANDLE,
    SC_MANAGER_ALL_ACCESS, SC_MANAGER_CONNECT, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
    SERVICE_CONFIG_DESCRIPTION, SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL,
    SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START, SERVICE_STATUS, SERVICE_STOP,
    SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS,
};
use windows::Win32::System::Services::{
    ChangeServiceConfigW, QueryServiceConfigW, ENUM_SERVICE_TYPE, QUERY_SERVICE_CONFIGW,
    SERVICE_ERROR, SERVICE_NO_CHANGE,
};

use crate::{Installed, DESCRIPTION, DISPLAY_NAME, SERVICE_NAME};

/// A service handle that closes itself.
///
/// The SCM leaks quietly rather than loudly when a handle is dropped on the
/// floor, which is the kind of bug nobody finds. This makes it impossible.
struct Handle(SC_HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // The only documented failure is an invalid handle, which cannot
        // happen here: this type is only ever built from a successful open.
        unsafe { CloseServiceHandle(self.0) }.ok();
    }
}

/// Open the SCM with the access a particular job needs.
fn manager(access: u32) -> Result<Handle> {
    unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), access) }
        .map(Handle)
        .map_err(explain("open the service manager"))
}

/// Open the Pravera service itself.
fn service(manager: &Handle, access: u32) -> Result<Handle> {
    unsafe { OpenServiceW(manager.0, &HSTRING::from(SERVICE_NAME), access) }
        .map(Handle)
        .map_err(explain("open the Pravera service"))
}

/// Turn a Windows error into something worth reading.
fn explain(what: &'static str) -> impl Fn(windows::core::Error) -> anyhow::Error {
    move |error| {
        if error.code() == ERROR_ACCESS_DENIED.to_hresult() {
            anyhow!(
                "Could not {what}: this needs an elevated prompt. Open Terminal or Command \
                 Prompt with \"Run as administrator\" and try again."
            )
        } else {
            anyhow!("Could not {what}: {error}")
        }
    }
}

/// Where this executable is, quoted for a service command line.
///
/// Quoted because an unquoted path with a space in it is the oldest Windows
/// service vulnerability there is: the SCM would try `C:\Program.exe` before
/// `C:\Program Files\Pravera\pravera-service.exe`, and anybody who could write
/// to the root of the drive would be running code as LocalSystem.
fn command_line() -> Result<String> {
    let exe = std::env::current_exe().context("Pravera could not find its own executable")?;
    Ok(format!("\"{}\" {}", exe.display(), crate::SERVICE_FLAG))
}

/// Register the service, or point an existing registration at this file.
///
/// The reconciliation is the point. A portable executable gets moved, and a
/// service still pointing at the folder it used to be in fails to start with
/// an error nobody reads. Comparing the registered command line against the
/// real one on every launch costs three SCM calls and makes moving the file a
/// non-event.
pub fn ensure_installed() -> Installed {
    let Ok(wanted) = command_line() else {
        return Installed::Refused("Pravera could not find its own executable.".into());
    };

    // Opened with the access needed to *change* things. Failing here is the
    // ordinary case on a machine where Pravera was not run as administrator,
    // and it is reported as such rather than as a fault.
    let Ok(manager) = manager(SC_MANAGER_ALL_ACCESS) else {
        return Installed::NotElevated;
    };

    let Ok(handle) = service(&manager, SERVICE_ALL_ACCESS) else {
        return match install_with(&manager, &wanted) {
            Ok(()) => {
                let _ = crate::secure::ensure_sas_policy();
                let _ = start();
                Installed::Registered
            }
            Err(error) => Installed::Refused(format!("{error:#}")),
        };
    };

    match registered_command(&handle) {
        // Already ours, already right. By far the common case.
        Some(current) if current.eq_ignore_ascii_case(&wanted) => {
            // Ensure the SAS policy is still set: the service relies on it for
            // Ctrl+Alt+Del on the lock screen, and a group policy or manual
            // edit may have cleared it since install.
            let _ = crate::secure::ensure_sas_policy();
            // Registered but not running is worth fixing quietly: it is what a
            // machine looks like after somebody stopped the service to test
            // something and forgot.
            let _ = start();
            Installed::Unchanged
        }
        _ => match repoint(&handle, &wanted) {
            Ok(()) => {
                let _ = crate::secure::ensure_sas_policy();
                let _ = stop();
                let _ = start();
                Installed::Repointed
            }
            Err(error) => Installed::Refused(format!("{error:#}")),
        },
    }
}

/// What the service manager says, asked without changing anything.
///
/// Opening the manager and the service for a query needs no elevation, so an
/// ordinary Pravera gets a true answer. [`ensure_installed`] cannot give one:
/// it opens everything for writing, which an unelevated process is refused,
/// and it reports that refusal as "not elevated" even when the service is
/// registered and correct.
pub fn current() -> Installed {
    let Ok(wanted) = command_line() else {
        return Installed::Refused("Pravera could not find its own executable.".into());
    };
    let Ok(manager) = manager(SC_MANAGER_CONNECT) else {
        return Installed::Refused("Pravera could not ask the service manager.".into());
    };
    let Ok(handle) = service(
        &manager,
        SERVICE_QUERY_STATUS | windows::Win32::System::Services::SERVICE_QUERY_CONFIG,
    ) else {
        return Installed::NotRegistered;
    };
    match registered_command(&handle) {
        Some(command) if command.eq_ignore_ascii_case(&wanted) => Installed::Unchanged,
        _ => Installed::Elsewhere,
    }
}

/// The command line the service is currently registered with.
fn registered_command(handle: &Handle) -> Option<String> {
    // Asked for its size first: the config is a variable-length structure with
    // the strings packed in behind it, so there is no fixed buffer to use.
    let mut needed = 0u32;
    let _ = unsafe { QueryServiceConfigW(handle.0, None, 0, &mut needed) };
    if needed == 0 {
        return None;
    }

    let mut buffer = vec![0u8; needed as usize];
    let config = buffer.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>();
    unsafe { QueryServiceConfigW(handle.0, Some(config), needed, &mut needed) }.ok()?;

    let path = unsafe { (*config).lpBinaryPathName };
    if path.is_null() {
        return None;
    }
    unsafe { path.to_string() }.ok()
}

/// Point an existing registration at a different command line.
fn repoint(handle: &Handle, command: &str) -> Result<()> {
    unsafe {
        ChangeServiceConfigW(
            handle.0,
            // Only the command line and the start type are being set; the
            // rest keep whatever they already had.
            ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
            SERVICE_AUTO_START,
            SERVICE_ERROR(SERVICE_NO_CHANGE),
            &HSTRING::from(command),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
        )
    }
    .map_err(explain("point the Pravera service at this copy of Pravera"))
}

/// Create the registration.
///
/// Split out from [`ensure_installed`] so the two callers cannot drift apart
/// in what they actually register.
fn install_with(manager: &Handle, command: &str) -> Result<()> {
    let handle = unsafe {
        CreateServiceW(
            manager.0,
            &HSTRING::from(SERVICE_NAME),
            &HSTRING::from(DISPLAY_NAME),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            // The whole point: started by Windows at boot, with nobody
            // logged in and nobody to click anything.
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            &HSTRING::from(command),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            // Null account means LocalSystem, which is what is needed to
            // duplicate another session's token and launch into it.
            PCWSTR::null(),
            PCWSTR::null(),
        )
    }
    .map(Handle)
    .map_err(explain("register the Pravera service"))?;

    // Cosmetic, and worth doing: an unexplained entry in the Services list is
    // exactly the kind of thing a careful person removes.
    let text = HSTRING::from(DESCRIPTION);
    let mut description = SERVICE_DESCRIPTIONW {
        lpDescription: windows::core::PWSTR(text.as_ptr() as *mut u16),
    };
    unsafe {
        ChangeServiceConfig2W(
            handle.0,
            SERVICE_CONFIG_DESCRIPTION,
            Some(std::ptr::from_mut(&mut description).cast()),
        )
    }
    .ok();

    Ok(())
}

pub fn uninstall() -> Result<String> {
    // Stopping first, because deleting a running service only marks it for
    // deletion and it stays until the next reboot — which looks exactly like
    // the uninstall not having worked.
    let stopped = stop().is_ok();

    let manager = manager(SC_MANAGER_ALL_ACCESS)?;
    let handle = service(&manager, SERVICE_ALL_ACCESS)?;

    unsafe { DeleteService(handle.0) }.map_err(explain("remove the Pravera service"))?;

    Ok(if stopped {
        format!("Stopped and removed \"{DISPLAY_NAME}\".")
    } else {
        format!("Removed \"{DISPLAY_NAME}\".")
    })
}

pub fn start() -> Result<String> {
    let manager = manager(SC_MANAGER_CONNECT)?;
    let handle = service(&manager, SERVICE_START | SERVICE_QUERY_STATUS)?;

    unsafe { StartServiceW(handle.0, None) }.map_err(explain("start the Pravera service"))?;
    Ok(format!("\"{DISPLAY_NAME}\" is starting."))
}

pub fn stop() -> Result<String> {
    let manager = manager(SC_MANAGER_CONNECT)?;
    let handle = service(&manager, SERVICE_STOP | SERVICE_QUERY_STATUS)?;

    let mut status = SERVICE_STATUS::default();
    unsafe { ControlService(handle.0, SERVICE_CONTROL_STOP, &mut status) }
        .map_err(explain("stop the Pravera service"))?;

    Ok(format!("\"{DISPLAY_NAME}\" is stopping."))
}

/// Whether the service exists at all, as far as this process may ask. Opening
/// it for a status query needs no elevation, so an ordinary user gets a true
/// answer.
pub fn is_registered() -> bool {
    manager(SC_MANAGER_CONNECT)
        .and_then(|manager| service(&manager, SERVICE_QUERY_STATUS).map(|_| ()))
        .is_ok()
}

/// Whether the registered service runs `exe`, so removing one copy of
/// Pravera never unregisters the service another copy set up.
pub fn registered_for(exe: &std::path::Path) -> bool {
    let wanted = exe.display().to_string().to_lowercase();
    manager(SC_MANAGER_CONNECT)
        .and_then(|manager| service(&manager, windows::Win32::System::Services::SERVICE_QUERY_CONFIG))
        .ok()
        .and_then(|handle| registered_command(&handle))
        .is_some_and(|command| command.to_lowercase().contains(&wanted))
}

pub fn status() -> Result<String> {
    let manager = manager(SC_MANAGER_CONNECT)?;

    // Not registered is an answer, not a failure: it is the answer somebody
    // running `status` before `install` is expecting to get.
    let Ok(handle) = service(&manager, SERVICE_QUERY_STATUS) else {
        return Ok(format!(
            "\"{DISPLAY_NAME}\" is not registered.\nRegister it with: pravera-service install"
        ));
    };

    let mut current = SERVICE_STATUS::default();
    unsafe { QueryServiceStatus(handle.0, &mut current) }
        .map_err(explain("read the Pravera service's state"))?;

    let state = match current.dwCurrentState {
        SERVICE_RUNNING => "running",
        SERVICE_STOPPED => "registered but stopped",
        _ => "changing state",
    };
    Ok(format!("\"{DISPLAY_NAME}\" is {state}."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_service_command_line_is_quoted() {
        // An unquoted path with a space is a privilege escalation, not a
        // cosmetic problem. See `command_line`.
        let command = command_line().expect("this executable exists");
        assert!(command.starts_with('"'), "got {command}");
        assert!(
            command.ends_with(&format!("\" {}", crate::SERVICE_FLAG)),
            "got {command}"
        );
    }
}
