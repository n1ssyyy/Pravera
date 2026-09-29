//! Turning framebuffers into bitstreams and back.
//!
//! Two traits, [`VideoEncoder`] and [`VideoDecoder`], and one implementation
//! behind each of them today: software H.264 via openh264. The hardware
//! encoders — Media Foundation and NVENC on Windows, VA-API and NVENC on Linux
//! — arrive in P2 and slot in behind the same traits, chosen by a runtime
//! probe rather than a compile-time feature.
//!
//! ## Why software first
//!
//! Every hardware encoder is a different set of capabilities, driver bugs and
//! initialisation dances, and a session that cannot start because the GPU said
//! no is worse than a session that runs a little hot. Software H.264 works on
//! every machine, needs nothing installed, and is the floor the negotiation
//! falls back to — so it is the one path that has to work, and building it
//! first means the pipeline above it is exercised from the very first frame.
//!
//! ## What crosses the boundary
//!
//! [`RawFrame`] borrows; [`EncodedFrame`] and [`DecodedFrame`] own. That
//! asymmetry is deliberate — the encoder reads a buffer that capture already
//! owns and must not copy it again, while its output outlives the call and
//! travels to another thread.
//!
//! Nothing here depends on `pravera-capture`. A decoding client has no reason
//! to link a GPU capture stack, and a `RawFrame` is three lines to build at
//! the seam where both are already in scope.

mod convert;
mod error;
#[cfg(windows)]
mod mediafoundation;
mod software;

pub mod scale;

use std::time::Duration;

use bytes::Bytes;
use pravera_core::{Codec, PixelFormat, QualityProfile, Resolution};

pub use error::{CodecError, Result};
pub use scale::Scaler;

/// Raw pixels on their way into an encoder.
#[derive(Debug, Clone, Copy)]
pub struct RawFrame<'a> {
    pub resolution: Resolution,
    pub format: PixelFormat,
    /// Bytes per row, which may exceed `width * 4` on a padded GPU buffer.
    pub stride: usize,
    pub pixels: &'a [u8],
    /// When the frame was captured, in the wire protocol's units. Carried
    /// through the encoder unchanged so the client can pace playback against
    /// the host's clock rather than against arrival times.
    pub capture_micros: u32,
}

impl RawFrame<'_> {
    /// Whether the buffer is the size the other fields claim.
    pub fn is_consistent(&self) -> bool {
        self.stride >= self.resolution.width as usize * 4
            && self.pixels.len() >= self.stride * self.resolution.height as usize
            && self.resolution.width > 0
            && self.resolution.height > 0
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.is_consistent() {
            return Ok(());
        }
        Err(CodecError::Malformed {
            resolution: self.resolution,
            stride: self.stride,
            expected: self.stride * self.resolution.height as usize,
            actual: self.pixels.len(),
        })
    }
}

/// One compressed frame, ready to be chunked into datagrams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    pub codec: Codec,
    /// The size actually encoded, which may be a pixel or two smaller than the
    /// display — see [`EncoderSettings::encode_resolution`].
    pub resolution: Resolution,
    /// Decodable on its own, without any earlier frame.
    pub keyframe: bool,
    pub capture_micros: u32,
    pub data: Bytes,
}

/// One frame back out of a decoder, ready to upload to a texture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    pub resolution: Resolution,
    /// Always [`PixelFormat::Rgba8`] on the CPU path. P7 replaces this whole
    /// struct with an imported GPU surface for the formats that support it.
    pub format: PixelFormat,
    pub stride: usize,
    pub pixels: Bytes,
    pub capture_micros: u32,
}

/// How to encode.
#[derive(Debug, Clone, PartialEq)]
pub struct EncoderSettings {
    pub codec: Codec,
    pub resolution: Resolution,
    pub profile: QualityProfile,
    /// Target bits per second.
    pub bitrate: u32,
    /// Frame rate the rate controller should budget for. Not a promise to
    /// produce that many; a still screen still produces none.
    pub fps: u32,
    /// How often to emit a keyframe even when nobody asked.
    ///
    /// A backstop, not the main mechanism. Media rides unreliable datagrams,
    /// so a client that loses a chunk asks for a keyframe and gets one
    /// immediately. This bounds how long a client that has lost the *control
    /// stream's* attention — or that joined mid-stream — stares at nothing.
    pub keyframe_interval: Duration,
}

