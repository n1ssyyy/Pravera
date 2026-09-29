//! Screen capture on Windows, via Windows Graphics Capture.
//!
//! WGC rather than Desktop Duplication because it composites correctly on
//! mixed-DPI setups, survives a full-screen exclusive application taking the
//! display, and hands over a D3D11 texture that P7 can import into wgpu
//! without a trip through system memory. Desktop Duplication remains the
//! fallback for the handful of cases WGC refuses, and slots in behind
//! [`crate::CaptureSource`] when it is needed.
//!
//! ## Coordinates and DPI
//!
//! Frame buffers are physical pixels; display positions are in the virtual
//! desktop's coordinate space, which is only physical if the process is
//! per-monitor DPI aware. The host binary declares that awareness in its
//! manifest. A library cannot make that choice for a process it does not own,
//! so this does not try — but a mixed-DPI machine whose host process forgot
//! will report monitor positions that do not line up, and this is where to
//! look when that happens.

// `pub(crate)` for `dda`, which falls back to this enumeration when DXGI's
// own list is enough in geometry but not in friendly names and DPI.
pub(crate) mod monitors;

use std::time::{Duration, Instant};

use bytes::Bytes;
use pravera_core::{PixelFormat, Rect, Resolution};
use tracing::debug;
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use crate::stream::{channel, FrameSink, Runner};
use crate::{
    CaptureError, CaptureOptions, CaptureSource, CapturedFrame, Damage, Display, DisplayId,
    FrameStream, Result,
};

/// One hundred nanoseconds, the unit every WinRT timestamp is counted in.
const TICKS_PER_SECOND: i64 = 10_000_000;

pub(crate) struct WindowsSource;

impl WindowsSource {
    pub(crate) fn new() -> Result<WindowsSource> {
        Ok(WindowsSource)
    }
}

impl CaptureSource for WindowsSource {
    fn name(&self) -> &'static str {
        "windows-graphics-capture"
    }

    fn displays(&self) -> Result<Vec<Display>> {
        Ok(monitors::enumerate()?
            .into_iter()
            .map(|(display, _)| display)
            .collect())
    }

    fn start(&self, id: DisplayId, options: &CaptureOptions) -> Result<FrameStream> {
        let (display, monitor) = monitors::find(id)?;

        let (colour, format) = match options.format {
            PixelFormat::Bgra8 => (ColorFormat::Bgra8, PixelFormat::Bgra8),
            PixelFormat::Rgba8 => (ColorFormat::Rgba8, PixelFormat::Rgba8),
            other => return Err(CaptureError::UnsupportedFormat(other)),
        };

        let interval = options
            .max_fps
            .filter(|fps| *fps > 0)
            .map(|_| options.frame_interval(display.refresh_hz));

        // Try Custom interval / ReportOnly first; some Windows builds
        // (Evercore, pre-24H2) do not support either and fail with
        // "not supported". Falling back to Default for each still captures —
        // just without the optimisation — which is what the pipeline expects
        // when max_fps is None or on pre-24H2.
        //
        // Fast path: once a fallback has fired, remember it process-wide so
        // every later session (and every reconnect) starts with working
        // settings instead of paying 1–2 failed `start_free_threaded` calls.
        // Also skip Custom when the display reports 0Hz (Basic fallback
        // headless): the interval is meaningless and Custom always fails.
        static CUSTOM_SUPPORTED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(true);
        static DIRTY_SUPPORTED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(true);
        use std::sync::atomic::Ordering;
        let want_interval = interval.filter(|_| {
            display.refresh_hz != 0
                && CUSTOM_SUPPORTED.load(Ordering::Relaxed)
        });
        let want_damage =
            options.damage && DIRTY_SUPPORTED.load(Ordering::Relaxed);
        let try_settings = |interval: Option<Duration>, damage: bool| {
            let (sink, pending) = channel(display.clone(), format);
            let settings = Settings::new(
                monitor,
                if options.cursor {
                    CursorCaptureSettings::WithCursor
                } else {
                    CursorCaptureSettings::WithoutCursor
                },
                // The yellow capture border is a privacy affordance for a local
                // recording. On a remote session the person at the keyboard is the
                // one being helped, already knows, and does not need a permanent
                // rectangle drawn around their screen.
                DrawBorderSettings::WithoutBorder,
                SecondaryWindowSettings::Default,
                match interval {
                    Some(interval) => MinimumUpdateIntervalSettings::Custom(interval),
                    None => MinimumUpdateIntervalSettings::Default,
                },
                // `ReportOnly`, never `ReportAndRender`: the latter renders only
                // the changed regions into the texture and leaves the rest of it
                // holding whatever was there before. Every consumer here reads the
                // whole buffer, so that would show as debris from older frames.
                if damage {
                    DirtyRegionSettings::ReportOnly
                } else {
                    DirtyRegionSettings::Default
                },
                colour,
                Flags {
                    sink,
                    display: id,
                    format,
                    damage,
                },
            );
            (settings, pending)
        };

        let control = {
            let mut cur_interval = want_interval;
            let mut cur_damage = want_damage;
            let (mut settings, mut pending) = try_settings(cur_interval, cur_damage);
            let mut attempt = 0;
            loop {
                match Handler::start_free_threaded(settings) {
                    Ok(c) => {
                        let monitor_name = display.name.clone();
                        if attempt == 0 {
                            debug!(
                                display = %id,
                                monitor = %monitor_name,
                                ?format,
                                "windows graphics capture started"
                            );
                        } else {
                            debug!(
                                display = %id,
                                monitor = %monitor_name,
                                ?format,
                                "windows graphics capture started (fallback attempt {attempt})"
                            );
                        }
                        break pending.attach(Box::new(WindowsRunner { control: Some(c) }));
                    }
                    Err(error) => {
                        let text = error.to_string();
                        let interval_err = text.contains("minimum update interval");
                        let dirty_err = text.to_ascii_lowercase().contains("dirty region");
                        // Distinguish permission vs not-supported.
                        if text.contains("Access is denied") || text.contains("access denied") {
                            return Err(CaptureError::PermissionDenied);
                        }
                        // Retry with fallbacks while either flag can be relaxed.
                        // Latch the result process-wide so later sessions skip
                        // the failing setting outright (see fast path above).
                        if interval_err && cur_interval.is_some() {
                            tracing::warn!(%text, "WGC Custom interval not supported; retrying with Default");
                            CUSTOM_SUPPORTED.store(false, Ordering::Relaxed);
                            cur_interval = None;
                            (settings, pending) = try_settings(cur_interval, cur_damage);
                            attempt += 1;
                            continue;
                        }
                        if dirty_err && cur_damage {
                            tracing::warn!(%text, "WGC Dirty region not supported on this platform; retrying with Default");
                            DIRTY_SUPPORTED.store(false, Ordering::Relaxed);
                            cur_damage = false;
                            (settings, pending) = try_settings(cur_interval, cur_damage);
                            attempt += 1;
                            continue;
                        }
                        // Generic "not supported" without more detail — try both defaults if not already.
                        let generic_not_supported = text.contains("not supported") || text.contains("NotSupported");
                        if generic_not_supported && (cur_interval.is_some() || cur_damage) {
                            if cur_interval.is_some() {
                                tracing::warn!(%text, "WGC setting not supported; retrying with Default interval+damage");
                                CUSTOM_SUPPORTED.store(false, Ordering::Relaxed);
                                cur_interval = None;
                            }
                            if cur_damage {
                                DIRTY_SUPPORTED.store(false, Ordering::Relaxed);
                                cur_damage = false;
                            }
                            (settings, pending) = try_settings(cur_interval, cur_damage);
                            attempt += 1;
                            // Only one generic retry to avoid loop.
                            if attempt < 3 {
                                continue;
                            }
                        }
                        return Err(CaptureError::backend(text));
                    }
                }
            }
        };
        Ok(control)
    }
}

