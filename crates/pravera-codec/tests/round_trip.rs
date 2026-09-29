//! Encode a known picture, decode it, and look at what came back.
//!
//! Compression is lossy, so nothing here asserts equality. What it asserts is
//! that the picture still *means* the same thing: the red square is still red,
//! the layout has not moved, and the error is small enough that a person would
//! call it the same image. Those are the properties a channel-order mistake, a
//! stride mistake or a plane mix-up all destroy, and they are the mistakes this
//! path is prone to.

use bytes::Bytes;
use pravera_codec::{
    decoder, encoder, CodecError, DecodedFrame, EncodedFrame, EncoderSettings, RawFrame,
    VideoDecoder, VideoEncoder,
};
use pravera_core::{Codec, PixelFormat, QualityProfile, Resolution};

const SIZE: Resolution = Resolution::new(320, 240);

/// Quadrant colours, clockwise from the top left, as RGB.
const QUADRANTS: [[u8; 3]; 4] = [
    [0xe0, 0x20, 0x20], // red
    [0x20, 0xe0, 0x20], // green
    [0x20, 0x20, 0xe0], // blue
    [0xf0, 0xf0, 0xf0], // near white
];

/// A framebuffer to feed the encoder.
struct Picture {
    resolution: Resolution,
    format: PixelFormat,
    stride: usize,
    pixels: Vec<u8>,
}

impl Picture {
    /// Four coloured quadrants, plus a bar whose position depends on `step` so
    /// consecutive frames genuinely differ.
    fn new(resolution: Resolution, format: PixelFormat, stride: usize, step: u32) -> Picture {
        let (width, height) = (resolution.width, resolution.height);
        let stride = stride.max(width as usize * 4);
        let mut pixels = vec![0u8; stride * height as usize];

        for y in 0..height {
            for x in 0..width {
                let quadrant = usize::from(x >= width / 2) + 2 * usize::from(y >= height / 2);
                let bar = x / 8 == step % (width / 8);
                let rgb = if bar {
                    [0x00, 0x00, 0x00]
                } else {
                    QUADRANTS[quadrant]
                };

                let at = y as usize * stride + x as usize * 4;
                pixels[at..at + 4].copy_from_slice(&encode(format, rgb));
            }
        }

        Picture {
            resolution,
            format,
            stride,
            pixels,
        }
    }

    fn frame(&self, capture_micros: u32) -> RawFrame<'_> {
        RawFrame {
            resolution: self.resolution,
            format: self.format,
            stride: self.stride,
            pixels: &self.pixels,
            capture_micros,
        }
    }
}

fn encode(format: PixelFormat, [r, g, b]: [u8; 3]) -> [u8; 4] {
    match format {
        PixelFormat::Bgra8 => [b, g, r, 0xff],
        _ => [r, g, b, 0xff],
    }
}

/// The colour at a point of a decoded RGBA frame.
fn sample(frame: &DecodedFrame, x: u32, y: u32) -> [u8; 3] {
    let at = y as usize * frame.stride + x as usize * 4;
    [frame.pixels[at], frame.pixels[at + 1], frame.pixels[at + 2]]
}

/// How far a decoded colour is from what was encoded, averaged over channels.
fn drift(got: [u8; 3], want: [u8; 3]) -> u32 {
    (0..3).map(|c| got[c].abs_diff(want[c]) as u32).sum::<u32>() / 3
}

/// The centre of one quadrant, kept well away from the edges where 4:2:0
/// chroma subsampling legitimately smears colour across the boundary.
fn quadrant_centre(resolution: Resolution, quadrant: usize) -> (u32, u32) {
    let (w, h) = (resolution.width, resolution.height);
    let x = if quadrant % 2 == 0 { w / 4 } else { w * 3 / 4 };
    let y = if quadrant < 2 { h / 4 } else { h * 3 / 4 };
    (x, y)
}

fn pipeline(profile: QualityProfile) -> (Box<dyn VideoEncoder>, Box<dyn VideoDecoder>) {
    let settings = EncoderSettings::new(SIZE, profile);
    (
        encoder(&settings).expect("no software encoder"),
        decoder(Codec::OpenH264).expect("no software decoder"),
    )
}