impl EncoderSettings {
    /// Sensible settings for a display and a profile.
    pub fn new(resolution: Resolution, profile: QualityProfile) -> EncoderSettings {
        let fps = default_fps(profile);
        EncoderSettings {
            codec: Codec::OpenH264,
            resolution,
            profile,
            bitrate: default_bitrate(resolution, fps, profile),
            fps,
            keyframe_interval: Duration::from_secs(4),
        }
    }

    /// The size the encoder will actually work in.
    ///
    /// H.264 codes in 16×16 macroblocks and cannot represent an odd dimension
    /// at all, so an odd display loses its last row or column. One pixel is
    /// invisible; refusing to start a session over it would not be.
    pub fn encode_resolution(&self) -> Resolution {
        Resolution::new(self.resolution.width & !1, self.resolution.height & !1)
    }

    /// Keyframe period expressed in frames, which is the unit encoders take.
    pub fn keyframe_frames(&self) -> u32 {
        if self.keyframe_interval.is_zero() || self.fps == 0 {
            return 0;
        }
        (self.keyframe_interval.as_secs_f64() * self.fps as f64).round() as u32
    }
}

/// Frame rate to budget the bitrate against.
const fn default_fps(profile: QualityProfile) -> u32 {
    match profile {
        QualityProfile::Quality => 30,
        QualityProfile::Adaptive => 60,
        QualityProfile::Latency => 60,
    }
}

/// A starting bitrate, in bits per second.
///
/// Bits per pixel per frame, scaled by resolution and frame rate. Desktop
/// content compresses far better than camera video — large flat areas, sharp
/// synthetic edges, most of the screen unchanged between frames — so these are
/// well below what the same numbers would mean for a video call.
///
/// Only a starting point. P2's rate controller moves it from measured loss and
/// round-trip time, which is the only way to get this right on a real link.
fn default_bitrate(resolution: Resolution, fps: u32, profile: QualityProfile) -> u32 {
    /// Slowest link worth trying at all.
    const FLOOR: f64 = 1_500_000.0;
    /// Past this, the encoder is no longer the constraint.
    const CEILING: f64 = 80_000_000.0;

    let bits_per_pixel = match profile {
        QualityProfile::Quality => 0.12,
        QualityProfile::Adaptive => 0.08,
        QualityProfile::Latency => 0.06,
    };

    let raw = resolution.pixels() as f64 * fps.max(1) as f64 * bits_per_pixel;
    raw.clamp(FLOOR, CEILING) as u32
}

/// Compresses frames.
///
/// One encoder per stream, and not shared between threads while in use: every
/// implementation carries reference frames and rate-control state that only
/// make sense applied to one sequence in order.
pub trait VideoEncoder: Send {
    fn codec(&self) -> Codec;

    /// The size this encoder produces, which is [`EncoderSettings::encode_resolution`]
    /// rather than the display's own size.
    fn resolution(&self) -> Resolution;

    /// Compress one frame.
    ///
    /// `Ok(None)` means the encoder chose to emit nothing — a skipped frame
    /// under rate control. That is a normal outcome, not a quiet failure.
    fn encode(&mut self, frame: RawFrame<'_>) -> Result<Option<EncodedFrame>>;

    /// Make the next encoded frame a keyframe.
    ///
    /// Called when a client reports loss it cannot recover from. Cheap to ask
    /// for repeatedly: asking twice before the next frame produces one
    /// keyframe, not two.
    fn request_keyframe(&mut self);

    /// What the encoder has seen of its driver, for the log line written when
    /// it stops producing output. `None` for an encoder with nothing to add.
    fn diagnostics(&self) -> Option<String> {
        None
    }
}

/// Decompresses frames.
pub trait VideoDecoder: Send {
    fn codec(&self) -> Codec;

