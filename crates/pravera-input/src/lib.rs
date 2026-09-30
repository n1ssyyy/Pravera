//! Replaying a remote user's mouse and keyboard on this machine.
//!
//! The host receives [`InputEvent`]s that a client produced, and this crate is
//! where they stop being data and start being real key presses. Everything
//! platform-specific hides behind [`InputSink`]; everything above this crate
//! deals only in protocol events and a [`Screen`].
//!
//! ## What this crate does not decide
//!
//! Whether the peer is *allowed* to send input. That is checked host-side in
//! `pravera-host` before an event ever reaches here, against the permission
//! set the session's role carries. A sink injects what it is handed, without
//! opinion — which is precisely why the check has to be somewhere else and has
//! to be somewhere that cannot be reached by a modified client.
//!
//! ## The Windows relative-motion ceiling
//!
//! `SendInput` cannot emit true relative mouse motion: even the "relative"
//! flag routes through the Win32 cursor pipeline, so a game reading raw input
//! sees a cursor that was teleported rather than a mouse that was moved, and
//! camera control breaks. Linux `uinput` has no such problem — it injects at
//! the evdev layer, below anything that could tell the difference.
//!
//! This is why relative motion is a distinct method on the trait rather than
//! something translated into absolute moves here. A signed kernel-mode HID
//! driver on Windows fixes it later by implementing the same trait, and
//! nothing above this crate changes. [`InputSink::injects_true_relative`] is
//! how the host tells the difference today, so it can say so honestly instead
//! of implying a game will work when it will not.

mod error;
mod keymap;

pub use error::{InputError, Result};
pub use keymap::{scan_code, ScanCode};

use pravera_core::Resolution;
use pravera_proto::{InputEvent, KeyCode, PointerButton};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod win32;

/// The `dwExtraInfo` value on every key and pointer event this crate injects
/// on Windows. A low-level hook on the same machine compares against it to
/// tell Pravera's own input from anybody else's.
#[cfg(windows)]
pub use win32::PRAVERA_TAG as INJECTION_TAG;

/// Somewhere input events can be injected.
///
/// Every method reports failure rather than swallowing it. A dropped key press
/// is worse than a visible error: the matching release still arrives, the host
/// ends up with a key stuck down, and the person at the other end is left
/// holding a keyboard that types nothing.
pub trait InputSink: Send {
    /// A short name for logs and the session overlay.
    fn name(&self) -> &'static str;

    /// Whether [`InputSink::pointer_by`] reaches games that read raw input.
    ///
    /// `false` on Windows until the kernel driver exists. The UI uses this to
    /// warn rather than to let someone discover it mid-match.
    fn injects_true_relative(&self) -> bool {
        false
    }

    /// Move the pointer to a desktop coordinate.
    fn pointer_to(&mut self, x: i32, y: i32) -> Result<()>;

    /// Move the pointer by a delta, without reference to where it is.
    fn pointer_by(&mut self, dx: i32, dy: i32) -> Result<()>;

    fn button(&mut self, button: PointerButton, pressed: bool) -> Result<()>;

    /// Scroll, in wheel detents. Fractional values come from trackpads.
    fn scroll(&mut self, dx: f32, dy: f32) -> Result<()>;

    /// Press or release a physical key, given as a HID usage on page 0x07.
    fn key(&mut self, code: KeyCode, pressed: bool) -> Result<()>;

    /// Type a string the client's own IME already composed.
    fn text(&mut self, text: &str) -> Result<()>;
}

/// The geometry a normalised pointer position is measured against.
///
/// The client sends pointer positions as fractions of the streamed picture,
/// because it does not know — and must not need to know — where the host's
/// displays sit relative to each other. Turning a fraction back into a desktop
/// coordinate needs this, and getting it wrong on a multi-monitor host puts
/// the pointer on the wrong screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Screen {
    /// Top-left of the streamed display in the host's virtual desktop.
    pub origin: (i32, i32),
    /// The display's own size, in physical pixels.
    pub resolution: Resolution,
}

impl Screen {
    pub fn new(origin: (i32, i32), resolution: Resolution) -> Screen {
        Screen { origin, resolution }
    }

