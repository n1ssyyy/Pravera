//! A capture source that invents its own picture.
//!
//! Not a mock. It runs the same thread, the same mailbox, the same copy and
//! the same damage bookkeeping as a real backend, and it is the only source
//! that works on a build machine with no GPU, no display and no compositor.
//! That makes it the substrate for the pipeline tests and for `pravera-bench`,
//! where a reproducible picture matters more than a real one.
//!
//! ## What it draws, and why that
//!
//! Three solid squares in pure red, green and blue, and one bright bar
//! sweeping left to right over a vertical gradient. Each element exists to
//! make one specific class of bug visible the instant a person looks at the
//! stream:
//!
//! - the coloured squares fail loudly on a channel-order mistake, which is the
//!   most common way a BGRA capture path goes wrong and the hardest to spot in
//!   a hex dump;
//! - the gradient runs dark at the top, so a vertically flipped frame is
//!   obvious rather than merely odd;
//! - the bar's position is a direct function of elapsed time, so a stalled,
//!   duplicated or reordered frame shows up as a stutter instead of hiding in
//!   an average frame-rate number.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use bytes::Bytes;
use pravera_core::{PixelFormat, Rect, Resolution};

use crate::stream::{channel, FrameSink, Runner};
use crate::{
    CaptureError, CaptureOptions, CaptureSource, CapturedFrame, Damage, Display, DisplayId,
    FrameStream, Result,
};

/// How long the bar takes to cross the screen once.
const SWEEP: Duration = Duration::from_secs(2);

/// Width of the sweeping bar, in pixels.
const BAR_WIDTH: u32 = 48;

/// Edge length of each corner marker, in pixels.
const MARKER: u32 = 64;

/// Longest the generator sleeps before re-checking whether it should stop.
///
/// Without a cap, a one-frame-per-second capture would keep `stop()` waiting a
/// full second, and a caller closing a session would feel it.
const TICK: Duration = Duration::from_millis(20);

/// A source of invented displays.
#[derive(Debug, Clone)]
pub struct SyntheticSource {
    displays: Vec<Display>,
    still: bool,
}

impl SyntheticSource {
    /// One display of the given size.
    pub fn new(resolution: Resolution, refresh_hz: u32) -> SyntheticSource {
        SyntheticSource::multi(1, resolution, refresh_hz)
    }

    /// One display that is painted once and then never changes again.
    ///
    /// A desktop nobody is touching, which is the state an unattended host
    /// spends nearly all its time in and the one the sweeping bar cannot
    /// model. Capture backends deliver on change, so such a screen produces
    /// exactly one frame and then goes quiet forever — and anything
    /// downstream that only acts when a frame arrives stops acting.
    pub fn still(resolution: Resolution) -> SyntheticSource {
        SyntheticSource {
            still: true,
            ..SyntheticSource::multi(1, resolution, 60)
        }
    }

    /// `count` identical displays laid out left to right, the first primary.
    ///
    /// The layout is what makes this useful beyond a single-screen smoke test:
    /// multi-monitor bugs are almost always about the arrangement rather than
    /// the pixels, and this produces a real arrangement to get wrong.
    pub fn multi(count: u8, resolution: Resolution, refresh_hz: u32) -> SyntheticSource {
        let displays = (0..count.max(1))
            .map(|index| Display {
                id: DisplayId(index),
                name: format!("Synthetic {}", index + 1),
                resolution,
                position: (index as i32 * resolution.width as i32, 0),
                scale: 1.0,
                primary: index == 0,
                refresh_hz,
            })
            .collect();

        SyntheticSource {
            displays: Display::normalise(displays),
            still: false,
        }
    }
}

