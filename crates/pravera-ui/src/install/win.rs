//! Windows: Start menu and desktop shortcuts, and the entry in Settings →
//! Apps that makes Pravera uninstallable like anything else.
//!
//! All of it under the current user — `HKCU` and the user's own known
//! folders — so installing never raises a UAC prompt.

use std::path::{Path, PathBuf};

use windows::core::{Interface, GUID, HSTRING, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IPersistFile,
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegGetValueW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Programs, IShellLinkW, SHGetKnownFolderPath, ShellLink,
    KF_FLAG_DEFAULT,
};

use super::{Layout, Report, HOMEPAGE, PUBLISHER, VERSION};

/// Where Settings → Apps looks for per-user programs.
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Pravera";
const SHORTCUT: &str = "Pravera.lnk";
const DESCRIPTION: &str = "Peer-to-peer remote desktop";
/// The temporary copy an uninstall runs from, so the real file can be deleted.
const TEMP_UNINSTALLER: &str = "pravera-uninstall-";

pub fn integrate(layout: &Layout, desktop: bool, report: &mut Report) {
    match known_folder(&FOLDERID_Programs) {
        Some(programs) => match shortcut(&programs.join(SHORTCUT), &layout.exe) {
            Ok(()) => report.steps.push("Added Pravera to the Start menu".into()),
            Err(error) => report
                .warnings
                .push(format!("Could not add the Start menu shortcut: {error}")),
        },
        None => report
            .warnings
            .push("Could not find the Start menu folder".into()),
    }

    if let Some(folder) = known_folder(&FOLDERID_Desktop) {
        let link = folder.join(SHORTCUT);
        if desktop {
            match shortcut(&link, &layout.exe) {
                Ok(()) => report.steps.push("Added a desktop shortcut".into()),
                Err(error) => report
                    .warnings
                    .push(format!("Could not add the desktop shortcut: {error}")),
            }
        } else {
            // Unticked on an update means "take it away", not "leave it".
            let _ = std::fs::remove_file(link);
        }
    }

    match uninstall_entry(layout) {
        Ok(()) => report
            .steps
            .push("Listed Pravera in Settings → Apps".into()),
        Err(error) => report
            .warnings
            .push(format!("Could not list Pravera in Settings → Apps: {error}")),
    }
}

pub fn disintegrate(report: &mut Report) {
    let mut removed = false;
    for folder in [known_folder(&FOLDERID_Programs), known_folder(&FOLDERID_Desktop)]
        .into_iter()
        .flatten()
    {
        removed |= std::fs::remove_file(folder.join(SHORTCUT)).is_ok();
    }
    if removed {
        report.steps.push("Removed the shortcuts".into());
    }
    let key = HSTRING::from(UNINSTALL_KEY);
    if unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &key) } == ERROR_SUCCESS {
        report
            .steps
            .push("Removed Pravera from Settings → Apps".into());
    }
}

/// Where the last install said it was.
pub fn registered_dir() -> Option<PathBuf> {
    let key = HSTRING::from(UNINSTALL_KEY);
    let value = HSTRING::from("InstallLocation");
    let mut buffer = [0u16; 1024];
    let mut size = std::mem::size_of_val(&buffer) as u32;
    // SAFETY: `size` is the byte length of `buffer`, as the call expects.
    let read = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &key,
            &value,
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if read != ERROR_SUCCESS {
        return None;
    }
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    let dir = String::from_utf16_lossy(&buffer[..end]);
    (!dir.trim().is_empty()).then(|| PathBuf::from(dir.trim()))
}

