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
//! # Which keys, and when
//!
//! The Windows keys always (so Win+R, Win+E, Win+D, Win+Tab and a lone tap of
//! Win all reach the far machine: the key itself is taken, and the letter that
//! follows arrives through the window like any other). Tab with Alt held,
//! Escape with Ctrl or Alt held, and F4 with Alt held — the shell's own
//! hotkeys — and nothing else. Whether a given key is taken is decided by
//! [`classify::verdict`], a plain function of the key and the state around
//! it, so the whole policy is testable without a keyboard.
//!
//! # What this cannot take, and why
//!
//! **Ctrl+Alt+Delete and Win+L cannot be intercepted by any program.** They are
//! the Secure Attention Sequence, handled inside the kernel specifically so
//! that no application — including this one, and including anything pretending
//! to be a login screen — can see or fake them. That is a security property
//! worth having, not a gap to work around. Reaching the far machine's Ctrl+Alt+
//! Delete needs the session agent that P5 installs, which can ask the secure
//! desktop directly; there is a button for it rather than a key. Nothing here
//! promises anything about either sequence, and nothing should be built that
//! assumes it does.
//!
//! # Why the hook did nothing, and what makes it work
//!
//! A hook installed by *this* process is not called for keys typed into one of
//! *this* process's own windows while the process is registered for raw
//! keyboard input — and the windowing library registers for exactly that when
//! it builds its event loop, for device events nothing here reads. The result
//! was a hook that installed cleanly, reported success, was armed by every
//! session, and was never once called while the session was on screen: the
//! Windows key opened the local Start menu, and Alt+Tab switched local windows,
//! exactly as if the hook did not exist. Anything else hooking the keyboard
//! (another program's hook, this process's *mouse* hook) is unaffected, which
//! is what makes it hard to see. Reproduced outside Pravera with a window, a
//! low-level hook and `RegisterRawInputDevices`, and undone by removing the
//! keyboard registration (`RIDEV_REMOVE`). [`imp::start`] does that before
//! installing the hook.
//!
//! # Why the hook has a thread of its own
//!
//! A low-level hook is called *on the thread that installed it*, which has to
//! be answering messages when a key arrives, and Windows gives it a short
//! deadline (`LowLevelHooksTimeout`, 300 ms by default) to answer. A hook that
//! misses that deadline is skipped for the key — which, for the Windows key,
//! means the local Start menu opens — and Windows may silently remove it
//! altogether, with no notification to the process that installed it.
//!
//! The thread that draws the picture is also the one decoding into it,
//! presenting to the swapchain and waking up for every window activation, and
//! any of those can hold it for longer than that. So the hook lives on a thread
//! that does nothing except pump messages for it, where the callback has the
//! whole deadline to itself and cannot be starved by anything the interface is
//! doing.
//!
//! # Why the state is global
//!
//! A hook procedure is a bare `extern "system" fn`: Windows calls it with no
//! way to carry a pointer along, so anything it touches has to be reachable
//! from a static. Everything here is small, lock-free on the hot path apart
//! from the queue, and only touched while a hook is installed.

use pravera_proto::KeyCode;

/// A key the hook took, on its way to the far machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grabbed {
    pub code: KeyCode,
    pub pressed: bool,
}

/// The policy: which keys are taken, in which state.
///
/// Kept apart from the hook so it can be exercised as data. Virtual key codes
/// are Windows' own, and nothing outside the Windows hook reads them, but the
/// table compiles everywhere so its tests run wherever the workspace does.
#[cfg_attr(not(windows), allow(dead_code))]
mod classify {
    /// The virtual keys this policy can ever take.
    pub const VK_TAB: u32 = 0x09;
    pub const VK_ESCAPE: u32 = 0x1B;
    pub const VK_F4: u32 = 0x73;
    pub const VK_LWIN: u32 = 0x5B;
    pub const VK_RWIN: u32 = 0x5C;

