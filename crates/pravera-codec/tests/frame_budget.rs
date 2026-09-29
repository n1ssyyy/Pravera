//! How many 1080p frames a second this machine can actually encode.
//!
//! The user-visible requirement is thirty frames a second at all times. Every
//! stage of the host pipeline runs on one thread — capture, scale, encode,
//! split, send, in that order — so the encoder's per-frame cost is a hard
//! ceiling on the frame rate, whatever the network can carry.
//!
//! These measure that ceiling and nothing else. They are deliberately not
//! `#[ignore]`d: a build where 1080p no longer encodes in time is a build that
//! cannot meet the requirement, and that should show up as a failing test
//! rather than as a complaint about choppy video.
//!
//! The content matters. An encoder handed the same picture repeatedly reports a
//! throughput nothing on a real desktop will ever see, because every frame
//! after the first costs almost nothing. So the pattern below moves, and moves
//! by an amount typical of a desktop rather than of a video: most of the screen
//! is unchanged, a window-sized region is not.

use std::time::{Duration, Instant};

use pravera_codec::{EncoderSettings, RawFrame};
use pravera_core::{PixelFormat, QualityProfile, Resolution};

/// The requirement, in frames per second.
const REQUIRED: f64 = 30.0;

/// Whether the figure is worth holding to the requirement.
///
/// An unoptimised build measures about a tenth of what a shipped one does —
/// the colour conversion and the frame packing are Rust and get no inlining —
/// so asserting in debug would fail every `cargo test` for a reason that has
/// nothing to do with the code. The measurement is still printed there, since
/// a relative change between two debug runs is still a signal.
const ENFORCED: bool = !cfg!(debug_assertions);

fn require(profile: &str, fps: f64) {
    assert!(
        !ENFORCED || fps >= REQUIRED,
        "{profile} manages {fps:.1} fps at {SCREEN}, below the {REQUIRED} fps floor"
    );
}

/// Frames per measurement. Enough to leave the first-frame keyframe behind and
/// let the rate controller settle.
const FRAMES: usize = 90;

/// Frames encoded before the clock starts.
const WARMUP: usize = 10;

const SCREEN: Resolution = Resolution::new(1920, 1080);

/// A desktop-like frame: a still background with one moving region.
///
/// Roughly a tenth of the screen changes per frame, which is what dragging a
/// window or scrolling a document looks like to an encoder. A full-screen
/// change every frame would be a video player, and a static frame would be an
/// encoder benchmark that flatters itself.
fn desktop_frame(resolution: Resolution, step: usize) -> Vec<u8> {
    let (width, height) = (resolution.width as usize, resolution.height as usize);
    let mut pixels = vec![0u8; width * height * 4];

    for y in 0..height {
        for x in 0..width {
            let at = (y * width + x) * 4;
            // Background: broad flat bands, the way a desktop is mostly one
            // colour with a few panels on it.
            let band = ((x / 240) + (y / 135)) as u8;
            let (b, g, r) = (24 + band * 3, 24 + band * 3, 26 + band * 3);
            pixels[at] = b;
            pixels[at + 1] = g;
            pixels[at + 2] = r;
            pixels[at + 3] = 255;
        }
    }

    // The moving part: a block that slides across, carrying fine detail so the
    // encoder has something real to spend bits on.
    let block = width / 4;
    let left = (step * 17) % (width - block);
    let top = height / 3;
    for y in top..(top + height / 4).min(height) {
        for x in left..(left + block) {
            let at = (y * width + x) * 4;
            let detail = ((x ^ y) & 0xff) as u8;
            pixels[at] = detail;
            pixels[at + 1] = detail.wrapping_mul(3);
            pixels[at + 2] = detail.wrapping_add(step as u8);
            pixels[at + 3] = 255;
        }
    }

    pixels
}

/// Encode `FRAMES` moving frames and report the per-frame cost.
fn measure(profile: QualityProfile) -> Duration {
    measure_with(profile, pravera_core::Codec::OpenH264)
}

fn measure_with(profile: QualityProfile, codec: pravera_core::Codec) -> Duration {
    let mut settings = EncoderSettings::new(SCREEN, profile);
    settings.codec = codec;
    let mut encoder = pravera_codec::encoder(&settings).expect("an encoder for this machine");

    let frames: Vec<Vec<u8>> = (0..(WARMUP + FRAMES))
        .map(|step| desktop_frame(SCREEN, step))
        .collect();

    let mut encode = |pixels: &[u8], step: usize| {
        let raw = RawFrame {
            resolution: SCREEN,
            format: PixelFormat::Bgra8,
            stride: SCREEN.width as usize * 4,
            pixels,
            capture_micros: step as u32 * 16_000,
        };
        encoder.encode(raw).expect("encode")
    };

    for (step, pixels) in frames.iter().enumerate().take(WARMUP) {
        encode(pixels, step);
    }

    let started = Instant::now();
    for (step, pixels) in frames.iter().enumerate().skip(WARMUP) {
        encode(pixels, step);
    }
    let elapsed = started.elapsed();

    let per_frame = elapsed / FRAMES as u32;
    eprintln!(
        "{:>8}: {:>6.1} fps  ({:.1} ms per frame, {} at {})",
        profile.name(),
        1.0 / per_frame.as_secs_f64(),
        per_frame.as_secs_f64() * 1000.0,
        settings.codec.name(),
        SCREEN,
    );
    per_frame
}

#[test]
fn the_latency_profile_encodes_1080p_fast_enough_to_be_played_on() {
    let per_frame = measure(QualityProfile::Latency);
    require("Latency", 1.0 / per_frame.as_secs_f64());
}

#[test]
fn the_default_profile_encodes_1080p_fast_enough_to_be_used() {
    // Adaptive is what a session starts on, so this is the number the user
    // actually sees unless they go and change it.
    let per_frame = measure(QualityProfile::Adaptive);
    require("Adaptive", 1.0 / per_frame.as_secs_f64());
}

#[test]
fn the_quality_profile_is_measured_even_if_it_is_allowed_to_be_slower() {
    // Quality is explicitly for reading text and administering, and its own
    // profile caps it at 60 rather than promising a floor. Measured anyway,
    // because the figure is the thing worth knowing.
    measure(QualityProfile::Quality);
}

#[test]
fn the_hardware_encoder_is_the_reason_any_of_this_is_fast_enough() {
    // Skips rather than fails where there is no hardware encoder: a machine
    // without one is a machine this test has nothing to say about, and the
    // software floor is asserted separately above.
    if !pravera_codec::encodable().contains(&pravera_core::Codec::H264) {
        eprintln!("no hardware H.264 encoder on this machine; skipping");
        return;
    }

    for profile in [
        QualityProfile::Latency,
        QualityProfile::Adaptive,
        QualityProfile::Quality,
    ] {
        let per_frame = measure_with(profile, pravera_core::Codec::H264);
        require("hardware", 1.0 / per_frame.as_secs_f64());
    }
}
