//! Every movement the mouse makes, not only the ones a frame happens to catch.
//!
//! The pointer's positions reach the interface through `WM_MOUSEMOVE`, and that
//! message is **coalesced**: if two movements land between two frames, Windows
//! keeps only the later one and the earlier never existed. A person moving
//! quickly produces hundreds of positions a second; a window redrawing at
//! sixty frames a second receives sixty of them, each a jump from wherever the
//! pointer was last drawn to wherever it ended up. Forwarded to the far
//! machine, the remote cursor crosses the same distance in the same time but
//! in a few large visible steps — movement that reads as filtered or robotic,
//! because the smooth middle of the gesture was thrown away before this
//! program ever saw it.
//!
//! A `WH_MOUSE_LL` hook runs before that coalescing. It is called once per
//! input event, at the mouse's own reporting rate, with the position of that
//! instant — so the queue below holds the gesture the hand actually made, and
//! forwarding it whole puts every intermediate position on the far machine.
//!
//! The hook only *observes*. Every call ends in `CallNextHookEx`, nothing is
//! ever swallowed, and a machine without a session running has no hook at all.

/// One pointer position, in physical screen pixels.
///
/// The low-level hook reports physical pixels; everything the interface
/// measures is in logical ones, so the consumer divides by the window's scale
/// factor before mapping onto the picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub x: i32,
    pub y: i32,
}

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
    use std::sync::Mutex;

    use tracing::warn;
    use windows::Win32::Foundation::{LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, ClipCursor, GetForegroundWindow, GetWindowRect, SetWindowsHookExW,
        UnhookWindowsHookEx, HHOOK, MOUSEHOOKSTRUCT, WH_MOUSE_LL, WM_MOUSEMOVE,
    };

    use super::Sample;

    /// Whether observed positions are being recorded.
    static RECORDING: AtomicBool = AtomicBool::new(false);
    /// The installed hook. Zero means nothing is installed.
    static HOOK: AtomicIsize = AtomicIsize::new(0);
    /// Positions taken since the last drain.
    static SEEN: Mutex<Vec<Sample>> = Mutex::new(Vec::new());

    /// The longest queue held between drains.
    ///
    /// One frame at a thousand movements a second is about sixteen entries;
    /// this is twenty frames of that. Reached only if the interface has
    /// stopped draining, which means the session is over; the queue then
    /// simply stops growing rather than growing without bound.
    const MAX_QUEUED: usize = 256;

    unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // A negative code means Windows is telling us to keep out of it, and
        // this hook never takes anything in any case.
        if code < 0 {
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }

        if wparam.0 as u32 == WM_MOUSEMOVE && RECORDING.load(Ordering::Relaxed) {
            // SAFETY: the pointer is the structure Windows documents for this
            // notification, valid for the duration of the call.
            let info = unsafe { &*(lparam.0 as *const MOUSEHOOKSTRUCT) };
            let point = &info.pt;
            if let Ok(mut queue) = SEEN.lock() {
                if queue.len() < MAX_QUEUED {
                    queue.push(Sample {
                        x: point.x,
                        y: point.y,
                    });
                }
            }
        }

        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    /// Begin recording positions, installing the hook if it is not there.
    ///
    /// Must be called from the thread running the message pump, for the same
    /// reason the keyboard hook must be: the callback is dispatched on the
    /// thread that installed it.
    pub fn start() {
        if HOOK.load(Ordering::Relaxed) == 0 {
            match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(hook), None, 0) } {
                Ok(handle) => {
                    HOOK.store(handle.0 as isize, Ordering::Relaxed);
                }
                // Movements still forward through the ordinary path, at the
                // coalesced rate. Worth a line in the log, nothing more.
                Err(error) => {
                    warn!(%error, "the pointer cannot be tracked at its full rate");
                    return;
                }
            }
        }
        if let Ok(mut queue) = SEEN.lock() {
            queue.clear();
        }
        RECORDING.store(true, Ordering::Relaxed);
    }

    /// Stop recording and remove the hook.
    pub fn stop() {
        RECORDING.store(false, Ordering::Relaxed);
        if let Ok(mut queue) = SEEN.lock() {
            queue.clear();
        }
        let handle = HOOK.swap(0, Ordering::Relaxed);
        if handle != 0 {
            let _ = unsafe { UnhookWindowsHookEx(HHOOK(handle as *mut _)) };
        }
    }

    /// Everything recorded since the last call.
    pub fn drain() -> Vec<Sample> {
        match SEEN.lock() {
            Ok(mut queue) => std::mem::take(&mut *queue),
            Err(_) => Vec::new(),
        }
    }

    pub fn is_recording() -> bool {
        RECORDING.load(Ordering::Relaxed)
    }

    /// Hold the cursor inside whatever window currently has the foreground.
    ///
    /// Gaming mode's other half. A first-person game on the far machine spins
    /// the camera as the pointer crosses its centre; the pointer escaping to a
    /// second monitor, or to this machine's taskbar, fires the camera off into
    /// the sky and the session is over. `ClipCursor` is the confinement games
    /// themselves use: the cursor physically cannot leave the rectangle until
    /// it is released, and Windows releases it anyway when the window loses
    /// focus, which is what makes Alt+Tab still work as the escape hatch.
    ///
    /// Windows also clears the clip on a number of its own interventions, so
    /// the caller re-asserts it every frame rather than setting it once.
    ///
    /// The foreground window is this application's whenever the caller asks:
    /// the clip is only ever wanted while the session window is focused, and
    /// clipping some other window would be a small act of sabotage.
    pub fn confine_to_foreground_window() {
        // SAFETY: all four calls take handles or out-parameters the
        // documentation describes, and the rectangle is filled by the call
        // that reads it.
        unsafe {
            let window = GetForegroundWindow();
            if window.is_invalid() {
                return;
            }
            let mut rect = RECT::default();
            if GetWindowRect(window, &mut rect).is_ok() {
                let _ = ClipCursor(Some(&rect));
            }
        }
    }

    /// Let the cursor go.
    ///
    /// Called when gaming mode ends, the session ends, or the window loses
    /// focus. Windows usually releases the clip by itself on exactly those
    /// events; calling it anyway means the one path that forgets cannot strand
    /// the cursor inside a rectangle nobody is watching any more.
    pub fn release_confinement() {
        // SAFETY: `None` is the documented way to release a cursor clip.
        unsafe {
            let _ = ClipCursor(None);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Sample;

    pub fn start() {}
    pub fn stop() {}
    pub fn drain() -> Vec<Sample> {
        Vec::new()
    }
    pub const fn is_recording() -> bool {
        false
    }
    pub fn confine_to_foreground_window() {}
    pub fn release_confinement() {}
}

pub use imp::{
    confine_to_foreground_window, drain, is_recording, release_confinement, start, stop,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_recorded_before_anything_asks() {
        assert!(!is_recording());
        assert!(drain().is_empty());
    }

    #[test]
    fn stopping_without_starting_is_harmless() {
        // Called on every session teardown, including ones that never began.
        stop();
        assert!(!is_recording());
        assert!(drain().is_empty());
    }
}
