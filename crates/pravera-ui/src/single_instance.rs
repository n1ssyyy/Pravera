//! Only one Pravera at a time.
//!
//! A second launch while the first is hidden in the tray must not start a new
//! process — it must focus the window that is already there. `iced`/`winit`
//! give every window the same class, so the second process cannot just
//! `FindWindow("Pravera")` and be done: the title is `Pravera` or
//! `Pravera — DEVICE`, and a hidden window (`Mode::Hidden`) is still a window
//! but `ShowWindow` from another process leaves the first's `visible` flag
//! false so it stops rendering.
//!
//! Two mechanisms, both cheap:
//!
//! 1. **Port lock** — the first process binds `127.0.0.1:47901`. A second that
//!    fails to bind knows a first exists. The bound `TcpListener` is held in
//!    `Pravera` for the lifetime of the process so the port stays claimed.
//! 2. **Flag file** — the second writes `data_dir/pravera.show`; the first
//!    polls it every 500 ms via `Message::SingleInstanceCheck` and calls its
//!    own `show_window()` (which flips `visible` and does `set_mode(Windowed)`
//!    + `gain_focus`). A direct `ShowWindow`/`SetForegroundWindow` via Win32 is
//!    also attempted from the second for the one frame before the poll fires.

use std::net::TcpListener;
use std::path::PathBuf;

const FLAG: &str = "pravera.show";

fn addr() -> String {
    format!("127.0.0.1:{}", pravera_core::lifecycle::SINGLE_INSTANCE_PORT)
}

/// All locations the show-flag may live.
///
/// The first is `C:\ProgramData\Pravera\pravera.show` — visible to both
/// `SYSTEM` (the service's agent in session 0) and `Everc` (the user's
/// manual launch). The second is the per-user `data_dir` fallback for the
/// unelevated `asInvoker` case where `ProgramData\Pravera` is `SY+BA` only
/// and a non-admin cannot write there. The second instance writes to every
/// location it can; the first polls every location. That covers headless
/// (both elevated → shared succeeds) and desktop (unelevated → per-user
/// succeeds) with no split.
fn flag_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    // Shared, machine-wide — preferred when writable.
    let shared = pravera_core::paths::service_data_dir().join(FLAG);
    paths.push(shared);
    // Per-user fallback — always writable by this user.
    if let Ok(dir) = pravera_core::paths::data_dir() {
        let per_user = dir.join(FLAG);
        if !paths.contains(&per_user) {
            paths.push(per_user);
        }
    }
    paths
}

#[allow(dead_code)]
fn flag_path() -> Option<PathBuf> {
    flag_paths().into_iter().next()
}

fn ensure_flag_dirs() {
    for path in flag_paths() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
}

/// Whether the single-instance port is held by another Pravera.
///
/// Only `AddrInUse` counts. Any other bind failure (firewall, `WSAEACCES`,
/// exhausted ports) is *not* another instance — treating it as held made the
/// UI exit `7` with no first running, and made the service never launch.
pub fn port_held_by_other() -> bool {
    match TcpListener::bind(addr()) {
        Ok(l) => {
            drop(l);
            false
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => true,
        Err(e) => {
            tracing::warn!(%e, "single-instance port probe failed; assuming no other instance");
            false
        }
    }
}

/// Try to become the single instance. `Some(listener)` means we are the first
/// and must keep the listener alive; `None` means another Pravera already holds
/// the port. Clears any stale `pravera.show` left from a prior crash so the
/// first does not spuriously replay `entrance` (black→slide) right after hide.
pub fn try_acquire() -> Option<TcpListener> {
    let listener = match TcpListener::bind(addr()) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => return None,
        Err(e) => {
            // Not another instance — run as first without holding 47901
            // rather than exiting `7` with nobody to hand off to. Hold an
            // ephemeral port so the return type still guarantees lifetime.
            tracing::warn!(%e, "single-instance bind failed; running without lock");
            match TcpListener::bind("127.0.0.1:0") {
                Ok(dummy) => {
                    clear_stale_flags();
                    return Some(dummy);
                }
                Err(e2) => {
                    tracing::warn!(%e2, "even ephemeral bind failed; running lockless");
                    // No socket at all, but we are still first. Return None
                    // would mean "second" to the caller, so instead return a
                    // listener on a best-effort basis: try once more on
                    // 127.0.0.1:0 with reuse, else give up the lock but still
                    // run. The caller distinguishes via `port_held_by_other`.
                    // To keep the type simple, just return None here and let
                    // the caller check `port_held_by_other()` — updated call
                    // sites do that. Legacy callers treat None as second, so
                    // this path is only hit when networking is broken.
                    return None;
                }
            }
        }
    };
    // Stale flag would make the first's next `SingleInstanceCheck` (500ms) call
    // `show_window` while `visible==false`/`hiding==true` — the exact
    // black→slide-in the user saw after X. Clear only flags older than 2s:
    // a concurrent second writing *right now* (dual launch at logon) must
    // survive, otherwise the double-click appears to do nothing.
    clear_stale_flags();
    Some(listener)
}