/// Push frames through until the decoder produces a picture, or give up.
fn through(
    encoder: &mut dyn VideoEncoder,
    decoder: &mut dyn VideoDecoder,
    pictures: &[Picture],
) -> DecodedFrame {
    for (step, picture) in pictures.iter().enumerate() {
        let Some(encoded) = encoder
            .encode(picture.frame(step as u32 * 16_667))
            .expect("encoding failed")
        else {
            continue;
        };
        if let Some(decoded) = decoder.decode(&encoded).expect("decoding failed") {
            return decoded;
        }
    }
    panic!(
        "the decoder never produced a picture from {} frames",
        pictures.len()
    );
}

#[test]
fn a_picture_survives_the_round_trip_recognisably() {
    let (mut encoder, mut decoder) = pipeline(QualityProfile::Quality);
    let pictures: Vec<Picture> = (0..4)
        .map(|step| Picture::new(SIZE, PixelFormat::Bgra8, 0, step))
        .collect();

    let decoded = through(&mut *encoder, &mut *decoder, &pictures);

    assert_eq!(decoded.resolution, SIZE);
    assert_eq!(decoded.format, PixelFormat::Rgba8);
    assert_eq!(decoded.stride, SIZE.width as usize * 4);
    assert_eq!(decoded.pixels.len(), decoded.stride * SIZE.height as usize);

    for (quadrant, want) in QUADRANTS.iter().enumerate() {
        let (x, y) = quadrant_centre(SIZE, quadrant);
        let got = sample(&decoded, x, y);
        assert!(
            drift(got, *want) < 24,
            "quadrant {quadrant} came back {got:?}, expected about {want:?}"
        );
    }
}

#[test]
fn red_does_not_come_back_blue() {
    // The single most likely bug in this path. BGRA in, RGBA out, and one
    // reversed channel triple anywhere in between produces a picture that
    // looks plausible until you notice every colour is wrong.
    let (mut encoder, mut decoder) = pipeline(QualityProfile::Quality);
    let pictures: Vec<Picture> = (0..4)
        .map(|step| Picture::new(SIZE, PixelFormat::Bgra8, 0, step))
        .collect();

    let decoded = through(&mut *encoder, &mut *decoder, &pictures);

    let (x, y) = quadrant_centre(SIZE, 0);
    let red = sample(&decoded, x, y);
    assert!(
        red[0] > red[1] + 60 && red[0] > red[2] + 60,
        "the red quadrant decoded as {red:?}"
    );

    let (x, y) = quadrant_centre(SIZE, 2);
    let blue = sample(&decoded, x, y);
    assert!(
        blue[2] > blue[0] + 60 && blue[2] > blue[1] + 60,
        "the blue quadrant decoded as {blue:?}"
    );
}

#[test]
fn both_input_channel_orders_produce_the_same_picture() {
    // If either branch of the conversion is wrong, the two disagree.
    let mut results = Vec::new();
    for format in [PixelFormat::Bgra8, PixelFormat::Rgba8] {
        let (mut encoder, mut decoder) = pipeline(QualityProfile::Quality);
        let pictures: Vec<Picture> = (0..4)
            .map(|step| Picture::new(SIZE, format, 0, step))
            .collect();
        let decoded = through(&mut *encoder, &mut *decoder, &pictures);
        results.push(
            (0..4)
                .map(|q| {
                    let (x, y) = quadrant_centre(SIZE, q);
                    sample(&decoded, x, y)
                })
                .collect::<Vec<_>>(),
        );
    }

    for (quadrant, (bgra, rgba)) in results[0].iter().zip(&results[1]).enumerate() {
        assert!(
            drift(*bgra, *rgba) < 8,
            "quadrant {quadrant}: BGRA gave {bgra:?}, RGBA gave {rgba:?}"
        );
    }
}