impl CaptureSource for SyntheticSource {
    fn name(&self) -> &'static str {
        "synthetic"
    }

    fn displays(&self) -> Result<Vec<Display>> {
        Ok(self.displays.clone())
    }

    fn start(&self, id: DisplayId, options: &CaptureOptions) -> Result<FrameStream> {
        let display = self
            .displays
            .iter()
            .find(|display| display.id == id)
            .cloned()
            .ok_or(CaptureError::NoSuchDisplay(id))?;

        let format = match options.format {
            format @ (PixelFormat::Bgra8 | PixelFormat::Rgba8) => format,
            other => return Err(CaptureError::UnsupportedFormat(other)),
        };

        let interval = options.frame_interval(display.refresh_hz);
        let track_damage = options.damage;
        let resolution = display.resolution;
        let still = self.still;

        let (sink, pending) = channel(display, format);
        let halt = Arc::new(AtomicBool::new(false));

        let thread_halt = halt.clone();
        let handle = thread::Builder::new()
            .name("pravera-synthetic".into())
            .spawn(move || {
                generate(
                    sink,
                    thread_halt,
                    id,
                    resolution,
                    format,
                    interval,
                    track_damage,
                    still,
                )
            })
            .map_err(CaptureError::backend)?;

        Ok(pending.attach(Box::new(SyntheticRunner {
            halt,
            handle: Some(handle),
        })))
    }
}

