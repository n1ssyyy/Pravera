//! What the GPU encodes, the other machine has to be able to decode.
//!
//! This is the assumption the whole hardware path rests on: [`Codec::H264`] and
//! [`Codec::OpenH264`] are the same bitstream produced two ways, so a client
//! with no hardware encoder of its own can still decode a stream from a host
//! that has one. If that is wrong the failure is not an error message, it is a
//! black window — the session negotiates, the packets arrive, the counters go
//! up, and nothing is ever shown.
//!
//! The specific risk is the H.264 profile. openh264 decodes Constrained
//! Baseline and nothing else, while a hardware encoder left to its own devices
//! will happily emit Main or High. So the encoder is pinned to Baseline, and
//! these check that the pin held.

use pravera_codec::{EncoderSettings, RawFrame};
use pravera_core::{Codec, PixelFormat, QualityProfile, Resolution};

const SCREEN: Resolution = Resolution::new(640, 360);

/// Enough frames to get past the encoder's pipeline delay and see steady state.
const FRAMES: usize = 40;

/// Long enough that an encoder which only works while its initial input credits
/// last has visibly stopped.
///
/// An asynchronous transform raises several `METransformNeedInput` events up
/// front to fill its pipeline. A caller that consumes those events without
/// feeding a frame for each one destroys the requests, and the transform never
/// raises them again — so it encodes a burst and then goes quiet forever. Forty
/// frames is not always enough to see that; five seconds of them is.
const SUSTAINED: usize = 300;

fn hardware_available() -> bool {
    pravera_codec::encodable().contains(&Codec::H264)
}

/// A frame with a moving block, so successive frames genuinely differ.
fn frame(step: usize) -> Vec<u8> {
    let (width, height) = (SCREEN.width as usize, SCREEN.height as usize);
    let mut pixels = vec![0u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let at = (y * width + x) * 4;
            // A recognisable field: dark background, one bright moving band.
            let lit = (x + step * 7) % width < width / 5;
            let value = if lit { 220 } else { 30 };
            pixels[at] = value;
            pixels[at + 1] = value;
            pixels[at + 2] = value;
            pixels[at + 3] = 255;
        }
    }
    pixels
}

fn encode_all() -> Vec<pravera_codec::EncodedFrame> {
    encode_n(FRAMES)
}

fn encode_n(frames: usize) -> Vec<pravera_codec::EncodedFrame> {
    encode_tracked(frames)
        .into_iter()
        .map(|(_, frame)| frame)
        .collect()
}

/// Encode `frames` frames, remembering which input each output came back on.
///
/// The input index matters for one question this file has to answer — whether
/// the encoder kept working — and a bare list of outputs cannot answer it. An
/// encoder that produces a burst and then stalls and one that keeps up but
/// drops a share of frames under an unpaced feed look identical by count, and
/// only the first is a defect.
fn encode_tracked(frames: usize) -> Vec<(usize, pravera_codec::EncodedFrame)> {
    let mut settings = EncoderSettings::new(SCREEN, QualityProfile::Adaptive);
    settings.codec = Codec::H264;
    let mut encoder = pravera_codec::encoder(&settings).expect("a hardware encoder");

    let mut out = Vec::new();
    for step in 0..frames {
        let pixels = frame(step);
        let raw = RawFrame {
            resolution: SCREEN,
            format: PixelFormat::Bgra8,
            stride: SCREEN.width as usize * 4,
            pixels: &pixels,
            capture_micros: step as u32 * 16_000,
        };
        if let Some(encoded) = encoder.encode(raw).expect("encode") {
            out.push((step, encoded));
        }
    }
    out
}

#[test]
fn the_software_decoder_can_read_what_the_gpu_wrote() {
    if !hardware_available() {
        eprintln!("no hardware H.264 encoder on this machine; skipping");
        return;
    }

    let encoded = encode_all();
    assert!(
        !encoded.is_empty(),
        "the hardware encoder produced nothing at all"
    );

    let mut decoder = pravera_codec::decoder(Codec::H264).expect("a decoder");
    let mut decoded = 0usize;
    for frame in &encoded {
        match decoder.decode(frame) {
            Ok(Some(picture)) => {
                assert_eq!(picture.resolution, SCREEN);
                decoded += 1;
            }
            // Parameter sets carry no picture. Ordinary at the start.
            Ok(None) => {}
            Err(error) => panic!("openh264 could not decode a hardware frame: {error}"),
        }
    }

    assert!(
        decoded > 0,
        "{} hardware frames decoded to no pictures at all",
        encoded.len()
    );
}

#[test]
fn the_first_thing_the_gpu_sends_is_a_keyframe() {
    // A stream that opens on a predicted frame is one the client stares at
    // until the periodic keyframe comes round, which is seconds later.
    if !hardware_available() {
        eprintln!("no hardware H.264 encoder on this machine; skipping");
        return;
    }

    let encoded = encode_all();
    assert!(
        encoded.first().is_some_and(|frame| frame.keyframe),
        "the first frame out was not a keyframe"
    );
}