#[test]
fn a_padded_input_buffer_does_not_shear_the_picture() {
    // A GPU staging texture is padded to the driver's alignment. Treating that
    // padding as pixels shifts every row a little further right than the last,
    // which reads as a diagonal tear.
    let (mut encoder, mut decoder) = pipeline(QualityProfile::Quality);
    let padded = SIZE.width as usize * 4 + 64;
    let pictures: Vec<Picture> = (0..4)
        .map(|step| Picture::new(SIZE, PixelFormat::Bgra8, padded, step))
        .collect();

    let decoded = through(&mut *encoder, &mut *decoder, &pictures);

    for (quadrant, want) in QUADRANTS.iter().enumerate() {
        let (x, y) = quadrant_centre(SIZE, quadrant);
        let got = sample(&decoded, x, y);
        assert!(
            drift(got, *want) < 24,
            "quadrant {quadrant} came back {got:?}, expected about {want:?}"
        );
    }
}

#[test]
fn the_first_frame_of_a_stream_is_always_a_keyframe() {
    // A client joining a session has nothing to predict from.
    let (mut encoder, _) = pipeline(QualityProfile::Adaptive);
    let picture = Picture::new(SIZE, PixelFormat::Bgra8, 0, 0);

    let first = encoder
        .encode(picture.frame(0))
        .expect("encoding failed")
        .expect("the first frame produced nothing");

    assert!(first.keyframe, "the stream opened with a delta frame");
    assert_eq!(first.codec, Codec::OpenH264);
    assert_eq!(first.resolution, SIZE);
    assert!(!first.data.is_empty());
}

#[test]
fn a_requested_keyframe_arrives_on_the_next_frame() {
    let (mut encoder, _) = pipeline(QualityProfile::Adaptive);

    let mut kinds = Vec::new();
    for step in 0..6u32 {
        if step == 3 {
            encoder.request_keyframe();
        }
        let picture = Picture::new(SIZE, PixelFormat::Bgra8, 0, step);
        if let Some(frame) = encoder.encode(picture.frame(step * 16_667)).unwrap() {
            kinds.push((step, frame.keyframe));
        }
    }

    assert!(kinds[0].1, "the first frame was not a keyframe");
    let after = kinds
        .iter()
        .find(|(step, _)| *step == 3)
        .expect("the frame after the request was dropped");
    assert!(
        after.1,
        "asking for a keyframe did not produce one: {kinds:?}"
    );
}

#[test]
fn asking_twice_before_a_frame_still_produces_one_keyframe() {
    // The client asks on every lost chunk, which can be several in a row. Each
    // request must not cost a separate keyframe, or a burst of loss turns into
    // a burst of the most expensive frames there are.
    let (mut encoder, _) = pipeline(QualityProfile::Adaptive);
    let mut keyframes = 0;

    for step in 0..8u32 {
        if step == 2 {
            encoder.request_keyframe();
            encoder.request_keyframe();
            encoder.request_keyframe();
        }
        let picture = Picture::new(SIZE, PixelFormat::Bgra8, 0, step);
        if let Some(frame) = encoder.encode(picture.frame(step * 16_667)).unwrap() {
            if step > 0 && frame.keyframe {
                keyframes += 1;
            }
        }
    }

    assert_eq!(
        keyframes, 1,
        "three requests produced {keyframes} keyframes"
    );
}

#[test]
fn capture_time_is_carried_through_untouched() {
    // The client paces playback against the host's clock. An encoder that
    // renumbered frames would make that meaningless.
    let (mut encoder, mut decoder) = pipeline(QualityProfile::Adaptive);
    let picture = Picture::new(SIZE, PixelFormat::Bgra8, 0, 0);

    let encoded = encoder
        .encode(picture.frame(1_234_567))
        .unwrap()
        .expect("the first frame produced nothing");
    assert_eq!(encoded.capture_micros, 1_234_567);

    if let Some(decoded) = decoder.decode(&encoded).unwrap() {
        assert_eq!(decoded.capture_micros, 1_234_567);
    }
}

