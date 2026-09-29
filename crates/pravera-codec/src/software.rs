//! Software H.264, via openh264.
//!
//! The floor every session can fall back to. It needs no GPU, no driver and
//! nothing installed — the C sources are compiled into the binary — which is
//! exactly what makes it the codec both peers can always agree on.
//!
//! It is not fast. A 1080p frame costs single-digit milliseconds of CPU on a
//! modern desktop, which is fine for reading a document and not fine for a
//! game. That is what the hardware encoders in P2 are for; this is the path
//! that guarantees a picture while they are being negotiated, and the one that
//! still works on the machine where they are not available.

use bytes::Bytes;
use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, QpRange,
    RateControlMode, UsageType,
};
use openh264::formats::{BgraSliceU8, RgbaSliceU8, YUVBuffer, YUVSource};
use openh264::{OpenH264API, Timestamp};
use pravera_core::{Codec, PixelFormat, QualityProfile, Resolution};
use tracing::trace;

use crate::{
    CodecError, DecodedFrame, EncodedFrame, EncoderSettings, RawFrame, Result, VideoDecoder,
    VideoEncoder,
};

/// openh264 refuses anything past level 5.2, which is 3840×2160 either way up.
const MAX_LONG_EDGE: u32 = 3840;
const MAX_SHORT_EDGE: u32 = 2160;

pub(crate) struct SoftwareEncoder {
    encoder: Encoder,
    resolution: Resolution,
    /// Reused I420 planes. Sized once; the conversion writes into it in place.
    yuv: YUVBuffer,
    /// Reused tightly-packed copy of the input, used only when the source is
    /// padded or has to be cropped to an even size.
    scratch: Vec<u8>,
    /// Frames handed to the encoder so far, which is where its timestamps come
    /// from. A counter rather than the capture clock: the wire's capture time
    /// wraps every seventy-odd minutes, and an encoder whose timestamps step
    /// backwards makes its own rate control nonsense.
    counter: u64,
    /// Milliseconds per frame at the configured rate.
    tick_millis: u64,
    keyframe_pending: bool,
}

impl SoftwareEncoder {
    pub(crate) fn new(settings: &EncoderSettings) -> Result<SoftwareEncoder> {
        let resolution = settings.encode_resolution();
        check_dimensions(resolution)?;

        let config = configure(settings);
        let encoder =
            Encoder::with_api_config(OpenH264API::from_source(), config).map_err(|error| {
                CodecError::Init {
                    codec: "openh264",
                    reason: error.to_string(),
                }
            })?;

        Ok(SoftwareEncoder {
            encoder,
            resolution,
            yuv: YUVBuffer::new(resolution.width as usize, resolution.height as usize),
            scratch: Vec::new(),
            counter: 0,
            tick_millis: (1000 / settings.fps.max(1)).max(1) as u64,
            // Nothing decodes without one, so the first frame is always a
            // keyframe whether anyone asked or not.
            keyframe_pending: true,
        })
    }
}

impl VideoEncoder for SoftwareEncoder {
    fn codec(&self) -> Codec {
        Codec::OpenH264
    }

    fn resolution(&self) -> Resolution {
        self.resolution
    }

    fn encode(&mut self, frame: RawFrame<'_>) -> Result<Option<EncodedFrame>> {
        frame.check()?;

        // The encoder's reference frames only make sense against a constant
        // size. A display that changed resolution is a control-stream event —
        // the client has to be told the new geometry — not something to paper
        // over by silently scaling.
        let cropped = Resolution::new(frame.resolution.width & !1, frame.resolution.height & !1);
        if cropped != self.resolution {
            return Err(CodecError::BadDimensions {
                resolution: frame.resolution,
                reason: "the display changed size; restart the stream",
            });
        }

        let target = self.resolution;
        let (width, height) = (target.width as usize, target.height as usize);
        let packed = pack(&mut self.scratch, &frame, target);

        match frame.format {
            PixelFormat::Bgra8 => self
                .yuv
                .read_bgra8(BgraSliceU8::new(packed, (width, height))),
            PixelFormat::Rgba8 => self
                .yuv
                .read_rgba8(RgbaSliceU8::new(packed, (width, height))),
            other => return Err(CodecError::UnsupportedInput(other)),
        }

        if self.keyframe_pending {
            self.encoder.force_intra_frame();
            self.keyframe_pending = false;
        }

        let timestamp = Timestamp::from_millis(self.counter * self.tick_millis);
        self.counter += 1;

        let bitstream = self
            .encoder
            .encode_at(&self.yuv, timestamp)
            .map_err(|error| CodecError::Encode(error.to_string()))?;

        let keyframe = match bitstream.frame_type() {
            FrameType::IDR | FrameType::I => true,
            FrameType::P | FrameType::IPMixed => false,
            // Rate control dropped this frame. Ordinary, and not something to
            // send: an empty payload downstream would be indistinguishable
            // from a frame whose chunks all went missing.
            FrameType::Skip => {
                trace!("rate control skipped a frame");
                return Ok(None);
            }
            FrameType::Invalid => {
                return Err(CodecError::Encode(
                    "encoder produced an invalid frame".into(),
                ))
            }
        };

        let data = bitstream.to_vec();
        if data.is_empty() {
            return Ok(None);
        }

        Ok(Some(EncodedFrame {
            codec: Codec::OpenH264,
            resolution: target,
            keyframe,
            capture_micros: frame.capture_micros,
            data: Bytes::from(data),
        }))
    }

