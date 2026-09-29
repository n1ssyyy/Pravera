//! Getting pixels off a screen.
//!
//! One trait, [`CaptureSource`], and a backend per platform behind it. The
//! trait exists because there is genuinely more than one implementation:
//! Windows Graphics Capture today and Desktop Duplication for the secure
//! desktop, PipeWire on Wayland and XSHM on X11. [`SyntheticSource`] exists
//! only as a test harness (pipeline unit tests); it is never used as a
//! runtime fallback — a headless machine gets a real IDD virtual display or
//! an honest refusal, never a test pattern.
//!
//! ```no_run
//! use std::time::Duration;
//! use pravera_capture::{CaptureOptions, DisplayId, Recv};
//!
//! let source = pravera_capture::source()?;
//! let displays = source.displays()?;
//! println!("capturing {}", displays[0].name);
//!
//! let stream = source.start(DisplayId::PRIMARY, &CaptureOptions::default())?;
//! loop {
//!     match stream.recv_timeout(Duration::from_millis(250))? {
//!         Recv::Frame(frame) => println!("{} bytes", frame.pixels.len()),
//!         Recv::Idle => continue,   // a still desktop produces no frames
//!         Recv::Ended => break,
//!     }
//! }
//! # Ok::<(), pravera_capture::CaptureError>(())
//! ```
//!
//! ## What this layer does not do
//!
//! No colour conversion, no scaling, no encoding. A backend hands over the
//! pixels the platform gave it, in a packed 8-bit layout, and says what
//! changed. Everything else belongs to `pravera-codec`, which is where the
//! hardware that can do those conversions for free actually lives.
//!
//! Nothing here knows a network exists either. Capture has no notion of a
//! peer, a permission or a wire format, and `pravera-host` does the mapping
//! from [`Display`] to the protocol's monitor list. That keeps the one piece
//! of code touching the GPU free of anything a remote party can influence.

mod display;
mod error;
mod frame;
mod stream;

// `pub` rather than wired in: DDA is the backend the service will drive from
// session 0 for the secure desktop, and no in-tree caller picks it yet. A
// public module compiles and cannot rot while the wiring catches up.
pub mod dda;

pub mod idd;

pub mod synthetic;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod win32;

use std::time::Duration;

use pravera_core::PixelFormat;

pub use display::{Display, DisplayId, MAX_DISPLAYS};
pub use error::{CaptureError, Result};
pub use frame::{CapturedFrame, Damage};
pub use idd::{
    add_virtual_display, diagnostics as virtual_display_diagnostics, ensure_at_least_one_display,
    ensure_virtual_display, find_bundled_package, has_bundled_package, idd_package_dir,
    install_from_dir, is_elevated, is_idd_installed, is_idd_usable, needs_elevation_message,
    needs_virtual_display, try_auto_install, virtual_display_status, IddFallbackSource,
    VirtualDisplayStatus,
};
pub use stream::{CaptureStats, FrameStream, Recv};
pub use synthetic::SyntheticSource;

/// Set this to `synthetic` to capture an invented picture instead of the
/// screen.
///
/// Test harness only (reproducible benchmarks, pipeline tests without a
/// display server). It logs a warning every time it takes effect, because a
/// stream showing a test pattern instead of the remote desktop is never what
/// a real session should do.
pub const BACKEND_OVERRIDE: &str = "PRAVERA_CAPTURE";

/// A thing that can capture screens.
///
/// `Send + Sync` because the service holds one for the lifetime of the process
/// and starts captures from whichever task asked.
pub trait CaptureSource: Send + Sync {
    /// Short backend name, for logs and the settings screen.
    fn name(&self) -> &'static str;

    /// Every display this backend can capture, primary first.
    ///
    /// Re-enumerated on each call rather than cached: monitors get unplugged,
    /// laptops get docked, and a stale list means offering the operator a
    /// screen that is no longer there.
    fn displays(&self) -> Result<Vec<Display>>;

    /// Begin capturing one display.
    ///
    /// The returned stream owns the backend's capture thread; dropping it
    /// stops the capture. Several streams may run at once — that is what
    /// multi-monitor sessions need — and each is independent.
    fn start(&self, display: DisplayId, options: &CaptureOptions) -> Result<FrameStream>;

    /// The primary display, or an error when the machine has none.
    fn primary(&self) -> Result<Display> {
        self.displays()?
            .into_iter()
            .next()
            .ok_or(CaptureError::NoDisplays)
    }
}