#[test]
fn a_decoder_given_nothing_it_can_use_says_so_rather_than_inventing_a_picture() {
    let mut decoder = decoder(Codec::OpenH264).unwrap();
    let rubbish = EncodedFrame {
        codec: Codec::OpenH264,
        resolution: SIZE,
        keyframe: false,
        capture_micros: 0,
        data: Bytes::from_static(&[0x00, 0x00, 0x01, 0x41, 0x9a, 0x02, 0x04]),
    };

    // Either outcome is honest: no picture, or a decode error. Producing a
    // frame of garbage would not be.
    match decoder.decode(&rubbish) {
        Ok(None) | Err(CodecError::Decode(_)) => {}
        Ok(Some(frame)) => panic!("invented a {} picture from noise", frame.resolution),
        Err(other) => panic!("unexpected error: {other}"),
    }
}

#[test]
fn a_decoder_refuses_a_frame_from_a_codec_it_does_not_speak() {
    let mut decoder = decoder(Codec::OpenH264).unwrap();
    let wrong = EncodedFrame {
        codec: Codec::Av1,
        resolution: SIZE,
        keyframe: true,
        capture_micros: 0,
        data: Bytes::from_static(&[0u8; 16]),
    };

    assert!(matches!(
        decoder.decode(&wrong),
        Err(CodecError::Unsupported(Codec::Av1))
    ));
}

#[test]
fn a_display_that_changes_size_mid_stream_is_refused_not_scaled() {
    // Silently rescaling would leave the client rendering at the size it was
    // told about, which is no longer the size it is being sent.
    let (mut encoder, _) = pipeline(QualityProfile::Adaptive);
    let first = Picture::new(SIZE, PixelFormat::Bgra8, 0, 0);
    encoder.encode(first.frame(0)).unwrap();

    let resized = Picture::new(Resolution::new(640, 480), PixelFormat::Bgra8, 0, 1);
    assert!(matches!(
        encoder.encode(resized.frame(16_667)),
        Err(CodecError::BadDimensions { .. })
    ));
}

#[test]
fn every_profile_produces_a_decodable_picture() {
    // The three profiles set genuinely different encoder parameters, and a
    // parameter combination openh264 rejects would only show up here.
    for profile in QualityProfile::ALL {
        let (mut encoder, mut decoder) = pipeline(profile);
        let pictures: Vec<Picture> = (0..6)
            .map(|step| Picture::new(SIZE, PixelFormat::Bgra8, 0, step))
            .collect();

        let decoded = through(&mut *encoder, &mut *decoder, &pictures);
        assert_eq!(
            decoded.resolution,
            SIZE,
            "{} produced the wrong size",
            profile.name()
        );

        let (x, y) = quadrant_centre(SIZE, 0);
        let red = sample(&decoded, x, y);
        assert!(
            red[0] > red[1] && red[0] > red[2],
            "{} decoded red as {red:?}",
            profile.name()
        );
    }
}

#[test]
fn a_still_screen_costs_less_to_send_than_a_moving_one() {
    // The reason a remote desktop is usable on a modest link at all: almost
    // every frame of ordinary use is nearly identical to the one before it.
    // If this stops holding, something is forcing keyframes.
    //
    // Measured under Quality, where rate control spends what the picture needs.
    // Under a bitrate-targeted profile openh264 will happily use the whole
    // budget on a still image — raising its quality rather than sending less —
    // so the same comparison there says nothing about the content.
    //
    // The margin is deliberately loose. How much less a still screen costs
    // depends entirely on what is on it, and pinning a ratio to this
    // particular test pattern would be measuring the pattern, not the codec.
    let still = bytes_over(QualityProfile::Quality, 8, |_| 0);
    let moving = bytes_over(QualityProfile::Quality, 8, |step| step);

    assert!(
        still * 2 < moving,
        "a still screen cost {still} bytes and a moving one {moving}"
    );
}

/// Total encoded bytes over `count` frames, with the bar placed by `position`.
fn bytes_over(profile: QualityProfile, count: u32, position: impl Fn(u32) -> u32) -> usize {
    let (mut encoder, _) = pipeline(profile);
    let mut total = 0;

    for step in 0..count {
        let picture = Picture::new(SIZE, PixelFormat::Bgra8, 0, position(step));
        if let Some(frame) = encoder.encode(picture.frame(step * 16_667)).unwrap() {
            // Skip the opening keyframe: it is the same cost either way and
            // would drown out the difference being measured.
            if step > 0 {
                total += frame.data.len();
            }
        }
    }
    total
}