    /// Which of Ctrl and Alt are down.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct Modifiers {
        pub ctrl: bool,
        pub alt: bool,
    }

    /// Everything that decides whether a key is taken, apart from the key.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Situation {
        /// A session holds the keyboard.
        pub grabbing: bool,
        /// This process's window is the foreground window. A key pressed while
        /// something else has focus belongs to that something.
        pub ours_in_front: bool,
        /// Pravera's own host half injected the event on this machine. Taking
        /// it back would put a machine hosting and viewing at once into a loop.
        pub injected_by_us: bool,
        /// The press of this very key was taken, so its release must be too.
        pub press_was_taken: bool,
        pub modifiers: Modifiers,
    }

    /// What to do with one key event.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Verdict {
        /// Let it through to the shell and the window as usual.
        Pass,
        /// Swallow it and forward this HID usage to the far machine.
        Take(u16),
    }

    /// The HID usage of a key that can be taken, if it is one.
    ///
    /// Deliberately short. Every key here is a key that stops working on this
    /// machine for as long as a session is up, so the list holds only the ones
    /// that are otherwise impossible to send.
    pub const fn usage(vk: u32) -> Option<u16> {
        match vk {
            VK_LWIN => Some(0xE3),   // Left Windows  -> Left GUI
            VK_RWIN => Some(0xE7),   // Right Windows -> Right GUI
            VK_TAB => Some(0x2B),    // Tab, for Alt+Tab
            VK_ESCAPE => Some(0x29), // Escape, for Ctrl+Esc and Alt+Esc
            VK_F4 => Some(0x3D),     // F4, for Alt+F4
            _ => None,
        }
    }

    /// A stable small number for each takeable key, to index the record of
    /// which presses were taken.
    pub const fn slot(vk: u32) -> Option<u32> {
        match vk {
            VK_LWIN => Some(0),
            VK_RWIN => Some(1),
            VK_TAB => Some(2),
            VK_ESCAPE => Some(3),
            VK_F4 => Some(4),
            _ => None,
        }
    }

    /// Whether this key, pressed now, is one the shell is about to act on.
    ///
    /// Tab, Escape and F4 are ordinary keys most of the time and must keep
    /// working as ordinary keys; only the combinations that trigger a shell
    /// hotkey are worth taking. The Windows key is always taken, because it
    /// does nothing else.
    const fn is_shell_hotkey(vk: u32, held: Modifiers) -> bool {
        match vk {
            VK_LWIN | VK_RWIN => true,
            // Alt+Tab.
            VK_TAB => held.alt,
            // Ctrl+Esc opens Start; Alt+Esc cycles windows.
            VK_ESCAPE => held.ctrl || held.alt,
            // Alt+F4 asks this window to close. While the keyboard is held it
            // belongs to the far machine: closing *this* window instead of the
            // window the person is looking at is worse than not answering.
            // The session's own End button is the way out that always works.
            VK_F4 => held.alt,
            _ => false,
        }
    }

    /// Decide what to do with one event.
    pub const fn verdict(vk: u32, pressed: bool, now: Situation) -> Verdict {
        let Some(usage) = usage(vk) else {
            return Verdict::Pass;
        };

        // Not holding the keyboard, or not the window in front: nothing here is
        // ours to take, whatever the key.
        if !now.grabbing || !now.ours_in_front || now.injected_by_us {
            return Verdict::Pass;
        }

        if pressed {
            if is_shell_hotkey(vk, now.modifiers) {
                Verdict::Take(usage)
            } else {
                Verdict::Pass
            }
        } else if now.press_was_taken {
            // A release is taken exactly when its press was, so the far machine
            // never ends up holding a key down forever, and this machine never
            // sees the release of a key it did not see pressed. Checking the
            // hotkey condition again on release would fail whenever the
            // modifier was let go first, which is the common case for Alt+Tab.
            Verdict::Take(usage)
        } else {
            Verdict::Pass
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{mpsc, Mutex};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use tracing::{debug, warn};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::Input::{RegisterRawInputDevices, RAWINPUTDEVICE, RIDEV_REMOVE};
    use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId, PeekMessageW,
        PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG,
        PM_NOREMOVE, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };

    use super::classify::{self, Modifiers, Situation, Verdict};
    use super::Grabbed;

    /// Whether the hook should be taking anything right now.
    ///
    /// Separate from whether a hook thread exists so that the hook procedure
    /// has one cheap thing to check first, and so that a hook mid-removal
    /// cannot take a key after the session has let go of the keyboard.
    static GRABBING: AtomicBool = AtomicBool::new(false);

    /// Which takeable keys currently have a press that was taken, one bit per
    /// [`classify::slot`]. A release is taken only if its bit is set.
    static TAKEN_DOWN: AtomicU32 = AtomicU32::new(0);

    /// The thread the hook lives on.
    static HOOK_THREAD: Mutex<Option<HookThread>> = Mutex::new(None);

    struct HookThread {
        /// Its Win32 thread id, to post `WM_QUIT` to.
        id: u32,
        join: JoinHandle<()>,
    }

    /// Keys taken, waiting to be forwarded.
    ///
    /// A plain `Vec` behind a lock rather than a channel: the hold is a push or
    /// a swap, and a lock held that briefly is cheaper than a channel that
    /// allocates.
    static TAKEN: Mutex<Vec<Grabbed>> = Mutex::new(Vec::new());

    /// The longest queue that will be held before keys start being dropped.
    ///
    /// Reached only if the interface has stopped draining, which means the
    /// session is already gone. Without a bound, a stuck window would grow this
    /// until the machine ran out of memory, with the keyboard still captured.
    const MAX_QUEUED: usize = 256;

    /// How long `start` waits for the hook thread to report it is running.
    const STARTUP_WAIT: Duration = Duration::from_secs(2);

    /// Whether a virtual key is down this instant.
    fn held(vk: i32) -> bool {
        // The high bit is "down now"; the low bit is "was pressed since the
        // last call", which is not the question being asked. Reading the
        // modifier here rather than tracking it ourselves, because the hook
        // may well have been installed while Alt was already down.
        (unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000) != 0
    }

    /// Whether the foreground window belongs to this process.
    ///
    /// The interface's own idea of "focused" comes from window events, which
    /// can lag the truth by a message or two and, if one is ever missed, be
    /// wrong indefinitely. The hook asks the system instead, at the moment the
    /// key arrives: a key pressed while another program has focus is that
    /// program's, whatever the interface last heard.
    fn ours_in_front() -> bool {
        let front = unsafe { GetForegroundWindow() };
        if front.0.is_null() {
            return false;
        }
        let mut owner = 0u32;
        unsafe { GetWindowThreadProcessId(front, Some(&mut owner as *mut u32)) };
        owner == unsafe { GetCurrentProcessId() }
    }

    unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // A negative code means Windows is telling us to keep out of it.
        if code < 0 || !GRABBING.load(Ordering::Relaxed) {
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }

        let pressed = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        let released = matches!(wparam.0 as u32, WM_KEYUP | WM_SYSKEYUP);
        if !pressed && !released {
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }

        // SAFETY: the pointer is the structure Windows documents for this
        // notification, valid for the duration of the call.
        let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };

        // Almost every key is not one of ours to take; answer those without
        // asking the system anything.
        let Some(slot) = classify::slot(event.vkCode) else {
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        };
        let bit = 1u32 << slot;

        let verdict = classify::verdict(
            event.vkCode,
            pressed,
            Situation {
                grabbing: true,
                ours_in_front: ours_in_front(),
                // Only what Pravera's own host half injected. Input injected
                // by anything else — a KVM switch's software, a remapper, an
                // on-screen keyboard — is somebody's keystroke and is held to
                // the same rules as a real one.
                injected_by_us: event.dwExtraInfo == pravera_input::INJECTION_TAG,
                press_was_taken: TAKEN_DOWN.load(Ordering::Relaxed) & bit != 0,
                modifiers: Modifiers {
                    ctrl: held(0x11),
                    alt: held(0x12),
                },
            },
        );

        let Verdict::Take(usage) = verdict else {
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        };

        if pressed {
            TAKEN_DOWN.fetch_or(bit, Ordering::Relaxed);
        } else {
            TAKEN_DOWN.fetch_and(!bit, Ordering::Relaxed);
        }

        if let Ok(mut queue) = TAKEN.lock() {
            if queue.len() < MAX_QUEUED {
                queue.push(Grabbed {
                    code: pravera_proto::KeyCode(usage),
                    pressed,
                });
            }
        }

        // Non-zero: the key stops here and this machine never sees it.
        LRESULT(1)
    }

    /// The hook thread's whole life: install, pump messages until told to quit,
    /// uninstall. Reports its thread id once a message queue exists to post the
    /// quit to, or the reason it could not install.
    fn run(ready: mpsc::Sender<Result<u32, String>>) {
        // A thread has no message queue until it first asks for one, and
        // `PostThreadMessage` to a thread without one fails. Ask before saying
        // the thread is ready.
        let mut message = MSG::default();
        let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };

        let handle = match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), None, 0) } {
            Ok(handle) => handle,
            Err(error) => {
                let _ = ready.send(Err(error.to_string()));
                return;
            }
        };
        let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));

        // `GetMessageW` returns zero for `WM_QUIT` and -1 for an error; either
        // ends the loop. Nothing here is dispatched: the hook needs the thread
        // to be *in* this call, not to handle anything it returns.
        while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {}

        let _ = unsafe { UnhookWindowsHookEx(handle) };
    }

    /// Take this process off the list of those receiving raw keyboard input.
    ///
    /// The windowing library registers for it when its event loop is built,
    /// for a kind of event nothing here reads — and while a process is
    /// registered, a low-level keyboard hook *of that same process* is not
    /// called at all whenever one of its own windows is in front. Every key
    /// goes past it, so the Windows key opens the local Start menu while the
    /// session is on screen and the hook, installed and armed, sits there
    /// unused. A hook in any other process is unaffected, and so is a mouse
    /// hook; it is the one combination, and it is the combination this is.
    ///
    /// Removing the registration (a `RIDEV_REMOVE` for the keyboard's usage,
    /// with no target window, which is how it is undone) restores the hook
    /// at once. Idempotent, so it is repeated on every start rather than
    /// remembered.
    fn release_raw_keyboard() {
        let keyboard = RAWINPUTDEVICE {
            usUsagePage: 0x01, // Generic desktop controls
            usUsage: 0x06,     // Keyboard
            dwFlags: RIDEV_REMOVE,
            hwndTarget: HWND::default(),
        };
        if let Err(error) =
            unsafe { RegisterRawInputDevices(&[keyboard], std::mem::size_of::<RAWINPUTDEVICE>() as u32) }
        {
            warn!(%error, "could not stop raw keyboard input; the Windows key may stay local");
        }
    }

    /// Start intercepting, bringing the hook thread up if it is not already
    /// running.
    pub fn start() {
        release_raw_keyboard();
        let Ok(mut slot) = HOOK_THREAD.lock() else {
            return;
        };

        if slot.is_none() {
            let (ready, report) = mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("pravera-key-hook".into())
                .spawn(move || run(ready));
            let join = match spawned {
                Ok(join) => join,
                Err(error) => {
                    warn!(%error, "the Windows key cannot be forwarded on this machine");
                    return;
                }
            };

            match report.recv_timeout(STARTUP_WAIT) {
                Ok(Ok(id)) => {
                    *slot = Some(HookThread { id, join });
                    debug!("holding the keys Windows would otherwise keep");
                }
                // Every other key still forwards; the shell keys keep going to
                // this machine. Worth a line in the log and nothing more.
                Ok(Err(error)) => {
                    warn!(%error, "the Windows key cannot be forwarded on this machine");
                    let _ = join.join();
                    return;
                }
                Err(_) => {
                    warn!("the key hook did not start in time; the Windows key stays local");
                    return;
                }
            }
        }

        TAKEN_DOWN.store(0, Ordering::Relaxed);
        GRABBING.store(true, Ordering::Relaxed);
    }

    /// Stop intercepting and give the keys back.
    ///
    /// The hook is removed rather than just disarmed: a hook left installed is
    /// a callback on every keystroke on the machine for as long as the window
    /// is open, which is not a thing to leave running for a session that ended.
    pub fn stop() {
        GRABBING.store(false, Ordering::Relaxed);
        TAKEN_DOWN.store(0, Ordering::Relaxed);
        if let Ok(mut queue) = TAKEN.lock() {
            queue.clear();
        }

        let Ok(mut slot) = HOOK_THREAD.lock() else {
            return;
        };
        if let Some(thread) = slot.take() {
            // Posting can only fail if the thread is already gone, which is
            // the outcome being asked for.
            let _ = unsafe { PostThreadMessageW(thread.id, WM_QUIT, WPARAM(0), LPARAM(0)) };
            let _ = thread.join.join();
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

    /// Whether a hook thread is up, for the lifecycle test.
    #[cfg(test)]
    pub fn hook_thread_running() -> bool {
        HOOK_THREAD.lock().is_ok_and(|slot| slot.is_some())
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
    use super::classify::*;
    use super::*;

    const NONE: Modifiers = Modifiers { ctrl: false, alt: false };
    const CTRL: Modifiers = Modifiers { ctrl: true, alt: false };
    const ALT: Modifiers = Modifiers { ctrl: false, alt: true };

    /// A session in front of the user, holding the keyboard, nothing pressed.
    const LIVE: Situation = Situation {
        grabbing: true,
        ours_in_front: true,
        injected_by_us: false,
        press_was_taken: false,
        modifiers: NONE,
    };

    fn with(modifiers: Modifiers) -> Situation {
        Situation { modifiers, ..LIVE }
    }

    #[test]
    fn a_windows_key_is_taken_and_forwarded_as_the_gui_key_it_is() {
        assert_eq!(verdict(VK_LWIN, true, LIVE), Verdict::Take(0xE3));
        assert_eq!(verdict(VK_RWIN, true, LIVE), Verdict::Take(0xE7));
    }

    #[test]
    fn a_windows_key_is_taken_with_any_modifier_state() {
        // Win+Ctrl, Win+Alt: the key itself is the shell's, whatever is held.
        for held in [NONE, CTRL, ALT, Modifiers { ctrl: true, alt: true }] {
            assert_eq!(verdict(VK_LWIN, true, with(held)), Verdict::Take(0xE3));
        }
    }

    #[test]
    fn a_lone_tap_of_the_windows_key_never_reaches_this_machines_start_menu() {
        // The press is taken, and so is the release that follows it: the
        // release is what would open Start, and it must not be the one thing
        // that slips through.
        let released = Situation { press_was_taken: true, ..LIVE };
        assert_eq!(verdict(VK_LWIN, false, released), Verdict::Take(0xE3));
    }

    #[test]
    fn a_release_is_taken_only_when_its_press_was() {
        // Pressed while another window had focus, released back in ours: the
        // far machine never saw the press, so it must not be sent the release.
        assert_eq!(verdict(VK_LWIN, false, LIVE), Verdict::Pass);
        assert_eq!(verdict(VK_TAB, false, LIVE), Verdict::Pass);
    }

    #[test]
    fn a_release_is_taken_even_if_the_modifier_was_let_go_first() {
        // Alt+Tab, Alt released before Tab: the release still has to reach the
        // far machine, or Tab is held down there for good.
        let released = Situation { press_was_taken: true, modifiers: NONE, ..LIVE };
        assert_eq!(verdict(VK_TAB, false, released), Verdict::Take(0x2B));
    }

    #[test]
    fn tab_escape_and_f4_are_taken_only_as_the_shell_hotkeys_they_can_be() {
        assert_eq!(verdict(VK_TAB, true, with(ALT)), Verdict::Take(0x2B));
        assert_eq!(verdict(VK_TAB, true, with(NONE)), Verdict::Pass, "plain Tab types a tab");
        assert_eq!(verdict(VK_TAB, true, with(CTRL)), Verdict::Pass, "Ctrl+Tab is the far app's");

        assert_eq!(verdict(VK_ESCAPE, true, with(CTRL)), Verdict::Take(0x29), "Ctrl+Esc");
        assert_eq!(verdict(VK_ESCAPE, true, with(ALT)), Verdict::Take(0x29), "Alt+Esc");
        assert_eq!(verdict(VK_ESCAPE, true, with(NONE)), Verdict::Pass, "plain Escape");

        assert_eq!(verdict(VK_F4, true, with(ALT)), Verdict::Take(0x3D), "Alt+F4");
        assert_eq!(verdict(VK_F4, true, with(NONE)), Verdict::Pass, "plain F4");
        assert_eq!(verdict(VK_F4, true, with(CTRL)), Verdict::Pass, "Ctrl+F4 closes a tab");
    }

    #[test]
    fn nothing_is_taken_when_the_keyboard_is_not_held() {
        let idle = Situation { grabbing: false, ..LIVE };
        assert_eq!(verdict(VK_LWIN, true, idle), Verdict::Pass);
        assert_eq!(verdict(VK_TAB, true, Situation { modifiers: ALT, ..idle }), Verdict::Pass);
        // Not even a release that had a press: a hook let go of mid-key leaves
        // the far machine to `release_everything`, not to a stale queue.
        let stale = Situation { press_was_taken: true, ..idle };
        assert_eq!(verdict(VK_LWIN, false, stale), Verdict::Pass);
    }

    #[test]
    fn nothing_is_taken_when_another_window_has_focus() {
        let elsewhere = Situation { ours_in_front: false, ..LIVE };
        assert_eq!(verdict(VK_LWIN, true, elsewhere), Verdict::Pass);
        assert_eq!(verdict(VK_LWIN, false, Situation { press_was_taken: true, ..elsewhere }), Verdict::Pass);
        assert_eq!(verdict(VK_TAB, true, Situation { modifiers: ALT, ..elsewhere }), Verdict::Pass);
    }

    #[test]
    fn what_pravera_injected_itself_is_never_taken_back() {
        // A machine that hosts and views at once would otherwise feed its own
        // host half's keystrokes back to itself, forever.
        let ours = Situation { injected_by_us: true, ..LIVE };
        assert_eq!(verdict(VK_LWIN, true, ours), Verdict::Pass);
        assert_eq!(verdict(VK_LWIN, false, Situation { press_was_taken: true, ..ours }), Verdict::Pass);
    }

    #[test]
    fn keys_the_release_and_gaming_chords_are_built_from_are_never_taken() {
        // Ctrl+Alt+Shift+P and +G are read by the window, and must keep
        // arriving there with every modifier down.
        let all = Modifiers { ctrl: true, alt: true };
        for vk in [
            0x50, // P
            0x47, // G
            0x10, // Shift
            0x11, // Ctrl
            0x12, // Alt
            0xA0, // Left Shift
            0xA2, // Left Ctrl
            0xA4, // Left Alt
        ] {
            assert_eq!(verdict(vk, true, with(all)), Verdict::Pass, "vk {vk:#x}");
            assert_eq!(
                verdict(vk, false, Situation { press_was_taken: true, ..with(all) }),
                Verdict::Pass,
                "vk {vk:#x}"
            );
        }
    }

    #[test]
    fn ordinary_keys_and_the_secure_attention_key_are_not_in_the_table() {
        assert_eq!(usage(0x41), None, "an ordinary letter");
        // Ctrl+Alt+Delete is not in the table and must never be: the kernel
        // owns it, and a table entry would promise something impossible. The
        // same goes for L, which is half of Win+L.
        assert_eq!(usage(0x2E), None, "Delete");
        assert_eq!(usage(0x4C), None, "L");
        assert_eq!(verdict(0x2E, true, with(Modifiers { ctrl: true, alt: true })), Verdict::Pass);
    }

    #[test]
    fn every_takeable_key_has_a_slot_of_its_own_in_the_record_of_taken_presses() {
        let keys = [VK_LWIN, VK_RWIN, VK_TAB, VK_ESCAPE, VK_F4];
        let mut seen = 0u32;
        for vk in keys {
            let slot = slot(vk).expect("a takeable key has a slot");
            assert!(slot < 32);
            assert_eq!(seen & (1 << slot), 0, "slot {slot} used twice");
            seen |= 1 << slot;
            assert!(usage(vk).is_some(), "{vk:#x} has a slot but no usage");
        }
        assert_eq!(slot(0x41), None);
    }

    #[test]
    fn the_keys_taken_are_the_ones_that_cannot_be_sent_any_other_way() {
        assert_eq!(usage(VK_LWIN), Some(0xE3), "Left Windows");
        assert_eq!(usage(VK_RWIN), Some(0xE7), "Right Windows");
        assert_eq!(usage(VK_TAB), Some(0x2B), "Tab");
        assert_eq!(usage(VK_ESCAPE), Some(0x29), "Escape");
        assert_eq!(usage(VK_F4), Some(0x3D), "F4, for Alt+F4");
    }

    #[test]
    fn the_hook_lives_exactly_as_long_as_the_grab() {
        // One test rather than three: the hook and its queue are process-wide,
        // and tests run in parallel.
        assert!(!is_grabbing());
        assert!(drain().is_empty());

        // Called on every session teardown, including ones that never got as
        // far as taking the keyboard.
        stop();
        assert!(!is_grabbing());

        #[cfg(windows)]
        {
            start();
            assert!(is_grabbing());
            assert!(imp::hook_thread_running(), "a grab has a thread to pump its hook");

            // Asking twice is not asking for a second hook.
            start();
            assert!(imp::hook_thread_running());

            stop();
            assert!(!is_grabbing());
            assert!(!imp::hook_thread_running(), "a released grab leaves no hook behind");

            // And it can be taken again afterwards.
            start();
            assert!(imp::hook_thread_running());
            stop();
            assert!(!imp::hook_thread_running());
        }
    }
}