    fn request_keyframe(&mut self) {
        // Latched rather than acted on now: `force_intra_frame` applies to the
        // next frame the encoder is given, and two requests before that frame
        // should still produce one keyframe.
        self.keyframe_pending = true;
    }
}

pub(crate) struct SoftwareDecoder {
    decoder: Decoder,
    /// Reused RGBA output. `write_rgba8` demands an exactly sized buffer, so
    /// this is resized rather than merely reserved.
    rgba: Vec<u8>,
}

impl SoftwareDecoder {
    pub(crate) fn new() -> Result<SoftwareDecoder> {
        let decoder = Decoder::new().map_err(|error| CodecError::Init {
            codec: "openh264",
            reason: error.to_string(),
        })?;
        Ok(SoftwareDecoder {
            decoder,
            rgba: Vec::new(),
        })
    }
}

impl VideoDecoder for SoftwareDecoder {
    fn codec(&self) -> Codec {
        Codec::OpenH264
    }

    fn decode(&mut self, frame: &EncodedFrame) -> Result<Option<DecodedFrame>> {
        // Both H.264 labels are accepted. They name which encoder produced the
        // stream, not what the stream is, and openh264 reads Constrained
        // Baseline whoever wrote it — which is exactly why the hardware encoder
        // is pinned to Baseline. Refusing `Codec::H264` here is refusing to
        // decode a stream this decoder can read perfectly well, and it shows up
        // as a black window rather than as an error anyone sees.
        if !matches!(frame.codec, Codec::OpenH264 | Codec::H264) {
            return Err(CodecError::Unsupported(frame.codec));
        }

        let picture = self
            .decoder
            .decode(&frame.data)
            .map_err(|error| CodecError::Decode(error.to_string()))?;

        // No picture yet. The opening packets of an H.264 stream are parameter
        // sets carrying no image at all, and a decoder that has not seen a
        // keyframe has nothing to show.
        let Some(picture) = picture else {
            return Ok(None);
        };

        let (width, height) = picture.dimensions();
        if width == 0 || height == 0 {
            return Ok(None);
        }

        self.rgba.resize(width * height * 4, 0);
        picture.write_rgba8(&mut self.rgba);

        Ok(Some(DecodedFrame {
            resolution: Resolution::new(width as u32, height as u32),
            format: PixelFormat::Rgba8,
            stride: width * 4,
            pixels: Bytes::copy_from_slice(&self.rgba),
            capture_micros: frame.capture_micros,
        }))
    }
}

fn check_dimensions(resolution: Resolution) -> Result<()> {
    if resolution.width == 0 || resolution.height == 0 {
        return Err(CodecError::BadDimensions {
            resolution,
            reason: "a display with no pixels cannot be encoded",
        });
    }

    let long = resolution.width.max(resolution.height);
    let short = resolution.width.min(resolution.height);
    if long > MAX_LONG_EDGE || short > MAX_SHORT_EDGE {
        return Err(CodecError::BadDimensions {
            resolution,
            reason: "openh264 encodes up to 3840x2160; use a hardware encoder for more",
        });
    }
    Ok(())
}

