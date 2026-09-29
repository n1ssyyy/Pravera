//! Capture from the real screen of whatever machine is running the tests.
//!
//! Everything here is about the boundary the unit tests cannot reach: what the
//! platform actually hands back. Frame sizes, strides, timestamps and monitor
//! geometry all come from the OS, and every one of them has a plausible-looking
//! wrong answer that only a real display will produce.
//!
//! On a machine with no capture backend — a headless Linux runner, a Windows
//! session with no console attached — these skip, loudly. A skipped test that
//! looks like a passing test is worse than no test, so each one says on stderr
//! exactly what it did not check.

use std::time::{Duration, Instant};

use pravera_capture::{CaptureError, CaptureOptions, CaptureSource, DisplayId, Recv};
use pravera_core::PixelFormat;

/// Long enough for a desktop to change on its own, short enough that a
/// genuinely frozen backend fails the run rather than hanging it.
const PATIENCE: Duration = Duration::from_secs(10);

/// The capture backend, or `None` when this machine has none.
fn backend() -> Option<Box<dyn CaptureSource>> {
    match pravera_capture::source() {
        Ok(source) => Some(source),
        Err(error @ (CaptureError::Unavailable(_) | CaptureError::NoDisplays)) => {
            eprintln!("SKIPPED: no capture backend on this machine ({error})");
            None
        }
        Err(error) => panic!("the capture backend failed to start: {error}"),
    }
}

/// Wait for one frame, tolerating a desktop that is not changing.
fn next_frame(stream: &pravera_capture::FrameStream) -> Option<pravera_capture::CapturedFrame> {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        match stream.recv_timeout(Duration::from_millis(250)) {
            Ok(Recv::Frame(frame)) => return Some(frame),
            Ok(Recv::Idle) => continue,
            Ok(Recv::Ended) => panic!("the capture ended before producing a frame"),
            Err(error) => panic!("the capture failed: {error}"),
        }
    }
    None
}

#[test]
fn this_machine_reports_displays_the_protocol_can_describe() {
    let Some(source) = backend() else { return };
    let displays = source.displays().expect("enumerating displays failed");

    assert!(
        !displays.is_empty(),
        "a backend exists but reported no displays"
    );
    eprintln!("{} backend, {} display(s)", source.name(), displays.len());

    for (index, display) in displays.iter().enumerate() {
        eprintln!(
            "  {} {:?} {} at {:?} scale {} @{}Hz{}",
            display.id,
            display.name,
            display.resolution,
            display.position,
            display.scale,
            display.refresh_hz,
            if display.primary { " (primary)" } else { "" }
        );

        assert_eq!(
            display.id,
            DisplayId(index as u8),
            "display ids must be dense and in order, or the wire ids address the wrong screen"
        );
        assert!(
            display.resolution.width > 0 && display.resolution.height > 0,
            "a zero-sized display would make every downstream buffer calculation zero"
        );
        assert!(
            display.scale > 0.0,
            "a zero or negative scale would divide by zero on the client"
        );
        assert!(!display.name.is_empty(), "a display with no label to show");
    }

    // The protocol lets a viewer without MULTI_MONITOR ask for display 0
    // without first listing the monitors. That is only safe if 0 is genuinely
    // the primary one.
    assert!(displays[0].primary, "display 0 is not the primary display");
    assert_eq!(
        displays.iter().filter(|display| display.primary).count(),
        1,
        "exactly one display must be primary"
    );
}

