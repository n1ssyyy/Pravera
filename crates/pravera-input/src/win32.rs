//! Injecting input on Windows through `SendInput`.
//!
//! ## What this can and cannot do
//!
//! Absolute motion, buttons, wheels, physical keys and Unicode text all work
//! correctly and need no driver, no elevation beyond the session agent's, and
//! no third-party components.
//!
//! Relative motion is the exception, and the reason is structural rather than
//! a matter of getting the flags right. `SendInput` with `MOUSEEVENTF_MOVE`
//! and no `MOUSEEVENTF_ABSOLUTE` does apply a delta — but it applies it to the
//! *cursor*, after pointer ballistics, and the injected motion never appears
//! in the Raw Input stream that fullscreen games read. Such a game sees a
//! cursor that jumped and a mouse that never moved, so the camera does not
//! turn. Nothing callable from user mode fixes this; the fix is a signed
//! kernel-mode HID driver, which the plan keeps behind [`InputSink`] so it can
//! land later without disturbing anything above.
//!
//! So [`SendInputSink::injects_true_relative`] returns `false`. The delta is
//! still applied — moving the cursor is the right thing for every desktop
//! application, which is most of what people do — but the host reports the
//! limitation instead of letting someone find it out during a match.
//!
//! ## Why UIPI failures are the interesting error
//!
//! `SendInput` returns the number of events it queued. A short count with
//! `ERROR_ACCESS_DENIED` (5) means User Interface Privilege Isolation blocked
//! it: a process at medium integrity cannot inject into one running elevated,
//! and the foreground window decides which applies. This is why the session
//! agent runs as SYSTEM. Reporting it as a distinct [`InputError::Refused`]
//! matters because the symptom — input works everywhere except in one window —
//! is otherwise indistinguishable from a broken keymap.

use windows::Win32::Foundation::{GetLastError, ERROR_ACCESS_DENIED};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY, VK_PAUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    WHEEL_DELTA, XBUTTON1, XBUTTON2,
};

use pravera_proto::{KeyCode, PointerButton};

use crate::{scan_code, InputError, InputSink, Result};

/// Marks events Pravera injected.
///
/// Carried in `dwExtraInfo` and visible to any low-level hook, including our
/// own capture side. Without it a host that also captures input would feed
/// injected events back to the client and produce a loop; with it, the loop is
/// one comparison away from being broken.
const PRAVERA_TAG: usize = 0x5052_4156;

/// The full range of a normalised absolute coordinate, per `MOUSEINPUT`.
const ABSOLUTE_RANGE: f64 = 65535.0;

pub(crate) struct SendInputSink {
    /// Accumulated sub-detent scroll.
    ///
    /// A trackpad sends fractions of a wheel click. Rounding each one to zero
    /// makes trackpad scrolling do nothing at all, so the remainder is kept
    /// and spent once it adds up to a click.
    scroll_debt: (f32, f32),
}

impl SendInputSink {
    pub(crate) fn new() -> SendInputSink {
        SendInputSink {
            scroll_debt: (0.0, 0.0),
        }
    }
}