fn uninstall_entry(layout: &Layout) -> Result<(), String> {
    let mut key = HKEY::default();
    let path = HSTRING::from(UNINSTALL_KEY);
    // SAFETY: `key` receives the opened handle and is closed below.
    let created = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &path,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
    };
    if created != ERROR_SUCCESS {
        return Err(format!("the registry refused ({})", created.0));
    }

    let exe = layout.exe.display().to_string();
    let size_kb = std::fs::metadata(&layout.exe)
        .map(|meta| (meta.len() / 1024) as u32)
        .unwrap_or(0);
    let strings = [
        ("DisplayName", "Pravera".to_string()),
        ("DisplayVersion", VERSION.to_string()),
        ("Publisher", PUBLISHER.to_string()),
        ("DisplayIcon", format!("{exe},0")),
        ("InstallLocation", layout.dir.display().to_string()),
        ("UninstallString", format!("\"{exe}\" --uninstall")),
        ("QuietUninstallString", format!("\"{exe}\" --uninstall --quiet")),
        ("URLInfoAbout", HOMEPAGE.to_string()),
        ("HelpLink", format!("{HOMEPAGE}/issues")),
        ("URLUpdateInfo", format!("{HOMEPAGE}/releases")),
    ];
    let dwords = [("NoModify", 1u32), ("NoRepair", 1), ("EstimatedSize", size_kb)];

    let mut failed = None;
    for (name, value) in strings {
        let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: a `u16` slice viewed as its bytes, alive for the call.
        let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2) };
        let set = unsafe { RegSetValueExW(key, &HSTRING::from(name), None, REG_SZ, Some(bytes)) };
        if set != ERROR_SUCCESS {
            failed = Some(name);
        }
    }
    for (name, value) in dwords {
        let bytes = value.to_le_bytes();
        let set = unsafe { RegSetValueExW(key, &HSTRING::from(name), None, REG_DWORD, Some(&bytes)) };
        if set != ERROR_SUCCESS {
            failed = Some(name);
        }
    }
    unsafe {
        let _ = RegCloseKey(key);
    }
    match failed {
        Some(name) => Err(format!("could not write {name}")),
        None => Ok(()),
    }
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    // SAFETY: the returned string is owned by us and freed with the COM
    // allocator, as the call's contract requires.
    unsafe {
        let path = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let text = path.to_string().ok();
        CoTaskMemFree(Some(path.0 as *const _));
        text.map(PathBuf::from)
    }
}

fn shortcut(link: &Path, target: &Path) -> windows::core::Result<()> {
    // SAFETY: COM is initialised for this thread for the length of the call
    // and every interface is released before it is uninitialised.
    unsafe {
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let made = (|| {
            let shell: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            shell.SetPath(&HSTRING::from(target.as_os_str()))?;
            if let Some(dir) = target.parent() {
                shell.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
            }
            shell.SetDescription(&HSTRING::from(DESCRIPTION))?;
            shell.SetIconLocation(&HSTRING::from(target.as_os_str()), 0)?;
            let file: IPersistFile = shell.cast()?;
            file.Save(&HSTRING::from(link.as_os_str()), true)
        })();
        if init.is_ok() {
            CoUninitialize();
        }
        made
    }
}

/// Whether this process is the installed executable itself.
pub fn runs_from(layout: &Layout) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    same(&exe, &layout.exe)
}

/// Uninstalling from Settings → Apps runs the installed file, which cannot
/// delete itself. So it copies itself to the temp folder, starts the copy with
/// the same arguments, and steps aside; the copy does the work and then
/// arranges its own deletion ([`delete_self_if_temporary`]).
pub fn hand_off_uninstall() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let copy = std::env::temp_dir().join(format!("{TEMP_UNINSTALLER}{}.exe", std::process::id()));
    std::fs::copy(&exe, &copy).map_err(|error| format!("could not stage the uninstaller: {error}"))?;
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if !args.iter().any(|arg| arg == "--dir") {
        if let Some(dir) = exe.parent() {
            args.push("--dir".into());
            args.push(dir.display().to_string());
        }
    }
    std::process::Command::new(&copy)
        .args(&args)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not start the uninstaller: {error}"))
}

/// The temporary uninstaller cleaning up after itself, once it has exited.
pub fn delete_self_if_temporary() {
    let Ok(exe) = std::env::current_exe() else { return };
    let temporary = exe
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(TEMP_UNINSTALLER));
    if temporary {
        let _ = delete_later(&exe);
    }
}

/// Delete a file that is in use, as soon as it is not. A hidden `cmd` waits a
/// few seconds and deletes it; if it is still held then, the temp folder's own
/// cleanup has it eventually.
pub fn delete_later(file: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // A file still running elsewhere cannot be deleted, but it can be moved,
    // which lets its folder go now.
    let parked = if file.starts_with(std::env::temp_dir()) {
        file.to_path_buf()
    } else {
        let parked = std::env::temp_dir().join(format!(
            "pravera-removed-{}-{}",
            std::process::id(),
            file.file_name().and_then(|n| n.to_str()).unwrap_or("file")
        ));
        std::fs::rename(file, &parked)?;
        parked
    };
    std::process::Command::new("cmd.exe")
        .raw_arg(format!(
            "/C ping 127.0.0.1 -n 4 >NUL & del /F /Q \"{}\"",
            parked.display()
        ))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
}