/// A tightly packed, correctly sized view of the frame.
///
/// Returns the caller's own buffer untouched in the common case. The copy only
/// happens when the source is padded — a GPU staging texture whose rows are
/// aligned — or when an odd display has to lose its last row or column.
fn pack<'a>(scratch: &'a mut Vec<u8>, frame: &'a RawFrame<'_>, target: Resolution) -> &'a [u8] {
    let tight = target.width as usize * 4;
    let needed = tight * target.height as usize;

    if frame.stride == tight && frame.resolution == target {
        return &frame.pixels[..needed];
    }

    scratch.resize(needed, 0);
    for row in 0..target.height as usize {
        let from = row * frame.stride;
        scratch[row * tight..(row + 1) * tight].copy_from_slice(&frame.pixels[from..from + tight]);
    }
    scratch
}

/// Turn a quality profile into openh264 settings.
///
/// The three profiles differ in what they are willing to spend. Quality spends
/// CPU and bits to keep text edges sharp; Latency spends picture quality to get
/// the frame out; Adaptive sits between them and is what P2's rate controller
/// will steer.
fn configure(settings: &EncoderSettings) -> EncoderConfig {
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(settings.bitrate))
        .max_frame_rate(FrameRate::from_hz(settings.fps.max(1) as f32))
        .intra_frame_period(IntraFramePeriod::from_num_frames(
            settings.keyframe_frames(),
        ))
        // Screen content, not camera video. The difference is real: the
        // encoder stops assuming sensor noise and gradients and starts
        // assuming large flat areas with hard synthetic edges, which is what a
        // desktop is made of.
        //
        // Always the real-time variant, including under the Quality profile.
        // openh264 validates this field and rejects both non-real-time usage
        // types outright — `ParamValidationExt(), Invalid usage type = 3` —
        // so asking for the offline mode does not merely tune the encoder
        // differently, it stops it initialising at all.
        .usage_type(UsageType::ScreenContentRealTime)
        // Threading left on auto. openh264 slices the frame across cores,
        // which lowers per-frame latency rather than merely raising
        // throughput — the opposite of what threading usually buys.
        .num_threads(0)
        // Denoising is for camera sensors. A desktop has no sensor noise, so
        // this would only blur text.
        .scene_change_detect(true);

    match settings.profile {
        QualityProfile::Quality => config
            // Bitrate is a budget, not a ceiling: hold the quality and spend
            // what it takes. Sharp text is the entire point of this profile.
            .rate_control_mode(RateControlMode::Quality)
            .skip_frames(false)
            .complexity(Complexity::High)
            .adaptive_quantization(true)
            .background_detection(true)
            .qp(QpRange::new(10, 40)),

        QualityProfile::Adaptive => config
            .rate_control_mode(RateControlMode::Bitrate)
            .skip_frames(true)
            .complexity(Complexity::Medium)
            .adaptive_quantization(true)
            .background_detection(true)
            .qp(QpRange::new(14, 45)),

        QualityProfile::Latency => config
            .rate_control_mode(RateControlMode::Bitrate)
            .skip_frames(true)
            .complexity(Complexity::Low)
            // Both of these buy compression by looking harder at the previous
            // frame, and both cost time on every frame. In a game the screen
            // is never still, so they pay for almost nothing.
            .adaptive_quantization(false)
            .background_detection(false)
            .qp(QpRange::new(18, 48)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(resolution: Resolution) -> EncoderSettings {
        EncoderSettings::new(resolution, QualityProfile::Adaptive)
    }

    #[test]
    fn a_screen_larger_than_openh264_can_encode_is_refused_at_startup() {
        // Better a clear error before the session starts than a failure on the
        // first frame, when the client is already waiting for a picture.
        let outcome = SoftwareEncoder::new(&settings(Resolution::new(7680, 4320)));
        assert!(matches!(outcome, Err(CodecError::BadDimensions { .. })));
    }

    #[test]
    fn a_four_k_screen_is_within_reach() {
        assert!(SoftwareEncoder::new(&settings(Resolution::new(3840, 2160))).is_ok());
    }

    #[test]
    fn a_zero_sized_display_is_refused() {
        assert!(matches!(
            check_dimensions(Resolution::new(0, 1080)),
            Err(CodecError::BadDimensions { .. })
        ));
    }

    #[test]
    fn a_tightly_packed_frame_is_not_copied() {
        let target = Resolution::new(4, 2);
        let pixels: Vec<u8> = (0..32u8).collect();
        let frame = RawFrame {
            resolution: target,
            format: PixelFormat::Bgra8,
            stride: 16,
            pixels: &pixels,
            capture_micros: 0,
        };

        let mut scratch = Vec::new();
        let packed = pack(&mut scratch, &frame, target);

        assert_eq!(packed, &pixels[..]);
        assert!(scratch.is_empty(), "an unnecessary copy was made");
    }

    #[test]
    fn a_padded_frame_has_its_padding_removed() {
        // Two rows of four pixels, stored with room for six. The padding bytes
        // must not survive into the encoder, or the picture shears diagonally.
        let target = Resolution::new(4, 2);
        let mut pixels = vec![0xffu8; 24 * 2];
        pixels[..16].copy_from_slice(&(0..16u8).collect::<Vec<_>>());
        pixels[24..40].copy_from_slice(&(16..32u8).collect::<Vec<_>>());

        let frame = RawFrame {
            resolution: target,
            format: PixelFormat::Bgra8,
            stride: 24,
            pixels: &pixels,
            capture_micros: 0,
        };

        let mut scratch = Vec::new();
        let packed = pack(&mut scratch, &frame, target);

        assert_eq!(packed.len(), 32);
        assert_eq!(packed, (0..32u8).collect::<Vec<_>>().as_slice());
    }

    #[test]
    fn an_odd_display_is_cropped_row_by_row_not_truncated() {
        // Truncating the buffer would keep the right number of bytes and the
        // wrong pixels: every row after the first would start one pixel late,
        // which looks like the picture sheared.
        let source = Resolution::new(3, 3);
        let target = Resolution::new(2, 2);
        let pixels: Vec<u8> = (0..36u8).collect();

        let frame = RawFrame {
            resolution: source,
            format: PixelFormat::Bgra8,
            stride: 12,
            pixels: &pixels,
            capture_micros: 0,
        };

        let mut scratch = Vec::new();
        let packed = pack(&mut scratch, &frame, target);

        assert_eq!(packed.len(), 16);
        assert_eq!(&packed[..8], &pixels[0..8], "first row");
        assert_eq!(&packed[8..], &pixels[12..20], "second row starts at row 1");
    }

    /// Where the per-frame time actually goes, split between the colour
    /// conversion and the encode itself.
    ///
    /// The two are fixed by completely different things — conversion is a
    /// scalar loop over every pixel in this process, encoding is openh264's
    /// own thread pool — so knowing which dominates is what decides whether
    /// the next hour is spent on one or the other. Reported rather than
    /// asserted on: the split is a property of the machine, and the figure
    /// that *is* a requirement is asserted in `tests/frame_budget.rs`.
    #[test]
    fn where_the_time_goes_converting_versus_encoding() {
        use std::time::Instant;

        const ROUNDS: u32 = 40;
        let resolution = Resolution::new(1920, 1080);
        let settings = EncoderSettings::new(resolution, QualityProfile::Adaptive);
        let mut encoder = SoftwareEncoder::new(&settings).expect("an encoder");

        // Fresh noise per frame, so neither stage gets to reuse anything.
        let mut pixels = vec![0u8; 1920 * 1080 * 4];
        let mut convert = std::time::Duration::ZERO;
        let mut encode = std::time::Duration::ZERO;

        for round in 0..ROUNDS {
            for (at, byte) in pixels.iter_mut().enumerate() {
                *byte = (at as u32).wrapping_mul(2_654_435_761).wrapping_add(round) as u8;
            }

            let frame = RawFrame {
                resolution,
                format: PixelFormat::Bgra8,
                stride: 1920 * 4,
                pixels: &pixels,
                capture_micros: 0,
            };

            let started = Instant::now();
            let packed = pack(&mut encoder.scratch, &frame, resolution);
            encoder
                .yuv
                .read_bgra8(BgraSliceU8::new(packed, (1920, 1080)));
            convert += started.elapsed();

            let started = Instant::now();
            let timestamp = Timestamp::from_millis(round as u64 * 16);
            encoder
                .encoder
                .encode_at(&encoder.yuv, timestamp)
                .expect("encode");
            encode += started.elapsed();
        }

        let (convert, encode) = (convert / ROUNDS, encode / ROUNDS);
        eprintln!(
            "1080p per frame: convert {:.1} ms, encode {:.1} ms, total {:.1} ms ({:.1} fps)",
            convert.as_secs_f64() * 1000.0,
            encode.as_secs_f64() * 1000.0,
            (convert + encode).as_secs_f64() * 1000.0,
            1.0 / (convert + encode).as_secs_f64(),
        );
    }
}