impl InputSink for SendInputSink {
    fn name(&self) -> &'static str {
        "SendInput"
    }

    fn injects_true_relative(&self) -> bool {
        false
    }

    fn pointer_to(&mut self, x: i32, y: i32) -> Result<()> {
        let (dx, dy) = to_virtual_desktop(x, y);
        send(&[mouse(
            dx,
            dy,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )])
    }

    fn pointer_by(&mut self, dx: i32, dy: i32) -> Result<()> {
        // Moves the cursor, which is correct for desktop use. Games reading
        // raw input will not see it — see the module comment.
        send(&[mouse(dx, dy, 0, MOUSEEVENTF_MOVE)])
    }

    fn button(&mut self, button: PointerButton, pressed: bool) -> Result<()> {
        let (flags, extra) = match (button, pressed) {
            (PointerButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (PointerButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (PointerButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (PointerButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (PointerButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (PointerButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            // Back and Forward are the two X buttons, told apart by mouseData
            // rather than by the flag.
            (PointerButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1 as u32),
            (PointerButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1 as u32),
            (PointerButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2 as u32),
            (PointerButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2 as u32),
        };
        send(&[mouse(0, 0, extra, flags)])
    }

    fn scroll(&mut self, dx: f32, dy: f32) -> Result<()> {
        let (clicks_x, rest_x) = detents(dx + self.scroll_debt.0);
        let (clicks_y, rest_y) = detents(dy + self.scroll_debt.1);
        self.scroll_debt = (rest_x, rest_y);

        let mut events = Vec::with_capacity(2);
        if clicks_y != 0 {
            events.push(mouse(0, 0, clicks_y as u32, MOUSEEVENTF_WHEEL));
        }
        if clicks_x != 0 {
            events.push(mouse(0, 0, clicks_x as u32, MOUSEEVENTF_HWHEEL));
        }
        if events.is_empty() {
            // Not yet a whole click. The remainder is banked, not lost.
            return Ok(());
        }
        send(&events)
    }

    fn key(&mut self, code: KeyCode, pressed: bool) -> Result<()> {
        if let Some(vk) = virtual_only(code.0) {
            let flags = if pressed {
                KEYBD_EVENT_FLAGS(0)
            } else {
                KEYEVENTF_KEYUP
            };
            return send(&[keyboard(vk, 0, flags)]);
        }

        let scan = scan_code(code.0).ok_or(InputError::UnmappedKey(code.0))?;

        let mut flags = KEYEVENTF_SCANCODE;
        if scan.extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !pressed {
            flags |= KEYEVENTF_KEYUP;
        }
        send(&[keyboard(VIRTUAL_KEY(0), scan.code, flags)])
    }

    fn text(&mut self, text: &str) -> Result<()> {
        // `KEYEVENTF_UNICODE` takes UTF-16 code units, so anything outside the
        // basic plane — emoji, and a fair amount of CJK — arrives as a
        // surrogate pair and needs two events. `encode_utf16` produces exactly
        // that, and pressing the two halves separately is what the API expects.
        let mut events = Vec::with_capacity(text.len() * 2);
        for unit in text.encode_utf16() {
            events.push(keyboard(VIRTUAL_KEY(0), unit, KEYEVENTF_UNICODE));
            events.push(keyboard(
                VIRTUAL_KEY(0),
                unit,
                KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
            ));
        }
        if events.is_empty() {
            return Ok(());
        }
        send(&events)
    }
}

/// Keys that have to go through a virtual key rather than a scan code.
///
/// The whole point of the scan-code path is that it carries a key *position*
/// and lets the host apply its own layout — see `keymap`. A virtual key
/// carries a meaning instead, so using one reintroduces exactly the layout
/// confusion that path avoids.
///
/// It is safe here for one reason: these keys mean the same thing on every
/// layout in existence. Pause is the only member. Its set-1 sequence uses the
/// `0xE1` prefix, which `SendInput` has no way to express, so the choice is
/// between a virtual key and the key simply not working — and `Ctrl+Break` in
/// a terminal is worth the exception.
///
/// Nothing layout-dependent may be added to this list.
fn virtual_only(usage: u16) -> Option<VIRTUAL_KEY> {
    match usage {
        0x48 => Some(VK_PAUSE),
        _ => None,
    }
}

/// Turn a desktop pixel into the 0..65535 coordinate `MOUSEINPUT` wants.
///
/// The range covers the *whole virtual desktop*, not the primary display, and
/// the origin can be negative when a monitor sits above or to the left. Using
/// the primary display's size here is the classic version of this bug: it
/// works perfectly on a single-monitor machine and puts the pointer in the
/// wrong place on every other one.
fn to_virtual_desktop(x: i32, y: i32) -> (i32, i32) {
    let (left, top, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    normalise(x, y, left, top, width, height)
}

/// The arithmetic of [`to_virtual_desktop`], separated so it can be tested
/// without a desktop of a particular shape.
fn normalise(x: i32, y: i32, left: i32, top: i32, width: i32, height: i32) -> (i32, i32) {
    // A zero here would mean Windows reported a desktop with no area, which
    // should not happen; dividing by it would, so it is guarded anyway.
    let span_x = (width.max(1) - 1).max(1) as f64;
    let span_y = (height.max(1) - 1).max(1) as f64;

    let across = ((x - left) as f64 / span_x * ABSOLUTE_RANGE).round();
    let down = ((y - top) as f64 / span_y * ABSOLUTE_RANGE).round();
    (
        across.clamp(0.0, ABSOLUTE_RANGE) as i32,
        down.clamp(0.0, ABSOLUTE_RANGE) as i32,
    )
}

/// Split a scroll amount into whole wheel clicks and what is left over.
///
/// `mouseData` counts in `WHEEL_DELTA` units, and truncating towards zero is
/// deliberate: the remainder is banked by the caller, so nothing is lost and
/// slow trackpad scrolling still eventually moves.
fn detents(amount: f32) -> (i32, f32) {
    let clicks = amount.trunc();
    ((clicks * WHEEL_DELTA as f32) as i32, amount - clicks)
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                // Zero means "now". Supplying our own timestamp would make the
                // events look older than they are once the queue is busy.
                time: 0,
                dwExtraInfo: PRAVERA_TAG,
            },
        },
    }
}

fn keyboard(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: PRAVERA_TAG,
            },
        },
    }
}