#[test]
fn a_captured_frame_describes_itself_honestly() {
    let Some(source) = backend() else { return };
    let primary = source.primary().expect("no primary display");

    let stream = source
        .start(DisplayId::PRIMARY, &CaptureOptions::default())
        .expect("starting the capture failed");

    let Some(frame) = next_frame(&stream) else {
        eprintln!("SKIPPED: the desktop produced no frames in {PATIENCE:?}");
        return;
    };

    eprintln!(
        "frame: {} {:?} stride {} ({} bytes) at {:?}, damage {:?}",
        frame.resolution,
        frame.format,
        frame.stride,
        frame.pixels.len(),
        frame.elapsed,
        match &frame.damage {
            pravera_capture::Damage::Full => "full".to_string(),
            pravera_capture::Damage::Regions(rects) => format!("{} regions", rects.len()),
        }
    );

    // The stride check is the one that matters. A padded staging texture that
    // reaches the encoder unannounced produces a picture sheared diagonally —
    // instantly obvious to a person, invisible to every unit test.
    assert!(
        frame.is_consistent(),
        "buffer is {} bytes but stride {} x height {} says {}",
        frame.pixels.len(),
        frame.stride,
        frame.resolution.height,
        frame.stride * frame.resolution.height as usize
    );
    assert_eq!(frame.stride, frame.resolution.width as usize * 4);
    assert_eq!(frame.format, PixelFormat::Bgra8);
    assert_eq!(frame.display, DisplayId::PRIMARY);
    assert_eq!(frame.resolution, primary.resolution);
    assert_eq!(stream.format(), frame.format);
}

#[test]
fn time_advances_across_frames_and_starts_near_zero() {
    let Some(source) = backend() else { return };
    let stream = source
        .start(DisplayId::PRIMARY, &CaptureOptions::default())
        .expect("starting the capture failed");

    let Some(first) = next_frame(&stream) else {
        eprintln!("SKIPPED: the desktop produced no frames in {PATIENCE:?}");
        return;
    };
    let Some(second) = next_frame(&stream) else {
        eprintln!("SKIPPED: the desktop produced only one frame in {PATIENCE:?}");
        return;
    };

    // Timestamps come from the compositor's clock, which counts from an
    // arbitrary origin. Rebasing on the first frame is what makes them usable
    // for pacing; without it these are hours-large numbers that overflow the
    // wire field on the very first frame.
    assert!(
        first.elapsed < Duration::from_secs(1),
        "the first frame claims to be {:?} old, so timestamps are not rebased",
        first.elapsed
    );
    assert!(
        second.elapsed > first.elapsed,
        "time did not advance: {:?} then {:?}",
        first.elapsed,
        second.elapsed
    );
    assert!(
        second.elapsed - first.elapsed < PATIENCE,
        "an implausible gap between frames: {:?}",
        second.elapsed - first.elapsed
    );
}

#[test]
fn the_backend_refuses_a_display_that_is_not_there() {
    let Some(source) = backend() else { return };
    let count = source.displays().unwrap().len();

    let absent = DisplayId((count + 8) as u8);
    match source.start(absent, &CaptureOptions::default()) {
        Err(CaptureError::NoSuchDisplay(id)) => assert_eq!(id, absent),
        Err(other) => panic!("wrong error for a missing display: {other}"),
        Ok(_) => panic!("captured a display that does not exist"),
    }
}

#[test]
fn capture_stops_when_the_stream_is_dropped() {
    let Some(source) = backend() else { return };
    let stream = source
        .start(DisplayId::PRIMARY, &CaptureOptions::default())
        .expect("starting the capture failed");
    let _ = next_frame(&stream);

    // `stop` joins the backend's capture thread, so returning at all is the
    // assertion: a backend that ignored the request would hang the test run.
    let started = Instant::now();
    stream.stop();
    eprintln!("capture stopped in {:?}", started.elapsed());
}

#[test]
fn two_displays_can_be_captured_at_once() {
    let Some(source) = backend() else { return };
    let displays = source.displays().unwrap();
    if displays.len() < 2 {
        eprintln!("SKIPPED: this machine has one display, so nothing to overlap");
        return;
    }

    // Multi-monitor sessions run one stream per screen. If starting the second
    // stole the first's frames, or the two shared state, this is where it
    // shows.
    let first = source
        .start(displays[0].id, &CaptureOptions::default())
        .expect("starting the first capture failed");
    let second = source
        .start(displays[1].id, &CaptureOptions::default())
        .expect("starting the second capture failed");

    if let Some(frame) = next_frame(&first) {
        assert_eq!(frame.display, displays[0].id);
        assert_eq!(frame.resolution, displays[0].resolution);
    }
    if let Some(frame) = next_frame(&second) {
        assert_eq!(frame.display, displays[1].id);
        assert_eq!(frame.resolution, displays[1].resolution);
    }
}