#[test]
fn the_picture_that_comes_back_is_the_picture_that_went_in() {
    // Not just "something decoded" — the wrong colour matrix, a swapped chroma
    // pair, or reading the buffer by the wrong stride all decode perfectly and
    // produce the wrong image. The test frame is a bright band on a dark
    // field, so the decoded frame must be bright in some places and dark in
    // others, and grey in neither.
    if !hardware_available() {
        eprintln!("no hardware H.264 encoder on this machine; skipping");
        return;
    }

    let encoded = encode_all();
    let mut decoder = pravera_codec::decoder(Codec::H264).expect("a decoder");

    let mut last = None;
    for frame in &encoded {
        if let Ok(Some(picture)) = decoder.decode(frame) {
            last = Some(picture);
        }
    }
    let picture = last.expect("at least one decoded picture");

    let luma: Vec<u8> = picture
        .pixels
        .chunks_exact(4)
        .map(|px| {
            // The decoder hands back RGBA; any channel will do on a grey field.
            px[0]
        })
        .collect();

    let bright = luma.iter().filter(|&&v| v > 180).count();
    let dark = luma.iter().filter(|&&v| v < 70).count();

    assert!(
        bright > luma.len() / 20,
        "the bright band did not survive the round trip: {bright} of {} pixels",
        luma.len()
    );
    assert!(
        dark > luma.len() / 2,
        "the dark field did not survive the round trip: {dark} of {} pixels",
        luma.len()
    );
}

#[test]
fn the_encoder_keeps_producing_frames_for_as_long_as_it_is_fed() {
    // The failure this exists for: a session that runs for about a second and
    // then freezes on one picture, while the mouse, the keyboard, the control
    // stream and every counter carry on working. Nothing reports an error —
    // the encoder simply stops being asked, because its input requests were
    // read off the event queue and thrown away.
    //
    // Measured as a rate rather than a total, so a slow machine that keeps up
    // passes and a fast one that stalls does not.
    if !hardware_available() {
        eprintln!("no hardware H.264 encoder on this machine; skipping");
        return;
    }

    let encoded = encode_tracked(SUSTAINED);
    assert!(!encoded.is_empty(), "the encoder produced nothing at all");

    // The question is not how many frames came back. This test feeds as fast as
    // the loop runs, which is faster than any encoder, so dropping a share of
    // them is correct behaviour — the host paces its capture and does not do
    // this. The question is whether output was still arriving at the end.
    let last = encoded.last().map(|(step, _)| *step).unwrap_or(0);
    assert!(
        last >= SUSTAINED - SUSTAINED / 10,
        "the last frame out came from input {last} of {SUSTAINED}: the encoder stopped part-way \
         through and everything after it was silence"
    );

    // And no long silence in the middle either, which is the same fault
    // recovering by luck rather than not happening.
    let gap = encoded
        .windows(2)
        .map(|pair| pair[1].0 - pair[0].0)
        .max()
        .unwrap_or(0);
    assert!(
        gap < SUSTAINED / 10,
        "{gap} consecutive frames went in with nothing coming out"
    );
}

#[test]
fn every_frame_the_encoder_produces_is_handed_back() {
    // H.264 frames reference the ones before them, so an encoder that produces
    // a frame and drops it on the floor corrupts the picture until the next
    // keyframe — seconds of smeared video from one lost buffer. The transform
    // can answer one input with more than one output at the start of a stream,
    // and the trait returns one frame per call, so anything extra has to be
    // queued rather than overwritten.
    //
    // Checked by counting: with B-frames off there is at most one output per
    // input, so the count out can never exceed the count in, and a healthy
    // encoder gets very close to it.
    if !hardware_available() {
        eprintln!("no hardware H.264 encoder on this machine; skipping");
        return;
    }

    let encoded = encode_n(SUSTAINED);
    assert!(
        encoded.len() <= SUSTAINED,
        "more frames came out ({}) than went in ({SUSTAINED})",
        encoded.len()
    );

    // Capture times must come back in the order they went in. They are the
    // client's only handle on when a frame was taken, and a queue that
    // desynchronises hands every frame the wrong one.
    let times: Vec<u32> = encoded.iter().map(|frame| frame.capture_micros).collect();
    assert!(
        times.windows(2).all(|pair| pair[0] < pair[1]),
        "capture times came back out of order"
    );
}