/// Hand a batch to Windows and check that all of it was taken.
///
/// Sent as one call rather than one call per event, because `SendInput`
/// guarantees a batch is not interleaved with input from a real device. Split
/// into separate calls, a key-down from the user's own keyboard can land
/// between an injected press and its release.
fn send(events: &[INPUT]) -> Result<()> {
    let sent = unsafe { SendInput(events, core::mem::size_of::<INPUT>() as i32) };
    if sent as usize == events.len() {
        return Ok(());
    }

    let error = unsafe { GetLastError() };
    if error == ERROR_ACCESS_DENIED {
        return Err(InputError::Refused(
            "blocked by UIPI — the foreground window is running at a higher \
             integrity level than the Pravera agent"
                .to_owned(),
        ));
    }
    Err(InputError::backend(format!(
        "SendInput accepted {sent} of {} events (error {})",
        events.len(),
        error.0
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_corners_of_the_virtual_desktop_map_to_the_ends_of_the_range() {
        // A single 1920x1080 display at the origin.
        assert_eq!(normalise(0, 0, 0, 0, 1920, 1080), (0, 0));
        assert_eq!(normalise(1919, 1079, 0, 0, 1920, 1080), (65535, 65535));
    }

    #[test]
    fn a_monitor_left_of_the_primary_is_still_addressable() {
        // Windows reports a negative virtual-screen origin when a display sits
        // to the left of the primary. Treating the origin as zero would fold
        // that entire monitor onto the left edge.
        let (left, top, width, height) = (-1920, 0, 3840, 1080);

        assert_eq!(normalise(-1920, 0, left, top, width, height), (0, 0));
        assert_eq!(normalise(1919, 0, left, top, width, height), (65535, 0));
        // The seam between the two displays lands in the middle.
        let (middle, _) = normalise(0, 0, left, top, width, height);
        assert!((32750..=32790).contains(&middle), "{middle}");
    }

    #[test]
    fn a_coordinate_off_the_desktop_is_clamped_rather_than_wrapped() {
        // These come from a peer via `Screen::to_desktop`, which clamps to the
        // streamed display — but the streamed display can itself be outside
        // the virtual desktop for a moment after a monitor is unplugged.
        assert_eq!(normalise(-500, -500, 0, 0, 1920, 1080), (0, 0));
        assert_eq!(normalise(99999, 99999, 0, 0, 1920, 1080), (65535, 65535));
    }

    #[test]
    fn whole_scroll_clicks_are_sent_and_fractions_are_banked() {
        assert_eq!(detents(1.0), (WHEEL_DELTA as i32, 0.0));
        assert_eq!(detents(-1.0), (-(WHEEL_DELTA as i32), 0.0));
        assert_eq!(detents(0.0), (0, 0.0));

        // A trackpad tick. Nothing is sent, but nothing is lost either.
        let (clicks, rest) = detents(0.3);
        assert_eq!(clicks, 0);
        assert!((rest - 0.3).abs() < 1e-6);
    }

    #[test]
    fn accumulated_trackpad_scrolling_eventually_moves() {
        // Four tenths of a click four times is more than one click. Without
        // the banked remainder, a trackpad would scroll nothing, ever.
        let mut debt = 0.0f32;
        let mut total = 0;
        for _ in 0..4 {
            let (clicks, rest) = detents(0.4 + debt);
            debt = rest;
            total += clicks;
        }
        assert_eq!(total, WHEEL_DELTA as i32);
    }

    #[test]
    fn an_unmapped_key_is_reported_and_does_not_stop_the_session() {
        let mut sink = SendInputSink::new();
        let error = sink.key(KeyCode(0x00), true).unwrap_err();

        assert!(matches!(error, InputError::UnmappedKey(0)));
        assert!(error.is_recoverable());
    }

    #[test]
    fn pause_is_the_only_key_that_bypasses_the_scan_code_path() {
        // A layout-dependent key added here would type the wrong character on
        // any keyboard that is not the client's own, which is the entire class
        // of bug the scan-code path exists to prevent.
        assert_eq!(virtual_only(0x48), Some(VK_PAUSE));
        for usage in (0x04..=0xE7u16).filter(|u| *u != 0x48) {
            assert_eq!(virtual_only(usage), None, "{usage:#04x}");
        }
    }

    #[test]
    fn empty_text_is_not_sent_at_all() {
        // `SendInput` with zero events returns zero, which the short-count
        // check would otherwise read as a failure.
        let mut sink = SendInputSink::new();
        assert!(sink.text("").is_ok());
    }
}