    /// Decode one frame.
    ///
    /// `Ok(None)` is ordinary: an H.264 stream opens with parameter sets that
    /// carry no picture, and a decoder that has not yet seen a keyframe has
    /// nothing to show.
    fn decode(&mut self, frame: &EncodedFrame) -> Result<Option<DecodedFrame>>;
}

/// Codecs this machine can turn frames into, best first.
///
/// A host's side of the handshake. It must never claim something the machine
/// cannot actually do — a peer that believes it gets a black screen and no
/// explanation — so the hardware entry is the result of asking the operating
/// system for a transform rather than of finding a GPU in the device list. A
/// card can be present with its encoder unexposed, which is the normal state of
/// affairs inside a virtual machine.
///
/// The probe runs once. Enumerating Media Foundation transforms takes a few
/// milliseconds and the answer cannot change while the process is running.
pub fn encodable() -> Vec<Codec> {
    #[cfg(windows)]
    {
        use std::sync::OnceLock;
        static HARDWARE: OnceLock<bool> = OnceLock::new();
        // Announced at info rather than debug, and once. Which encoder a
        // machine got is the first thing worth knowing when a session looks
        // slow, and the difference between the two is roughly six-fold — so
        // "why is this choppy" should be answerable from the log of the machine
        // being viewed without anyone having to turn logging up first.
        let hardware = *HARDWARE.get_or_init(|| {
            let available = mediafoundation::hardware_h264_available();
            if available {
                tracing::info!("this machine encodes H.264 in hardware");
            } else {
                tracing::warn!(
                    "no hardware H.264 encoder on this machine; falling back to software, \
                     which is several times slower and will limit the frame rate"
                );
            }
            available
        });
        if hardware {
            return vec![Codec::H264, Codec::OpenH264];
        }
    }
    vec![Codec::OpenH264]
}

/// Whether this machine has a GPU encoder, for saying so in an interface.
///
/// The same cached probe [`encodable`] uses, asked as a yes-or-no. Worth
/// showing on the machine that will be *hosting*, because that is where the
/// encoding happens and therefore where the frame rate is decided — a fact that
/// is not obvious from either end of a session.
pub fn encodes_in_hardware() -> bool {
    encodable().iter().any(|codec| codec.is_hardware())
}

/// Codecs this machine can turn back into pictures, best first.
///
/// Deliberately not the same list. [`Codec::H264`] and [`Codec::OpenH264`] are
/// the same bitstream described two ways — which encoder produced it — and
/// openh264 decodes both. So every machine can decode hardware H.264 whether or
/// not it can produce it.
///
/// Keeping the two lists separate is what lets a fast host stream to a slow
/// client at full speed. Were this the encode list, a client with no GPU
/// encoder would advertise software only, and the host would drop to software
/// encoding to match a limitation the client does not have.
pub fn decodable() -> Vec<Codec> {
    vec![Codec::H264, Codec::OpenH264]
}

pub fn encoder(settings: &EncoderSettings) -> Result<Box<dyn VideoEncoder>> {
    match settings.codec {
        Codec::OpenH264 => Ok(Box::new(software::SoftwareEncoder::new(settings)?)),
        #[cfg(windows)]
        Codec::H264 => Ok(Box::new(mediafoundation::HardwareEncoder::new(settings)?)),
        other => Err(CodecError::Unsupported(other)),
    }
}

pub fn decoder(codec: Codec) -> Result<Box<dyn VideoDecoder>> {
    match codec {
        // One decoder for both: see [`decodable`]. The hardware encoder is
        // configured to emit Constrained Baseline precisely so that this holds.
        Codec::OpenH264 | Codec::H264 => Ok(Box::new(software::SoftwareDecoder::new()?)),
        other => Err(CodecError::Unsupported(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HD: Resolution = Resolution::new(1920, 1080);

    #[test]
    fn an_odd_display_loses_a_pixel_rather_than_the_session() {
        let settings = EncoderSettings::new(Resolution::new(1919, 1081), QualityProfile::Adaptive);
        assert_eq!(settings.encode_resolution(), Resolution::new(1918, 1080));
    }

    #[test]
    fn an_even_display_is_encoded_exactly() {
        let settings = EncoderSettings::new(HD, QualityProfile::Adaptive);
        assert_eq!(settings.encode_resolution(), HD);
    }

    #[test]
    fn quality_spends_more_on_each_frame_than_latency_does() {
        // Bits per *frame*, not per second. The two profiles happen to land on
        // similar totals — Quality asks for twice the bits at half the frame
        // rate — and comparing the totals would say nothing about either.
        let per_frame = |profile| {
            let settings = EncoderSettings::new(HD, profile);
            settings.bitrate / settings.fps
        };

        let quality = per_frame(QualityProfile::Quality);
        let adaptive = per_frame(QualityProfile::Adaptive);
        let latency = per_frame(QualityProfile::Latency);

        assert!(
            quality > adaptive && adaptive > latency,
            "bits per frame should fall from Quality to Latency: {quality} / {adaptive} / {latency}"
        );
    }

    #[test]
    fn latency_never_budgets_for_fewer_frames_than_its_profile_promises() {
        // The profile promises a 60 fps floor. A rate controller budgeting for
        // 30 would starve every frame above that.
        let settings = EncoderSettings::new(HD, QualityProfile::Latency);
        assert!(settings.fps >= QualityProfile::Latency.fps_range().0);
    }

    #[test]
    fn a_tiny_screen_still_gets_a_usable_bitrate() {
        // The formula alone would ask for a few hundred kbps, which produces a
        // smeared picture no matter how small the screen is.
        let tiny = EncoderSettings::new(Resolution::new(320, 240), QualityProfile::Latency);
        assert!(tiny.bitrate >= 1_500_000, "{}", tiny.bitrate);
    }

    #[test]
    fn an_enormous_screen_does_not_ask_for_the_whole_link() {
        let huge = EncoderSettings::new(Resolution::new(7680, 4320), QualityProfile::Quality);
        assert!(huge.bitrate <= 80_000_000, "{}", huge.bitrate);
    }

    #[test]
    fn the_keyframe_backstop_converts_to_whole_frames() {
        let mut settings = EncoderSettings::new(HD, QualityProfile::Adaptive);
        settings.fps = 60;
        settings.keyframe_interval = Duration::from_secs(4);
        assert_eq!(settings.keyframe_frames(), 240);

        // Zero means "only when asked", not "every frame".
        settings.keyframe_interval = Duration::ZERO;
        assert_eq!(settings.keyframe_frames(), 0);
    }

    #[test]
    fn a_frame_whose_buffer_does_not_match_its_geometry_is_refused() {
        let pixels = vec![0u8; 64 * 4 * 4];
        let good = RawFrame {
            resolution: Resolution::new(64, 4),
            format: PixelFormat::Bgra8,
            stride: 64 * 4,
            pixels: &pixels,
            capture_micros: 0,
        };
        assert!(good.is_consistent());

        let short = RawFrame {
            resolution: Resolution::new(64, 8),
            ..good
        };
        assert!(!short.is_consistent());
        assert!(matches!(short.check(), Err(CodecError::Malformed { .. })));

        let narrow = RawFrame { stride: 4, ..good };
        assert!(!narrow.is_consistent());
    }

    #[test]
    fn only_codecs_this_build_can_actually_encode_are_offered_to_a_client() {
        // The handshake sends this list. Claiming a codec the machine cannot
        // produce negotiates a session that shows nothing at all.
        for codec in encodable() {
            assert!(
                encoder(&EncoderSettings {
                    codec,
                    ..EncoderSettings::new(Resolution::new(320, 240), QualityProfile::Adaptive)
                })
                .is_ok(),
                "{} is offered but has no encoder",
                codec.name()
            );
        }
    }

    #[test]
    fn only_codecs_this_build_can_actually_decode_are_accepted_from_a_host() {
        // The other half of the same promise: a client that accepts a stream
        // it cannot read leaves the user with a black window and no error.
        for codec in decodable() {
            assert!(
                decoder(codec).is_ok(),
                "{} is accepted but has no decoder",
                codec.name()
            );
        }
    }

    #[test]
    fn asking_for_a_codec_this_build_lacks_says_so_plainly() {
        assert!(matches!(
            decoder(Codec::Av1),
            Err(CodecError::Unsupported(Codec::Av1))
        ));

        let settings = EncoderSettings {
            codec: Codec::H265,
            ..EncoderSettings::new(HD, QualityProfile::Adaptive)
        };
        assert!(matches!(
            encoder(&settings),
            Err(CodecError::Unsupported(Codec::H265))
        ));
    }
}