    /// Where a normalised position lands on the host's desktop.
    ///
    /// Clamped, because the value came from a peer. An unclamped fraction of a
    /// few thousand would put the pointer somewhere no display is, which on
    /// Windows is not an error — it just moves the cursor off every screen.
    pub fn to_desktop(&self, x: f32, y: f32) -> (i32, i32) {
        let across = x.clamp(0.0, 1.0) * self.resolution.width.saturating_sub(1) as f32;
        let down = y.clamp(0.0, 1.0) * self.resolution.height.saturating_sub(1) as f32;
        (
            self.origin.0 + across.round() as i32,
            self.origin.1 + down.round() as i32,
        )
    }

    /// The reverse of [`Screen::to_desktop`]: where a desktop pixel sits on
    /// this display, as a fraction of it. `None` when the pixel is on another
    /// display.
    ///
    /// Uses the same `width - 1` span as the forward direction, so feeding the
    /// result back through `to_desktop` lands on the same pixel. That is what
    /// lets a viewer draw the host's cursor exactly where a click at that spot
    /// would land.
    pub fn from_desktop(&self, x: i32, y: i32) -> Option<(f32, f32)> {
        let across = i64::from(x) - i64::from(self.origin.0);
        let down = i64::from(y) - i64::from(self.origin.1);
        if across < 0
            || down < 0
            || across >= i64::from(self.resolution.width)
            || down >= i64::from(self.resolution.height)
        {
            return None;
        }
        let span_x = i64::from(self.resolution.width.saturating_sub(1)).max(1);
        let span_y = i64::from(self.resolution.height.saturating_sub(1)).max(1);
        Some((across as f32 / span_x as f32, down as f32 / span_y as f32))
    }
}

/// Replay one protocol event on a sink.
///
/// The single place a wire event becomes a platform call, so the normalised
/// coordinate maths lives once rather than in each backend.
///
/// Malformed events are refused rather than clamped. `is_well_formed` has
/// already rejected NaN upstream; repeating the check here means a caller that
/// forgets cannot turn a NaN into a pointer parked in a corner.
pub fn apply(sink: &mut dyn InputSink, event: &InputEvent, screen: Screen) -> Result<()> {
    if !event.is_well_formed() {
        return Err(InputError::Malformed);
    }
    match event {
        InputEvent::PointerMoveAbsolute { x, y } => {
            let (x, y) = screen.to_desktop(*x, *y);
            sink.pointer_to(x, y)
        }
        InputEvent::PointerMoveRelative { dx, dy } => sink.pointer_by(*dx, *dy),
        InputEvent::PointerButton { button, pressed } => sink.button(*button, *pressed),
        InputEvent::Scroll { dx, dy } => sink.scroll(*dx, *dy),
        InputEvent::Key { code, pressed } => sink.key(*code, *pressed),
        InputEvent::Text(text) => sink.text(text),
    }
}

/// The input backend for this platform.
///
/// Fails on platforms with no implementation rather than returning something
/// that silently discards every event — a remote desktop where clicks vanish
/// looks identical to one where the network died, and takes far longer to
/// diagnose.
pub fn sink() -> Result<Box<dyn InputSink>> {
    #[cfg(windows)]
    {
        Ok(Box::new(win32::SendInputSink::new()))
    }
    #[cfg(target_os = "linux")]
    {
        linux::sink()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Err(InputError::Unavailable(
            "input injection is only implemented for Windows and Linux",
        ))
    }
}

// ------------------------------------------------------------------ testing

/// A sink that writes events down instead of performing them.
///
/// Public because `pravera-host` needs it to test dispatch and permission
/// enforcement without moving the developer's actual cursor — a test suite
/// that fights you for the mouse is a test suite nobody runs.
#[derive(Debug, Default)]
pub struct RecordingSink {
    events: Vec<Injected>,
    /// When set, every call fails with this. For testing the error paths that
    /// a real backend only reaches when the platform refuses.
    refusing: Option<&'static str>,
}

/// One thing a [`RecordingSink`] was asked to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Injected {
    PointerTo { x: i32, y: i32 },
    PointerBy { dx: i32, dy: i32 },
    Button { button: PointerButton, down: bool },
    Scroll { dx: f32, dy: f32 },
    Key { code: KeyCode, down: bool },
    Text(String),
}

impl RecordingSink {
    pub fn new() -> RecordingSink {
        RecordingSink::default()
    }

    /// A sink that refuses everything, standing in for a platform that says no.
    pub fn refusing(reason: &'static str) -> RecordingSink {
        RecordingSink {
            events: Vec::new(),
            refusing: Some(reason),
        }
    }

