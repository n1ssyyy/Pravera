//! Taking the keys Windows would otherwise keep for itself.
//!
//! Forwarding a key press needs the application to see it first, and for a
//! handful of keys Windows never lets that happen. Pressing the Windows key
//! opens *this* machine's Start menu; Alt+Tab switches *this* machine's
//! windows. The remote machine never hears about either, and no amount of
//! swallowing the event afterwards helps, because the shell has already acted.
//!
//! The fix is a `WH_KEYBOARD_LL` hook, which runs ahead of the shell and can
//! refuse to pass a key along. While a session holds the keyboard, the hook
//! takes those keys, queues them for forwarding, and tells Windows they never
//! happened. Everything else falls through untouched and reaches the window
//! the ordinary way — one path for one key, never both.
//!
//! # What this cannot take, and why
//!
//! **Ctrl+Alt+Delete and Win+L cannot be intercepted by any program.** They are
//! the Secure Attention Sequence, handled inside the kernel specifically so
//! that no application — including this one, and including anything pretending
//! to be a login screen — can see or fake them. That is a security property
//! worth having, not a gap to work around. Reaching the far machine's Ctrl+Alt+
//! Delete needs the session agent that P5 installs, which can ask the secure
//! desktop directly; there is a button for it rather than a key.
//!
//! # Why the state is global
//!
//! A hook procedure is a bare `extern "system" fn`: Windows calls it with no
//! way to carry a pointer along, so anything it touches has to be reachable
//! from a static. Both statics here are small, lock-free on the hot path, and
//! only ever touched while a hook is installed.

use pravera_proto::KeyCode;

/// A key the hook took, on its way to the far machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grabbed {
    pub code: KeyCode,
    pub pressed: bool,
}

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

    use std::sync::Mutex;
    use tracing::{debug, warn};
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT,
        LLKHF_INJECTED, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };

    use super::Grabbed;

    /// Whether the hook should be taking anything right now.
    ///
    /// Separate from whether the hook is installed: installing and removing on
    /// every focus change would mean a `SetWindowsHookExW` call every time
    /// somebody alt-tabbed, and a window that lost the race would drop keys.
    static GRABBING: AtomicBool = AtomicBool::new(false);

    /// The installed hook, as an integer because a raw handle is not `Sync`.
    /// Zero means nothing is installed.
    static HOOK: AtomicIsize = AtomicIsize::new(0);

    /// Keys taken, waiting to be forwarded.
    ///
    /// A plain `Vec` behind a lock rather than a channel: the hook runs on the
    /// same thread as the message pump that drains it, so this is never
    /// contended, and a lock that is never contended is cheaper than a channel
    /// that allocates.
    static TAKEN: Mutex<Vec<Grabbed>> = Mutex::new(Vec::new());

    /// The longest queue that will be held before keys start being dropped.
    ///
    /// Reached only if the interface has stopped draining, which means the
    /// session is already gone. Without a bound, a stuck window would grow this
    /// until the machine ran out of memory, with the keyboard still captured.
    const MAX_QUEUED: usize = 256;

    /// Virtual key codes the shell would eat, and what each is in HID terms.
    ///
    /// Deliberately short. Every key taken here is a key that stops working on
    /// this machine while a session is up, so the list holds only the ones
    /// that are otherwise impossible to send.
    const fn stolen(vk: u32) -> Option<u16> {
        match vk {
            0x5B => Some(0xE3), // Left Windows  -> Left GUI
            0x5C => Some(0xE7), // Right Windows -> Right GUI
            0x09 => Some(0x2B), // Tab, for Alt+Tab
            0x1B => Some(0x29), // Escape, for Ctrl+Esc and Alt+Esc
            0x46 => Some(0x3D), // F4, for Alt+F4
            _ => None,
        }
    }

    /// Whether this key, pressed now, is one the shell is about to act on.
    ///
    /// Tab and Escape are ordinary keys most of the time and must keep working
    /// as ordinary keys; only the combinations that trigger a shell hotkey are
    /// worth taking. The Windows key is always taken, because it does nothing
    /// else.
    fn is_shell_hotkey(vk: u32) -> bool {
        match vk {
            0x5B | 0x5C => true,
            // Alt+Tab. Reading the modifier here rather than tracking it
            // ourselves, because the hook may well have been installed while
            // Alt was already down.
            0x09 => held(0x12),
            // Ctrl+Esc opens Start; Alt+Esc cycles windows.
            0x1B => held(0x11) || held(0x12),
            // Alt+F4 asks this window to close. While the keyboard is held it
            // belongs to the far machine: closing *this* window instead of the
            // window the person is looking at is worse than not answering.
            // The session's own End button is the way out that always works.
            0x46 => held(0x12),
            _ => false,
        }
    }

    /// Whether a virtual key is down this instant.
    fn held(vk: i32) -> bool {
        // The high bit is "down now"; the low bit is "was pressed since the
        // last call", which is not the question being asked.
        (unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000) != 0
    }

    unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // A negative code means Windows is telling us to keep out of it.
        if code < 0 || !GRABBING.load(Ordering::Relaxed) {
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }

        let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };

        // Anything Pravera's own host half injected on this machine. Taking it
        // back would put a machine hosting and viewing at once into a loop.
        if event.flags.0 & LLKHF_INJECTED.0 != 0 {
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }

        let pressed = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        let released = matches!(wparam.0 as u32, WM_KEYUP | WM_SYSKEYUP);
        if !pressed && !released {
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }

        let Some(usage) = stolen(event.vkCode) else {
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        };

        // A release is taken whenever its press was, so the far machine never
        // ends up holding a key down forever. Checking the hotkey condition
        // again on release would fail exactly when the modifier was let go
        // first, which is the common case for Alt+Tab.
        if pressed && !is_shell_hotkey(event.vkCode) {
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }

        let Ok(mut queue) = TAKEN.lock() else {
            return LRESULT(1);
        };
        if queue.len() < MAX_QUEUED {
            queue.push(Grabbed {
                code: pravera_proto::KeyCode(usage),
                pressed,
            });
        }

        // Non-zero: the key stops here and this machine never sees it.
        LRESULT(1)
    }

    /// Start intercepting, installing the hook if it is not already there.
    ///
    /// Must be called from the thread running the message pump — the hook
    /// procedure is dispatched on the thread that installed it, and a thread
    /// that never pumps messages will silently never receive a callback.
    pub fn start() {
        if HOOK.load(Ordering::Relaxed) == 0 {
            match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), None, 0) } {
                Ok(handle) => {
                    HOOK.store(handle.0 as isize, Ordering::Relaxed);
                    debug!("holding the keys Windows would otherwise keep");
                }
                // Every other key still forwards; the shell keys keep going to
                // this machine. Worth a line in the log and nothing more.
                Err(error) => {
                    warn!(%error, "the Windows key cannot be forwarded on this machine");
                    return;
                }
            }
        }
        GRABBING.store(true, Ordering::Relaxed);
    }

    /// Stop intercepting and give the keys back.
    ///
    /// The hook is removed rather than just disarmed: a hook left installed is
    /// a callback on every keystroke on the machine for as long as the window
    /// is open, which is not a thing to leave running for a session that ended.
    pub fn stop() {
        GRABBING.store(false, Ordering::Relaxed);
        if let Ok(mut queue) = TAKEN.lock() {
            queue.clear();
        }

        let handle = HOOK.swap(0, Ordering::Relaxed);
        if handle != 0 {
            let _ = unsafe { UnhookWindowsHookEx(HHOOK(handle as *mut _)) };
        }
    }

    /// Everything taken since the last call.
    pub fn drain() -> Vec<Grabbed> {
        match TAKEN.lock() {
            Ok(mut queue) => std::mem::take(&mut *queue),
            Err(_) => Vec::new(),
        }
    }

    pub fn is_grabbing() -> bool {
        GRABBING.load(Ordering::Relaxed)
    }

    /// The interception table, for the test that guards its contents.
    #[cfg(test)]
    pub const fn stolen_for_test(vk: u32) -> Option<u16> {
        stolen(vk)
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Grabbed;

    // X11 and Wayland do not hand a single application the compositor's own
    // shortcuts, and asking for them is a compositor-specific negotiation
    // rather than one call. Until that exists, every key that reaches the
    // window still forwards; the ones the compositor claims stay local.
    pub fn start() {}
    pub fn stop() {}
    pub fn drain() -> Vec<Grabbed> {
        Vec::new()
    }
    pub const fn is_grabbing() -> bool {
        false
    }
}