/// How to capture.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureOptions {
    /// Packed layout to produce. Only [`PixelFormat::Bgra8`] and
    /// [`PixelFormat::Rgba8`] are capture outputs; anything else is refused
    /// rather than silently substituted.
    pub format: PixelFormat,

    /// Draw the mouse pointer into the frame.
    ///
    /// On by default because the alternative — sending the pointer position
    /// separately and drawing it client-side — needs the cursor bitmap, the
    /// hotspot and a compositing pass that does not exist yet. It is the
    /// better answer for latency and arrives with the P7 tuning work.
    pub cursor: bool,

    /// Upper bound on frames per second, or `None` to follow the display.
    ///
    /// A ceiling, not a target: nothing here manufactures frames for a screen
    /// that is not changing.
    pub max_fps: Option<u32>,

    /// Ask the platform which regions changed.
    ///
    /// Worth turning off only for an encoder that cannot use the information,
    /// since collecting it is not free on every backend.
    pub damage: bool,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        CaptureOptions {
            format: PixelFormat::Bgra8,
            cursor: true,
            max_fps: None,
            damage: true,
        }
    }
}

impl CaptureOptions {
    /// Shortest interval between frames, given what the display can do.
    ///
    /// Falls back to 60 Hz when the display will not say — a made-up number,
    /// but one that is wrong in the harmless direction: capturing slower than
    /// the panel costs smoothness, capturing faster costs work that is thrown
    /// away.
    pub fn frame_interval(&self, refresh_hz: u32) -> Duration {
        let hz = match (self.max_fps, refresh_hz) {
            (Some(cap), 0) if cap > 0 => cap,
            (Some(cap), refresh) if cap > 0 => cap.min(refresh),
            (_, 0) => 60,
            (_, refresh) => refresh,
        };
        Duration::from_secs_f64(1.0 / hz as f64)
    }
}

/// The capture backend for this machine.
///
/// Returns [`CaptureError::Unavailable`] on a platform with no backend yet,
/// which the caller should surface as "this host cannot be shared" rather than
/// as a crash — the rest of Pravera still works on such a machine, it just
/// cannot be the host end of a session.
///
/// On Windows a headless machine gets a real IDD virtual display via
/// [`ensure_virtual_display`](crate::idd::ensure_virtual_display) (see
/// [`crate::idd`]); there is deliberately no synthetic fallback. When the
/// platform reports zero displays the caller gets `NoDisplays` and hosting
/// refuses with [`NO_DISPLAY`](https://github.com/n1ssyyy/Pravera)
/// guidance instead of streaming a test pattern.
pub fn source() -> Result<Box<dyn CaptureSource>> {
    if let Ok(requested) = std::env::var(BACKEND_OVERRIDE) {
        if requested.eq_ignore_ascii_case("synthetic") {
            tracing::warn!(
                "{BACKEND_OVERRIDE}=synthetic: streaming a test pattern, not this screen (test harness only)"
            );
            return Ok(Box::new(SyntheticSource::new(
                pravera_core::Resolution::new(1920, 1080),
                60,
            )));
        }
        tracing::warn!(backend = %requested, "ignoring unknown {BACKEND_OVERRIDE} value");
    }

    platform_source()
}

#[cfg(windows)]
fn platform_source() -> Result<Box<dyn CaptureSource>> {
    Ok(Box::new(win32::WindowsSource::new()?))
}

#[cfg(target_os = "linux")]
fn platform_source() -> Result<Box<dyn CaptureSource>> {
    linux::source()
}

#[cfg(not(any(windows, target_os = "linux")))]
fn platform_source() -> Result<Box<dyn CaptureSource>> {
    Err(CaptureError::Unavailable(
        "Pravera hosts sessions on Windows and Linux only",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_uncapped_capture_follows_the_display() {
        let options = CaptureOptions::default();
        assert_eq!(
            options.frame_interval(120),
            Duration::from_secs_f64(1.0 / 120.0)
        );
    }

    #[test]
    fn a_cap_below_the_refresh_rate_wins() {
        let options = CaptureOptions {
            max_fps: Some(30),
            ..CaptureOptions::default()
        };
        assert_eq!(
            options.frame_interval(144),
            Duration::from_secs_f64(1.0 / 30.0)
        );
    }

    #[test]
    fn a_cap_above_the_refresh_rate_does_not_invent_frames() {
        // Asking for 240 fps from a 60 Hz panel would just capture the same
        // picture four times.
        let options = CaptureOptions {
            max_fps: Some(240),
            ..CaptureOptions::default()
        };
        assert_eq!(
            options.frame_interval(60),
            Duration::from_secs_f64(1.0 / 60.0)
        );
    }

    #[test]
    fn a_display_that_will_not_report_its_refresh_rate_still_gets_a_cadence() {
        assert_eq!(
            CaptureOptions::default().frame_interval(0),
            Duration::from_secs_f64(1.0 / 60.0)
        );
        let capped = CaptureOptions {
            max_fps: Some(15),
            ..CaptureOptions::default()
        };
        assert_eq!(
            capped.frame_interval(0),
            Duration::from_secs_f64(1.0 / 15.0)
        );
    }

    #[test]
    fn a_nonsensical_cap_is_ignored_rather_than_dividing_by_zero() {
        let options = CaptureOptions {
            max_fps: Some(0),
            ..CaptureOptions::default()
        };
        assert_eq!(
            options.frame_interval(60),
            Duration::from_secs_f64(1.0 / 60.0)
        );
        assert!(options.frame_interval(0) > Duration::ZERO);
    }
}