struct WindowsRunner {
    control: Option<CaptureControl<Handler, CaptureError>>,
}

impl Runner for WindowsRunner {
    fn stop(&mut self) {
        if let Some(control) = self.control.take() {
            // Posts WM_QUIT to the capture thread and joins it. An error here
            // means the thread was already gone, which is the state we wanted.
            if let Err(error) = control.stop() {
                debug!(%error, "capture thread did not stop cleanly");
            }
        }
    }
}

/// What the handler needs, carried through `Settings` to the capture thread.
struct Flags {
    sink: FrameSink,
    display: DisplayId,
    format: PixelFormat,
    damage: bool,
}

struct Handler {
    sink: FrameSink,
    display: DisplayId,
    format: PixelFormat,
    damage: bool,
    started: Instant,
    /// The compositor's timestamp for the first frame, so later frames can be
    /// expressed relative to it.
    origin: Option<i64>,
    /// Reused between frames to unpad a padded staging texture.
    scratch: Vec<u8>,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = Flags;
    type Error = CaptureError;

    fn new(ctx: Context<Self::Flags>) -> std::result::Result<Self, Self::Error> {
        Ok(Handler {
            sink: ctx.flags.sink,
            display: ctx.flags.display,
            format: ctx.flags.format,
            damage: ctx.flags.damage,
            started: Instant::now(),
            origin: None,
            scratch: Vec::new(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> std::result::Result<(), Self::Error> {
        // Cheapest possible exit when the session has ended: no copy, no
        // conversion, just stop. This runs on a compositor callback, so
        // anything done here delays the next frame of the whole desktop.
        if !self.sink.is_listening() {
            control.stop();
            return Ok(());
        }

        let resolution = Resolution::new(frame.width(), frame.height());
        let elapsed = self.elapsed(frame);
        let damage = self.damage_of(frame, resolution);

        let buffer = frame.buffer().map_err(CaptureError::backend)?;
        let stride = resolution.width as usize * 4;

        // `as_nopadding_buffer` returns the mapped texture directly when its
        // rows are already tight, and only falls back to `scratch` when the
        // driver padded them. Either way the result is copied once into the
        // frame, because the mapping ends when this function returns.
        let pixels = Bytes::copy_from_slice(buffer.as_nopadding_buffer(&mut self.scratch));

        let captured = CapturedFrame {
            display: self.display,
            format: self.format,
            resolution,
            stride,
            pixels,
            elapsed,
            damage,
        };
        debug_assert!(
            captured.is_consistent(),
            "capture produced a frame that does not match its own stride: {captured:?}"
        );

        if !self.sink.put(captured) {
            control.stop();
        }
        Ok(())
    }

    /// The captured display went away: unplugged, or the session ended.
    fn on_closed(&mut self) -> std::result::Result<(), Self::Error> {
        debug!(display = %self.display, "capture item closed");
        self.sink.fail(CaptureError::Lost);
        Ok(())
    }
}

impl Drop for Handler {
    fn drop(&mut self) {
        // The capture thread is unwinding, so nothing more will arrive. This
        // is a no-op when the stream already ended for a stated reason: a
        // reported failure outranks a tidy ending.
        self.sink.end();
    }
}

impl Handler {
    /// Time since the first frame of this capture.
    ///
    /// Taken from the compositor's own clock when it will give one. That
    /// measures when the frame was *produced*, which is what pacing wants;
    /// wall-clock time here would also fold in however long this callback
    /// waited to be scheduled.
    fn elapsed(&mut self, frame: &Frame) -> Duration {
        let Ok(timestamp) = frame.timestamp().map(|span| span.Duration) else {
            return self.started.elapsed();
        };

        let origin = *self.origin.get_or_insert(timestamp);
        let ticks = timestamp.saturating_sub(origin).max(0);

        Duration::new(
            (ticks / TICKS_PER_SECOND) as u64,
            (ticks % TICKS_PER_SECOND) as u32 * 100,
        )
    }

    fn damage_of(&self, frame: &Frame, resolution: Resolution) -> Damage {
        if !self.damage {
            return Damage::Full;
        }

        match frame.dirty_regions() {
            Ok(regions) => Damage::regions(
                regions
                    .into_iter()
                    .filter_map(|region| clamp(region, resolution))
                    .collect(),
            ),
            // Windows before 24H2 has no dirty-region reporting at all. A full
            // repaint is always correct, just less efficient.
            Err(_) => Damage::Full,
        }
    }
}

/// Bring a reported region inside the frame.
///
/// The rectangles come back signed and are trusted no further than that:
/// anything reaching outside the buffer would become an out-of-bounds read in
/// the encoder, so it is clipped here rather than validated three crates
/// later.
fn clamp(region: windows_capture::frame::DirtyRegion, resolution: Resolution) -> Option<Rect> {
    let left = region.x.max(0) as u32;
    let top = region.y.max(0) as u32;
    let right = region
        .x
        .saturating_add(region.width.max(0))
        .max(0)
        .min(resolution.width as i32) as u32;
    let bottom = region
        .y
        .saturating_add(region.height.max(0))
        .max(0)
        .min(resolution.height as i32) as u32;

    (right > left && bottom > top).then(|| Rect::new(left, top, right - left, bottom - top))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_capture::frame::DirtyRegion;

    const HD: Resolution = Resolution::new(1920, 1080);

    fn region(x: i32, y: i32, width: i32, height: i32) -> DirtyRegion {
        DirtyRegion {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn an_ordinary_region_survives_unchanged() {
        assert_eq!(
            clamp(region(10, 20, 30, 40), HD),
            Some(Rect::new(10, 20, 30, 40))
        );
    }

    #[test]
    fn a_region_running_off_the_edge_is_clipped_to_the_buffer() {
        // A rectangle wider than the frame would index past the end of the
        // pixel buffer in whichever stage trusted it.
        assert_eq!(
            clamp(region(1900, 1070, 100, 100), HD),
            Some(Rect::new(1900, 1070, 20, 10))
        );
    }

    #[test]
    fn a_region_starting_off_screen_is_pulled_back_in() {
        assert_eq!(
            clamp(region(-50, -50, 100, 100), HD),
            Some(Rect::new(0, 0, 50, 50))
        );
    }

    #[test]
    fn a_region_entirely_outside_the_frame_is_dropped() {
        assert_eq!(clamp(region(5000, 5000, 10, 10), HD), None);
        assert_eq!(clamp(region(-100, 0, 50, 50), HD), None);
    }

    #[test]
    fn a_degenerate_region_is_dropped_rather_than_encoded() {
        assert_eq!(clamp(region(10, 10, 0, 40), HD), None);
        assert_eq!(clamp(region(10, 10, 40, -5), HD), None);
    }
}