pub use imp::{drain, is_grabbing, start, stop};

/// Whether this build can take the shell's keys at all.
///
/// Shown in the session so a person who presses the Windows key and watches
/// their own Start menu open is told why, rather than concluding Pravera drops
/// keys at random.
pub const fn available() -> bool {
    cfg!(windows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_held_before_anything_asked_for_it() {
        assert!(!is_grabbing());
        assert!(drain().is_empty());
    }

    #[test]
    fn stopping_without_starting_is_harmless() {
        // Called on every session teardown, including ones that never got as
        // far as taking the keyboard.
        stop();
        assert!(!is_grabbing());
    }

    #[cfg(windows)]
    #[test]
    fn the_keys_taken_are_the_ones_that_cannot_be_sent_any_other_way() {
        use super::imp;
        // Compiled against the same table the hook reads. A key added here
        // stops working on the local machine for the length of a session, so
        // the list is meant to stay short.
        assert_eq!(imp::stolen_for_test(0x5B), Some(0xE3), "Left Windows");
        assert_eq!(imp::stolen_for_test(0x5C), Some(0xE7), "Right Windows");
        assert_eq!(imp::stolen_for_test(0x09), Some(0x2B), "Tab");
        assert_eq!(imp::stolen_for_test(0x1B), Some(0x29), "Escape");
        assert_eq!(imp::stolen_for_test(0x46), Some(0x3D), "F4, for Alt+F4");
        assert_eq!(imp::stolen_for_test(0x41), None, "an ordinary letter");
        // Ctrl+Alt+Delete is not in the table and must never be: the kernel
        // owns it, and a table entry would promise something impossible.
        assert_eq!(imp::stolen_for_test(0x2E), None, "Delete");
    }
}
