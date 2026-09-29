//! Secure Attention Sequence helper for the host.
//!
//! Duplicated from `pravera-service::secure` so the host crate does not have
//! to depend on the service crate — the host runs both in the agent (as the
//! user) and, on a headless box, potentially as `SYSTEM`, and only the second
//! one can actually deliver a SAS.

#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

/// Generate Ctrl+Alt+Del on the console, if this process can.
///
/// Returns `Ok(())` when `sas.dll!SendSAS` was called, and a human-readable
/// string otherwise so the host can log it and the control reply (`SasSent`)
/// still went out — the client was told the request was heard even if the
/// helper was not available.
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
        let name: Vec<u16> = OsStr::new("sas.dll")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let lib = windows::Win32::System::LibraryLoader::LoadLibraryW(windows::core::PCWSTR(
            name.as_ptr(),
        ))
        .map_err(|e| format!("could not load sas.dll (is SoftwareSASGeneration enabled?): {e}"))?;
        if lib.is_invalid() {
            return Err("sas.dll could not be loaded".into());
        }
        let proc_name = windows::core::PCSTR(b"SendSAS\0".as_ptr());
        let addr = windows::Win32::System::LibraryLoader::GetProcAddress(lib, proc_name);
        let func: Option<unsafe extern "system" fn(bool)> = std::mem::transmute(addr);
        let Some(send_sas) = func else {
            return Err(
                "sas.dll does not export SendSAS — this Windows build may not support Software SAS"
                    .into(),
            );
        };
        send_sas(false);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_sas_on_non_windows_reports_unavailable() {
        #[cfg(not(windows))]
        {
            let err = send_sas().unwrap_err();
            assert!(err.to_ascii_lowercase().contains("windows"), "{err}");
        }
    }
}