/// Clear only stale flags (older than 2s). A fresh flag is a live request
/// from a concurrent second and must survive startup.
fn clear_stale_flags() {
    const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(2);
    for path in flag_paths() {
        let stale = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().unwrap_or(STALE_AFTER) >= STALE_AFTER)
            .unwrap_or(true);
        if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
    // A quit request is only ever meant for a Pravera that was already
    // running when it was written; the one starting now is its replacement.
    for path in quit_paths() {
        let _ = std::fs::remove_file(&path);
    }
}

/// Second instance: tell the first to show itself and then exit.
///
/// No direct `ShowWindow` — that would make the OS window visible while
/// `Pravera.visible==false`/`hiding==true`, and the first's next 500ms poll
/// would then call `show_window` and replay `entrance` 0→1 (black→slide)
/// on top of an already-visible window — the second slide-in. The flag alone
/// is the single source of truth.
pub fn notify_first_and_exit() -> ! {
    notify_first();
    std::process::exit(pravera_core::lifecycle::EXIT_ALREADY_RUNNING);
}

/// Write the show-flag without exiting. Shared by `notify_first_and_exit`.
pub fn notify_first() {
    ensure_flag_dirs();
    let mut wrote_one = false;
    for path in flag_paths() {
        if std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&path)
            .is_ok()
        {
            wrote_one = true;
        }
    }
    // Best-effort: even if neither write succeeded the port already tells the
    // first we tried — it will still be the single instance until it exits.
    let _ = wrote_one;
}

/// Second instance that must NOT wake the first: the service agent losing the
/// port race to the user's own Pravera. Writing `pravera.show` here would make
/// the first's next 500ms poll call `show_window` + `gain_focus` — a
/// focus-steal at logon/boot the user never asked for. Silent `exit(7)` lets
/// the service stand down (`service.rs` treats `7` as "already covered").
pub fn exit_silently_as_second() -> ! {
    std::process::exit(pravera_core::lifecycle::EXIT_ALREADY_RUNNING);
}

/// The other request a second process can make: go away. Setup writes it
/// before it replaces or removes the executable, and the first answers on the
/// same 500 ms poll as the show-flag. A service agent that quits this way is
/// started again by the service a few seconds later — from the new file, which
/// is the point.
const QUIT_FLAG: &str = "pravera.quit";

fn quit_paths() -> Vec<PathBuf> {
    flag_paths()
        .into_iter()
        .filter_map(|path| path.parent().map(|dir| dir.join(QUIT_FLAG)))
        .collect()
}