struct SyntheticRunner {
    halt: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Runner for SyntheticRunner {
    fn stop(&mut self) {
        self.halt.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn generate(
    sink: FrameSink,
    halt: Arc<AtomicBool>,
    display: DisplayId,
    resolution: Resolution,
    format: PixelFormat,
    interval: Duration,
    track_damage: bool,
    still: bool,
) {
    let mut canvas = Canvas::new(resolution, format, still);
    let started = Instant::now();
    let mut next = started;

    loop {
        if halt.load(Ordering::Acquire) {
            break;
        }

        let now = Instant::now();
        if now < next {
            // Sleep in bounded slices so a stop request is not held up by a
            // long frame interval.
            thread::sleep((next - now).min(TICK));
            continue;
        }

        let elapsed = now.duration_since(started);

        // Advance from the previous deadline rather than from now, so the
        // cadence does not drift by however long painting took. If painting
        // fell more than a whole interval behind, give up on catching up and
        // resynchronise: chasing a backlog only makes the next frame later.
        next += interval;
        if next < now {
            next = now + interval;
        }

        // Nothing moved since the last frame, so there is no frame to send.
        // Real backends behave exactly this way — a still desktop produces no
        // callbacks at all — and manufacturing a duplicate here would hide
        // every downstream bug that only shows up when frames stop arriving.
        let Some((pixels, damage)) = canvas.paint(elapsed) else {
            continue;
        };
        let damage = if track_damage { damage } else { Damage::Full };

        let frame = CapturedFrame {
            display,
            format,
            resolution,
            stride: canvas.stride,
            pixels,
            elapsed,
            damage,
        };

        if !sink.put(frame) {
            break;
        }
    }

    sink.end();
}

/// The framebuffer being drawn into, plus what changed in it.
struct Canvas {
    resolution: Resolution,
    format: PixelFormat,
    stride: usize,
    buffer: Vec<u8>,
    previous_bar: Option<Rect>,
    /// Hold the bar still, so the picture never changes after the first frame.
    still: bool,
}

impl Canvas {
    fn new(resolution: Resolution, format: PixelFormat, still: bool) -> Canvas {
        let stride = resolution.width as usize * 4;
        let mut canvas = Canvas {
            resolution,
            format,
            stride,
            buffer: vec![0u8; stride * resolution.height as usize],
            previous_bar: None,
            still,
        };
        canvas.paint_background(Rect::covering(resolution));
        canvas
    }

    /// Paint the picture for this instant, or `None` if it is the one already
    /// in the buffer.
    fn paint(&mut self, elapsed: Duration) -> Option<(Bytes, Damage)> {
        let bar = self.bar_at(elapsed);

        let damage = match self.previous_bar {
            // The first frame has no history, so nothing about it can be
            // described as a change.
            None => Damage::Full,
            // The bar has not advanced a whole pixel yet. Nothing on screen is
            // different, so there is nothing to send.
            Some(previous) if previous == bar => return None,
            Some(previous) => {
                self.paint_background(previous);
                Damage::regions(vec![previous, bar])
            }
        };
        self.previous_bar = Some(bar);

        self.fill(bar, [0xf5, 0xf5, 0xf5]);
        // Markers go on top of the bar, not under it. They are the channel
        // reference; a diagnostic that disappears for a fifth of every sweep
        // is one you cannot rely on when you need it.
        self.paint_markers(bar);

        Some((Bytes::copy_from_slice(&self.buffer), damage))
    }

    /// Where the bar sits at this instant. A sawtooth: it crosses the screen,
    /// jumps back, and crosses again.
    fn bar_at(&self, elapsed: Duration) -> Rect {
        let width = self.bar_width();
        let travel = self.resolution.width.saturating_sub(width);
        // A still screen parks the bar at the start and leaves it there, so
        // every frame after the first is identical to the one before it and
        // `paint` stops producing anything at all.
        let phase = if self.still {
            0.0
        } else {
            (elapsed.as_micros() % SWEEP.as_micros()) as f64 / SWEEP.as_micros() as f64
        };
        let x = (phase * travel as f64) as u32;
        Rect::new(x, 0, width, self.resolution.height)
    }

    /// Narrow enough to leave room to travel. Without the halving, a display
    /// no wider than the bar would hold still forever and the source would go
    /// permanently idle after one frame.
    fn bar_width(&self) -> u32 {
        BAR_WIDTH.min(self.resolution.width.max(2) / 2).max(1)
    }

    fn paint_background(&mut self, area: Rect) {
        let height = self.resolution.height.max(1);
        for y in area.y..(area.y + area.height).min(self.resolution.height) {
            // Dark at the top: a vertically flipped frame reads as "lit from
            // the wrong end" rather than as a plausible picture.
            let shade = 8 + (y * 40 / height) as u8;
            self.fill_row(y, area.x, area.width, [shade, shade, shade + 4]);
        }
        self.paint_markers(area);
    }

    /// Pure-channel squares in three corners.
    fn paint_markers(&mut self, area: Rect) {
        let Resolution { width, height } = self.resolution;
        let edge = MARKER.min(width / 4).min(height / 4).max(1);
        let inset = edge / 2;

        let markers = [
            (Rect::new(inset, inset, edge, edge), [0xff, 0x00, 0x00]),
            (
                Rect::new(width.saturating_sub(inset + edge), inset, edge, edge),
                [0x00, 0xff, 0x00],
            ),
            (
                Rect::new(inset, height.saturating_sub(inset + edge), edge, edge),
                [0x00, 0x00, 0xff],
            ),
        ];

        for (marker, colour) in markers {
            if let Some(visible) = intersect(marker, area) {
                self.fill(visible, colour);
            }
        }
    }

    fn fill(&mut self, area: Rect, rgb: [u8; 3]) {
        for y in area.y..(area.y + area.height).min(self.resolution.height) {
            self.fill_row(y, area.x, area.width, rgb);
        }
    }

    fn fill_row(&mut self, y: u32, x: u32, width: u32, rgb: [u8; 3]) {
        let pixel = encode(self.format, rgb);
        let end = (x + width).min(self.resolution.width);
        let row = y as usize * self.stride;
        for column in x..end {
            let at = row + column as usize * 4;
            self.buffer[at..at + 4].copy_from_slice(&pixel);
        }
    }
}

/// Lay out one pixel in the requested channel order.
fn encode(format: PixelFormat, [r, g, b]: [u8; 3]) -> [u8; 4] {
    match format {
        PixelFormat::Bgra8 => [b, g, r, 0xff],
        _ => [r, g, b, 0xff],
    }
}

fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);

    (right > x && bottom > y).then(|| Rect::new(x, y, right - x, bottom - y))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Recv;

    const SMALL: Resolution = Resolution::new(320, 240);

    fn pixel_at(frame: &CapturedFrame, x: u32, y: u32) -> [u8; 4] {
        let at = y as usize * frame.stride + x as usize * 4;
        frame.pixels[at..at + 4].try_into().unwrap()
    }

    fn first_frame(source: &SyntheticSource, options: &CaptureOptions) -> CapturedFrame {
        let stream = source.start(DisplayId::PRIMARY, options).unwrap();
        loop {
            match stream.recv_timeout(Duration::from_secs(5)) {
                Ok(Recv::Frame(frame)) => return frame,
                Ok(Recv::Idle) => continue,
                other => panic!("expected a frame, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_synthetic_display_looks_like_a_real_one() {
        let source = SyntheticSource::new(SMALL, 60);
        let displays = source.displays().unwrap();

        assert_eq!(displays.len(), 1);
        assert_eq!(displays[0].id, DisplayId::PRIMARY);
        assert!(displays[0].primary);
        assert_eq!(displays[0].resolution, SMALL);
    }

    #[test]
    fn several_displays_are_laid_out_side_by_side_with_one_primary() {
        let displays = SyntheticSource::multi(3, SMALL, 60).displays().unwrap();

        assert_eq!(displays.len(), 3);
        assert_eq!(displays.iter().filter(|d| d.primary).count(), 1);
        assert_eq!(displays[1].position, (SMALL.width as i32, 0));
        assert_eq!(displays[2].position, (SMALL.width as i32 * 2, 0));
    }

    #[test]
    fn asking_for_a_display_that_is_not_there_fails_rather_than_substituting_one() {
        let source = SyntheticSource::new(SMALL, 60);
        let outcome = source.start(DisplayId(7), &CaptureOptions::default());
        assert!(matches!(
            outcome,
            Err(CaptureError::NoSuchDisplay(DisplayId(7)))
        ));
    }

    #[test]
    fn capture_will_not_pretend_to_produce_encoder_input() {
        // NV12 is the encoder's output of a colour conversion, not something a
        // screen-capture API hands over. Silently returning BGRA instead would
        // be discovered as a garbled picture rather than as an error.
        let source = SyntheticSource::new(SMALL, 60);
        let options = CaptureOptions {
            format: PixelFormat::Nv12,
            ..CaptureOptions::default()
        };
        assert!(matches!(
            source.start(DisplayId::PRIMARY, &options),
            Err(CaptureError::UnsupportedFormat(PixelFormat::Nv12))
        ));
    }

    #[test]
    fn every_frame_describes_its_own_buffer_correctly() {
        let frame = first_frame(&SyntheticSource::new(SMALL, 60), &CaptureOptions::default());

        assert!(frame.is_consistent(), "{frame:?}");
        assert_eq!(frame.resolution, SMALL);
        assert_eq!(frame.stride, SMALL.width as usize * 4);
        assert_eq!(frame.display, DisplayId::PRIMARY);
    }

    #[test]
    fn the_red_marker_really_is_red_in_both_channel_orders() {
        // The whole reason the marker exists. If this passes for BGRA and
        // fails for RGBA, the writer is ignoring the requested format.
        let source = SyntheticSource::new(SMALL, 60);
        let inset = MARKER.min(SMALL.width / 4).min(SMALL.height / 4) / 2;
        let probe = (inset + 4, inset + 4);

        let bgra = first_frame(
            &source,
            &CaptureOptions {
                format: PixelFormat::Bgra8,
                ..CaptureOptions::default()
            },
        );
        assert_eq!(pixel_at(&bgra, probe.0, probe.1), [0x00, 0x00, 0xff, 0xff]);

        let rgba = first_frame(
            &source,
            &CaptureOptions {
                format: PixelFormat::Rgba8,
                ..CaptureOptions::default()
            },
        );
        assert_eq!(pixel_at(&rgba, probe.0, probe.1), [0xff, 0x00, 0x00, 0xff]);
    }

    #[test]
    fn the_first_frame_of_a_session_is_a_full_repaint() {
        // Nothing preceded it, so there is no change to describe and the
        // viewer has nothing on screen to keep.
        let frame = first_frame(&SyntheticSource::new(SMALL, 60), &CaptureOptions::default());
        assert_eq!(frame.damage, Damage::Full);
    }

    #[test]
    fn later_frames_report_only_the_strips_the_bar_touched() {
        let source = SyntheticSource::new(SMALL, 240);
        let stream = source
            .start(DisplayId::PRIMARY, &CaptureOptions::default())
            .unwrap();

        let mut partial = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match stream.recv_timeout(Duration::from_millis(200)) {
                Ok(Recv::Frame(frame)) if !frame.damage.is_full() => {
                    partial = Some(frame);
                    break;
                }
                Ok(Recv::Frame(_)) | Ok(Recv::Idle) => continue,
                other => panic!("unexpected {other:?}"),
            }
        }

        let frame = partial.expect("no frame reported partial damage");
        assert!(
            frame.damage.area(SMALL) < SMALL.pixels(),
            "damage covered the whole screen: {:?}",
            frame.damage
        );
    }

    #[test]
    fn damage_tracking_can_be_turned_off_for_encoders_that_cannot_use_it() {
        let source = SyntheticSource::new(SMALL, 240);
        let options = CaptureOptions {
            damage: false,
            ..CaptureOptions::default()
        };
        let stream = source.start(DisplayId::PRIMARY, &options).unwrap();

        let mut seen = 0;
        let deadline = Instant::now() + Duration::from_secs(5);
        while seen < 3 && Instant::now() < deadline {
            if let Ok(Recv::Frame(frame)) = stream.recv_timeout(Duration::from_millis(200)) {
                assert_eq!(frame.damage, Damage::Full);
                seen += 1;
            }
        }
        assert_eq!(seen, 3, "did not see enough frames to judge");
    }

    #[test]
    fn the_bar_moves_between_frames() {
        // A source that returns the same picture forever would satisfy every
        // other test in this file.
        let source = SyntheticSource::new(SMALL, 240);
        let stream = source
            .start(DisplayId::PRIMARY, &CaptureOptions::default())
            .unwrap();

        let mut frames = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while frames.len() < 2 && Instant::now() < deadline {
            if let Ok(Recv::Frame(frame)) = stream.recv_timeout(Duration::from_millis(200)) {
                frames.push(frame);
            }
        }

        assert_eq!(frames.len(), 2, "did not receive two frames");
        assert_ne!(
            frames[0].pixels, frames[1].pixels,
            "the picture never changed"
        );
        assert!(frames[1].elapsed > frames[0].elapsed);
    }

    #[test]
    fn a_picture_that_has_not_changed_is_not_sent_again() {
        // The bar advances a fraction of a pixel between frames at this rate,
        // so most ticks produce nothing. A source that sent duplicates anyway
        // would hide every bug that only appears when frames stop arriving.
        let source = SyntheticSource::new(Resolution::new(64, 64), 1000);
        let stream = source
            .start(DisplayId::PRIMARY, &CaptureOptions::default())
            .unwrap();

        let mut frames = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < deadline {
            match stream.recv_timeout(Duration::from_millis(50)) {
                Ok(Recv::Frame(frame)) => frames.push(frame.pixels),
                Ok(Recv::Idle) => continue,
                other => panic!("unexpected {other:?}"),
            }
        }

        assert!(frames.len() > 1, "the source produced nothing to compare");
        for pair in frames.windows(2) {
            assert_ne!(pair[0], pair[1], "the same picture was delivered twice");
        }
    }

    #[test]
    fn a_still_screen_produces_one_frame_and_then_nothing() {
        // The state an unattended host is in almost all the time, and the one
        // the sweeping bar cannot model. Everything downstream that only acts
        // when a frame arrives has to be tested against this.
        let source = SyntheticSource::still(SMALL);
        let stream = source
            .start(DisplayId::PRIMARY, &CaptureOptions::default())
            .unwrap();

        let Ok(Recv::Frame(first)) = stream.recv_timeout(Duration::from_secs(5)) else {
            panic!("a still screen still has to produce its first picture");
        };
        assert_eq!(first.damage, Damage::Full);
        assert!(first.is_consistent(), "{first:?}");

        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            match stream.recv_timeout(Duration::from_millis(100)) {
                Ok(Recv::Idle) => continue,
                other => panic!("a still screen produced {other:?}"),
            }
        }
    }

    #[test]
    fn dropping_the_stream_stops_the_generator() {
        let source = SyntheticSource::new(SMALL, 240);
        let stream = source
            .start(DisplayId::PRIMARY, &CaptureOptions::default())
            .unwrap();
        let _ = stream.recv_timeout(Duration::from_secs(5));

        // `stop` joins the generator thread, so returning at all proves it
        // noticed. A generator that ignored the halt flag would hang here.
        stream.stop();
    }

    #[test]
    fn a_frame_rate_cap_is_respected() {
        let source = SyntheticSource::new(SMALL, 240);
        let options = CaptureOptions {
            max_fps: Some(10),
            ..CaptureOptions::default()
        };
        let stream = source.start(DisplayId::PRIMARY, &options).unwrap();

        let mut times = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while times.len() < 3 && Instant::now() < deadline {
            if let Ok(Recv::Frame(frame)) = stream.recv_timeout(Duration::from_millis(500)) {
                times.push(frame.elapsed);
            }
        }

        assert_eq!(times.len(), 3, "did not receive three frames");
        let gap = times[2] - times[1];
        assert!(
            gap >= Duration::from_millis(80),
            "frames arrived {gap:?} apart, faster than the 10 fps cap"
        );
    }
}