#[test]
fn a_full_size_stream_decodes_all_the_way_through_not_just_the_first_picture() {
    // The failure this is for: a session that shows one picture and then holds
    // it, while the mouse, the keyboard and every counter carry on. That is
    // what a broken reference chain looks like from the outside — the opening
    // keyframe decodes, and every predicted frame after it fails.
    //
    // At full size rather than the small frame the other tests use, because
    // resolution is what changes both the encoder's choices and whether the
    // software decoder can keep up.
    if !hardware_available() {
        eprintln!("no hardware H.264 encoder on this machine; skipping");
        return;
    }

    const FULL: Resolution = Resolution::new(1920, 1080);
    const RUN: usize = 120;

    let mut settings = EncoderSettings::new(FULL, QualityProfile::Adaptive);
    settings.codec = Codec::H264;
    let mut encoder = pravera_codec::encoder(&settings).expect("a hardware encoder");
    let mut decoder = pravera_codec::decoder(Codec::H264).expect("a decoder");

    let (width, height) = (FULL.width as usize, FULL.height as usize);
    let mut pixels = vec![0u8; width * height * 4];

    let (mut decoded, mut failed, mut keyframes) = (0usize, 0usize, 0usize);

    for step in 0..RUN {
        // A moving band, redrawn each step so successive frames really differ.
        for y in 0..height {
            let lit_row = (y + step * 3) % height < height / 6;
            for x in 0..width {
                let at = (y * width + x) * 4;
                let value = if lit_row || (x + step * 11) % width < width / 8 {
                    210
                } else {
                    28
                };
                pixels[at] = value;
                pixels[at + 1] = value;
                pixels[at + 2] = value;
                pixels[at + 3] = 255;
            }
        }

        let raw = RawFrame {
            resolution: FULL,
            format: PixelFormat::Bgra8,
            stride: width * 4,
            pixels: &pixels,
            capture_micros: step as u32 * 16_000,
        };

        let Some(encoded) = encoder.encode(raw).expect("encode") else {
            continue;
        };
        if encoded.keyframe {
            keyframes += 1;
        }

        match decoder.decode(&encoded) {
            Ok(Some(picture)) => {
                assert_eq!(picture.resolution, FULL);
                decoded += 1;
            }
            Ok(None) => {}
            Err(_) => failed += 1,
        }
    }

    eprintln!("1080p: {decoded} decoded, {failed} failed, {keyframes} keyframes of {RUN} frames");

    assert_eq!(
        failed, 0,
        "{failed} predicted frames could not be decoded: the picture would freeze on the last one \
         that worked"
    );
    assert!(
        decoded >= RUN * 3 / 4,
        "only {decoded} of {RUN} frames produced a picture"
    );
}

#[test]
fn asking_for_a_keyframe_produces_one_whatever_the_encoder_thinks_of_the_idea() {
    // This is the only way a session recovers. When a client's decoder loses
    // the reference chain — a dropped packet, a frame it could not keep up
    // with — every predicted frame after that point is undecodable, so it asks
    // for a keyframe and shows the last good picture until one arrives. If the
    // request goes unanswered the picture is frozen for good, while input, the
    // control stream and every counter keep working: a fault that looks like
    // anything except a codec problem.
    //
    // Whether a transform honours `AVEncVideoForceKeyFrame` is up to its
    // vendor, so the encoder does not rely on it — an unanswered request is
    // timed out and the transform replaced, because every H.264 encoder ever
    // written begins with a keyframe. This asserts the guarantee, not the
    // mechanism.
    if !hardware_available() {
        eprintln!("no hardware H.264 encoder on this machine; skipping");
        return;
    }

    let mut settings = EncoderSettings::new(SCREEN, QualityProfile::Adaptive);
    settings.codec = Codec::H264;
    let mut encoder = pravera_codec::encoder(&settings).expect("a hardware encoder");

    let feed = |encoder: &mut Box<dyn pravera_codec::VideoEncoder>, step: usize| {
        let pixels = frame(step);
        encoder
            .encode(RawFrame {
                resolution: SCREEN,
                format: PixelFormat::Bgra8,
                stride: SCREEN.width as usize * 4,
                pixels: &pixels,
                capture_micros: step as u32 * 16_000,
            })
            .expect("encode")
    };

    // Settle past the opening keyframe and into predicted frames.
    for step in 0..40 {
        feed(&mut encoder, step);
    }

    encoder.request_keyframe();

    // Long enough to cover an encoder that answers immediately, one that
    // answers after its pipeline drains, and one that has to be replaced.
    let mut arrived = None;
    for step in 40..120 {
        if let Some(encoded) = feed(&mut encoder, step) {
            if encoded.keyframe {
                arrived = Some(step - 40);
                break;
            }
        }
    }

    let after = arrived.expect(
        "no keyframe ever arrived after one was asked for: a client whose decoder lost the \
         reference chain would stay frozen indefinitely",
    );
    eprintln!("keyframe arrived {after} frames after it was asked for");
}

#[test]
fn a_machine_that_can_encode_in_hardware_says_so_before_software() {
    // Order is the preference the handshake uses. Software first would mean
    // the fast path exists and is never chosen.
    let encodable = pravera_codec::encodable();
    assert!(
        encodable.contains(&Codec::OpenH264),
        "the software floor must always be offered"
    );
    if encodable.contains(&Codec::H264) {
        let hardware = encodable.iter().position(|c| *c == Codec::H264);
        let software = encodable.iter().position(|c| *c == Codec::OpenH264);
        assert!(hardware < software, "{encodable:?}");
    }
}

#[test]
fn every_machine_claims_it_can_decode_hardware_h264() {
    // The point of splitting the two lists. A client without a GPU encoder
    // still decodes a hardware stream, and saying otherwise would drag every
    // host down to software encoding to match a limitation the client does not
    // have.
    assert!(pravera_codec::decodable().contains(&Codec::H264));
    assert!(pravera_codec::decodable().contains(&Codec::OpenH264));
}