    pub fn events(&self) -> &[Injected] {
        &self.events
    }

    pub fn take(&mut self) -> Vec<Injected> {
        std::mem::take(&mut self.events)
    }

    fn record(&mut self, event: Injected) -> Result<()> {
        match self.refusing {
            Some(reason) => Err(InputError::Refused(reason.to_owned())),
            None => {
                self.events.push(event);
                Ok(())
            }
        }
    }
}

impl InputSink for RecordingSink {
    fn name(&self) -> &'static str {
        "recording"
    }

    fn pointer_to(&mut self, x: i32, y: i32) -> Result<()> {
        self.record(Injected::PointerTo { x, y })
    }

    fn pointer_by(&mut self, dx: i32, dy: i32) -> Result<()> {
        self.record(Injected::PointerBy { dx, dy })
    }

    fn button(&mut self, button: PointerButton, pressed: bool) -> Result<()> {
        self.record(Injected::Button {
            button,
            down: pressed,
        })
    }

    fn scroll(&mut self, dx: f32, dy: f32) -> Result<()> {
        self.record(Injected::Scroll { dx, dy })
    }

    fn key(&mut self, code: KeyCode, pressed: bool) -> Result<()> {
        self.record(Injected::Key {
            code,
            down: pressed,
        })
    }

    fn text(&mut self, text: &str) -> Result<()> {
        self.record(Injected::Text(text.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_desktop_pixel_survives_the_round_trip_through_the_fraction() {
        for screen in [
            Screen::new((0, 0), Resolution::new(1920, 1080)),
            Screen::new((1920, 0), Resolution::new(2560, 1440)),
            Screen::new((-1080, -200), Resolution::new(1080, 1920)),
            Screen::new((0, 0), Resolution::new(3, 3)),
            Screen::new((5, 7), Resolution::new(1, 1)),
        ] {
            let (w, h) = (screen.resolution.width as i32, screen.resolution.height as i32);
            for (dx, dy) in [(0, 0), (w - 1, h - 1), (w / 2, h / 3), (w - 1, 0), (0, h - 1)] {
                let (x, y) = (screen.origin.0 + dx, screen.origin.1 + dy);
                let (fx, fy) = screen.from_desktop(x, y).expect("on the display");
                assert_eq!(screen.to_desktop(fx, fy), (x, y), "{screen:?} at {dx},{dy}");
            }
        }
    }

    #[test]
    fn every_pixel_across_a_row_round_trips() {
        let screen = Screen::new((100, 0), Resolution::new(1366, 768));
        for dx in 0..1366 {
            let (fx, _) = screen.from_desktop(100 + dx, 10).unwrap();
            assert_eq!(screen.to_desktop(fx, 0.0).0, 100 + dx);
        }
    }

    #[test]
    fn a_pixel_off_the_display_has_no_fraction() {
        let screen = Screen::new((1920, 0), Resolution::new(1920, 1080));
        assert_eq!(screen.from_desktop(1919, 5), None);
        assert_eq!(screen.from_desktop(3840, 5), None);
        assert_eq!(screen.from_desktop(2000, -1), None);
        assert_eq!(screen.from_desktop(2000, 1080), None);
        assert!(screen.from_desktop(1920, 0).is_some());
    }

    use super::*;

    const SCREEN: Screen = Screen {
        origin: (0, 0),
        resolution: Resolution::new(1920, 1080),
    };

    #[test]
    fn the_corners_of_a_normalised_position_land_on_the_corners_of_the_screen() {
        assert_eq!(SCREEN.to_desktop(0.0, 0.0), (0, 0));
        assert_eq!(SCREEN.to_desktop(1.0, 1.0), (1919, 1079));
        assert_eq!(SCREEN.to_desktop(0.5, 0.5), (960, 540));
    }

    #[test]
    fn a_second_display_is_offset_by_where_it_sits() {
        let right = Screen::new((1920, 0), Resolution::new(1920, 1080));
        assert_eq!(right.to_desktop(0.0, 0.0), (1920, 0));
        assert_eq!(right.to_desktop(1.0, 0.0), (3839, 0));

        // Displays above or to the left of the primary have negative origins.
        let above = Screen::new((0, -1080), Resolution::new(1920, 1080));
        assert_eq!(above.to_desktop(0.5, 0.0), (960, -1080));
    }

    #[test]
    fn a_position_outside_the_screen_is_pulled_back_onto_it() {
        assert_eq!(SCREEN.to_desktop(-5.0, -5.0), (0, 0));
        assert_eq!(SCREEN.to_desktop(9999.0, 9999.0), (1919, 1079));
        assert_eq!(SCREEN.to_desktop(f32::NAN, 0.5), (0, 540));
    }

    #[test]
    fn a_normalised_position_becomes_a_desktop_pixel() {
        let mut sink = RecordingSink::new();
        apply(
            &mut sink,
            &InputEvent::PointerMoveAbsolute { x: 0.5, y: 0.25 },
            SCREEN,
        )
        .unwrap();

        assert_eq!(sink.events(), [Injected::PointerTo { x: 960, y: 270 }]);
    }

    #[test]
    fn a_position_on_the_second_monitor_lands_on_the_second_monitor() {
        // The bug this guards against is silent: everything works on a
        // single-monitor host and the pointer is on the wrong screen forever
        // on a dual-monitor one.
        let right = Screen::new((1920, 0), Resolution::new(2560, 1440));
        let mut sink = RecordingSink::new();
        apply(
            &mut sink,
            &InputEvent::PointerMoveAbsolute { x: 0.0, y: 1.0 },
            right,
        )
        .unwrap();

        assert_eq!(sink.events(), [Injected::PointerTo { x: 1920, y: 1439 }]);
    }

    #[test]
    fn relative_motion_is_passed_through_untouched() {
        // Deltas are already in host pixels and must not be scaled by the
        // screen — doing so would make mouse sensitivity depend on resolution.
        let mut sink = RecordingSink::new();
        apply(
            &mut sink,
            &InputEvent::PointerMoveRelative { dx: -7, dy: 3 },
            SCREEN,
        )
        .unwrap();

        assert_eq!(sink.events(), [Injected::PointerBy { dx: -7, dy: 3 }]);
    }

    #[test]
    fn every_event_variant_reaches_its_method() {
        let mut sink = RecordingSink::new();
        let events = [
            InputEvent::PointerMoveAbsolute { x: 1.0, y: 1.0 },
            InputEvent::PointerMoveRelative { dx: 1, dy: 1 },
            InputEvent::PointerButton {
                button: PointerButton::Right,
                pressed: true,
            },
            InputEvent::Scroll { dx: 0.0, dy: -2.5 },
            InputEvent::Key {
                code: KeyCode(0x04),
                pressed: false,
            },
            InputEvent::Text("ß".into()),
        ];
        for event in &events {
            apply(&mut sink, event, SCREEN).unwrap();
        }

        assert_eq!(
            sink.take(),
            [
                Injected::PointerTo { x: 1919, y: 1079 },
                Injected::PointerBy { dx: 1, dy: 1 },
                Injected::Button {
                    button: PointerButton::Right,
                    down: true
                },
                Injected::Scroll { dx: 0.0, dy: -2.5 },
                Injected::Key {
                    code: KeyCode(0x04),
                    down: false
                },
                Injected::Text("ß".into()),
            ]
        );
    }

    #[test]
    fn a_malformed_event_is_refused_rather_than_clamped() {
        // These arrive from the network. `is_well_formed` rejects them at the
        // protocol edge too; this is the second line, because a NaN that gets
        // this far turns into a pointer parked in a corner with no error.
        let mut sink = RecordingSink::new();
        for bad in [
            InputEvent::PointerMoveAbsolute {
                x: f32::NAN,
                y: 0.5,
            },
            InputEvent::PointerMoveAbsolute { x: 1.5, y: 0.5 },
            InputEvent::Scroll {
                dx: 0.0,
                dy: f32::INFINITY,
            },
            InputEvent::Text(String::new()),
        ] {
            assert!(
                matches!(apply(&mut sink, &bad, SCREEN), Err(InputError::Malformed)),
                "{bad:?} was accepted"
            );
        }
        assert!(sink.events().is_empty(), "a bad event reached the platform");
    }

    #[test]
    fn a_refusing_platform_reports_rather_than_pretending() {
        let mut sink = RecordingSink::refusing("UIPI");
        let result = apply(
            &mut sink,
            &InputEvent::PointerMoveAbsolute { x: 0.5, y: 0.5 },
            SCREEN,
        );

        assert!(matches!(result, Err(InputError::Refused(_))));
        assert!(sink.events().is_empty());
    }
}