/// Ask the running Pravera to quit, and wait up to `patience` for the port to
/// come free. `true` means nothing holds it any more.
pub fn ask_first_to_quit(patience: std::time::Duration) -> bool {
    if !port_held_by_other() {
        return true;
    }
    ensure_flag_dirs();
    for path in quit_paths() {
        let _ = std::fs::write(&path, b"");
    }
    let deadline = std::time::Instant::now() + patience;
    let gone = loop {
        if !port_held_by_other() {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    };
    // Whether it listened or not, a request left lying around would close the
    // next Pravera somebody starts.
    for path in quit_paths() {
        let _ = std::fs::remove_file(&path);
    }
    gone
}

/// First instance: poll hook for [`ask_first_to_quit`].
pub fn check_quit_request() -> bool {
    let mut found = false;
    for path in quit_paths() {
        if path.exists() {
            let _ = std::fs::remove_file(&path);
            found = true;
        }
    }
    found
}

/// Wait for a Pravera that is on its way out to let go of the port, so the one
/// replacing it after an update does not mistake it for a first instance and
/// defer to it. Gives up after `patience` and carries on regardless.
pub fn wait_for_port(patience: std::time::Duration) {
    let deadline = std::time::Instant::now() + patience;
    while port_held_by_other() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// First instance: poll hook. `true` means a second instance asked us to show.
pub fn check_and_clear_flag() -> bool {
    let mut found = false;
    for path in flag_paths() {
        if path.exists() {
            let _ = std::fs::remove_file(&path);
            found = true;
        }
    }
    found
}

/// Hide *this* process's window synchronously via Win32, so the OS hides it
/// on this frame, not one frame later via the async `set_mode(Hidden)` task.
/// Without this the `view`'s last frame (still at `present` 0.55 dim) is
/// visible for one compositing interval as "fully black".
///
/// PID-scoped: only windows owned by this process are hidden. The old
/// title-only match could hide the *other* Pravera's window when agent and
/// user coexisted briefly (same title `Pravera` / `Pravera — host`).
#[cfg(windows)]
pub fn hide_os_window() {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::core::BOOL;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, GetWindowThreadProcessId, ShowWindow, SW_HIDE,
    };
    let own_pid = std::process::id();
    unsafe extern "system" fn enum_hide(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let want_pid = lparam.0 as u32;
        let mut pid = 0u32;
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != want_pid {
            return BOOL(1);
        }
        let mut buf = [0u16; 512];
        let len = GetWindowTextW(hwnd, &mut buf);
        if len == 0 {
            return BOOL(1);
        }
        let title = String::from_utf16_lossy(&buf[..len as usize]);
        if title == "Pravera" || title.starts_with("Pravera —") || title.starts_with("Pravera -") {
            let _ = ShowWindow(hwnd, SW_HIDE);
            return BOOL(0);
        }
        BOOL(1)
    }
    unsafe {
        let _ = EnumWindows(Some(enum_hide), LPARAM(own_pid as isize));
    }
}

#[cfg(not(windows))]
pub fn hide_os_window() {}

#[cfg(windows)]
#[allow(dead_code)]
fn show_existing_window() {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::core::BOOL;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, IsWindowVisible, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };

    unsafe extern "system" fn enum_proc(hwnd: HWND, _: LPARAM) -> BOOL {
        // Hidden windows are still windows, but we only want Pravera's.
        // `GetWindowTextW` works even for hidden top-levels.
        let mut buf = [0u16; 512];
        let len = GetWindowTextW(hwnd, &mut buf);
        if len == 0 {
            return BOOL(1);
        }
        let title = String::from_utf16_lossy(&buf[..len as usize]);
        if title == "Pravera" || title.starts_with("Pravera —") || title.starts_with("Pravera -") {
            // `IsWindowVisible` is false for Mode::Hidden, but ShowWindow will
            // still bring it back — that is exactly the case we are handling.
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
            // Also try to bring to top; not all foreground rules allow it, but
            // the poll in the first process will do the iced-level show next
            // tick and that always works.
            let _ = IsWindowVisible(hwnd); // keep import used
            // Stop enumeration once we found it.
            return BOOL(0);
        }
        BOOL(1)
    }

    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(0));
    }
}

#[cfg(not(windows))]
fn show_existing_window() {}
