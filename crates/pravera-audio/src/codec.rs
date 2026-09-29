//! Turning sample frames into packets and back.
//!
//! Both codecs here are fixed-rate and packet-independent: five milliseconds of
//! audio in, a known number of bytes out, and nothing in one packet depending
//! on the packet before it. That second property is what makes them right for
//! a datagram path — a lost packet costs exactly its own five milliseconds and
//! the audio afterwards is unaffected.
//!
//! ## Why a trait for two implementations
//!
//! The same reason `pravera-codec` has one: the interesting codec is not here
//! yet. When Opus can be built without linking a stranger's five-year-old
//! binary, it arrives as one more implementation and nothing upstream of this
//! module changes. The seam is cheap now and expensive later.

use pravera_core::audio::{AudioCodec, AudioFormat, ADPCM_STATE_BYTES};

use crate::Error;

/// Compresses sample frames for the wire.
///
/// Stateful: ADPCM carries its adaptation forward between packets even though
/// it writes that state into each one. Encoding the same audio through two
/// encoders will not produce the same bytes, and does not need to.
pub trait AudioEncoder: Send {
    /// Encode exactly one packet's worth of interleaved samples.
    ///
    /// `out` is cleared first. Fails only if `samples` is not the length the
    /// format calls for — a caller bug, not a runtime condition.
    fn encode(&mut self, samples: &[i16], out: &mut Vec<u8>) -> Result<(), Error>;

    fn codec(&self) -> AudioCodec;
}

/// Expands a packet back into interleaved samples.
pub trait AudioDecoder: Send {
    /// Decode one packet into exactly `format.packet_samples()` samples.
    ///
    /// `out` is cleared first. Every byte here came off the network, so a
    /// packet of the wrong length is an ordinary refusal rather than a panic.
    fn decode(&mut self, packet: &[u8], out: &mut Vec<i16>) -> Result<(), Error>;

    fn codec(&self) -> AudioCodec;
}

/// An encoder for the agreed format.
pub fn encoder(format: AudioFormat) -> Box<dyn AudioEncoder> {
    match format.codec {
        AudioCodec::Pcm16 => Box::new(Pcm16Encoder { format }),
        AudioCodec::Adpcm4 => Box::new(AdpcmEncoder::new(format)),
    }
}

/// A decoder for the agreed format.
pub fn decoder(format: AudioFormat) -> Box<dyn AudioDecoder> {
    match format.codec {
        AudioCodec::Pcm16 => Box::new(Pcm16Decoder { format }),
        AudioCodec::Adpcm4 => Box::new(AdpcmDecoder { format }),
    }
}

// ------------------------------------------------------------------- PCM 16

struct Pcm16Encoder {
    format: AudioFormat,
}

impl AudioEncoder for Pcm16Encoder {
    fn encode(&mut self, samples: &[i16], out: &mut Vec<u8>) -> Result<(), Error> {
        expect_samples(samples.len(), self.format.packet_samples())?;
        out.clear();
        out.reserve(samples.len() * 2);
        for sample in samples {
            out.extend_from_slice(&sample.to_le_bytes());
        }
        Ok(())
    }

    fn codec(&self) -> AudioCodec {
        AudioCodec::Pcm16
    }
}

struct Pcm16Decoder {
    format: AudioFormat,
}

impl AudioDecoder for Pcm16Decoder {
    fn decode(&mut self, packet: &[u8], out: &mut Vec<i16>) -> Result<(), Error> {
        expect_packet(packet.len(), self.format.packet_bytes())?;
        out.clear();
        out.reserve(packet.len() / 2);
        out.extend(
            packet
                .chunks_exact(2)
                .map(|pair| i16::from_le_bytes([pair[0], pair[1]])),
        );
        Ok(())
    }

    fn codec(&self) -> AudioCodec {
        AudioCodec::Pcm16
    }
}

// --------------------------------------------------------------- IMA ADPCM

/// The quantiser step for each index. From the IMA/DVI specification; the
/// ratio between neighbours is about 1.1, so the 89 entries span roughly 20
/// milliseconds of dynamic range from 7 up to 32767.
const STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

/// How the step index moves after each nibble.
///
/// Small codes mean the signal is quieter than the step allows, so the step
/// shrinks; large codes mean it is clipping, so the step grows fast. This
/// asymmetry — down by one, up by as much as eight — is why ADPCM recovers
/// from a transient in a handful of samples rather than a hundred.
const INDEX_TABLE: [i32; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];

/// One channel's adaptation, which is all the decoder needs to start.
///
/// The default is silence at the finest step, which is where a cold encoder
/// begins and where a decoder would begin if a packet header were all zeroes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Adapt {
    predictor: i32,
    index: i32,
}

impl Adapt {
    /// Quantise one sample against the current prediction and adapt.
    fn encode(&mut self, sample: i16) -> u8 {
        let step = STEP_TABLE[self.index as usize];
        let delta = sample as i32 - self.predictor;

        // Sign in bit 3, magnitude in bits 0..3 as a fraction of the step.
        let mut code = if delta < 0 { 8u8 } else { 0u8 };
        let magnitude = delta.abs();

        // The three magnitude bits are step/2, step/4, step/8 — a binary
        // search down the step, which is the whole of the ADPCM quantiser.
        let mut remainder = magnitude;
        let mut reconstructed = step >> 3;
        let mut bit = 4u8;
        let mut scale = step;
        for _ in 0..3 {
            if remainder >= scale {
                code |= bit;
                remainder -= scale;
                reconstructed += scale;
            }
            scale >>= 1;
            bit >>= 1;
        }

        self.advance(code, reconstructed);
        code
    }

    /// Reconstruct one sample from a nibble and adapt identically.
    fn decode(&mut self, code: u8) -> i16 {
        let step = STEP_TABLE[self.index as usize];

        let mut reconstructed = step >> 3;
        if code & 4 != 0 {
            reconstructed += step;
        }
        if code & 2 != 0 {
            reconstructed += step >> 1;
        }
        if code & 1 != 0 {
            reconstructed += step >> 2;
        }

        self.advance(code, reconstructed);
        self.predictor as i16
    }

    /// The half both directions must perform identically.
    ///
    /// Encoder and decoder walk the same state machine from the same starting
    /// point; if they diverge by one step index the channel drifts into noise.
    /// Keeping it in one function is what stops that.
    fn advance(&mut self, code: u8, reconstructed: i32) {
        if code & 8 != 0 {
            self.predictor -= reconstructed;
        } else {
            self.predictor += reconstructed;
        }
        self.predictor = self.predictor.clamp(i16::MIN as i32, i16::MAX as i32);

        self.index =
            (self.index + INDEX_TABLE[code as usize]).clamp(0, STEP_TABLE.len() as i32 - 1);
    }

    fn write(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.predictor as i16).to_le_bytes());
        out.push(self.index as u8);
    }

    /// Read a channel's state out of a packet header.
    ///
    /// The index is clamped rather than refused: it is a peer-chosen byte, and
    /// a value past the end of the step table would index out of bounds. A
    /// clamped index plays slightly wrong audio; an unchecked one panics.
    fn read(bytes: &[u8]) -> Adapt {
        Adapt {
            predictor: i16::from_le_bytes([bytes[0], bytes[1]]) as i32,
            index: (bytes[2] as i32).clamp(0, STEP_TABLE.len() as i32 - 1),
        }
    }
}

struct AdpcmEncoder {
    format: AudioFormat,
    channels: Vec<Adapt>,
}

impl AdpcmEncoder {
    fn new(format: AudioFormat) -> AdpcmEncoder {
        AdpcmEncoder {
            channels: vec![Adapt::default(); format.channels.max(1) as usize],
            format,
        }
    }
}

impl AudioEncoder for AdpcmEncoder {
    fn encode(&mut self, samples: &[i16], out: &mut Vec<u8>) -> Result<(), Error> {
        expect_samples(samples.len(), self.format.packet_samples())?;
        out.clear();
        out.reserve(self.format.packet_bytes());

        // State first, so the decoder can start from where this packet begins
        // rather than from silence. Written before encoding, because encoding
        // is what changes it.
        for channel in &self.channels {
            channel.write(out);
        }

        let channels = self.channels.len();
        let mut pending: Option<u8> = None;
        for (index, &sample) in samples.iter().enumerate() {
            let code = self.channels[index % channels].encode(sample);
            match pending.take() {
                Some(low) => out.push(low | (code << 4)),
                None => pending = Some(code),
            }
        }
        if let Some(low) = pending {
            out.push(low);
        }

        Ok(())
    }

    fn codec(&self) -> AudioCodec {
        AudioCodec::Adpcm4
    }
}

struct AdpcmDecoder {
    format: AudioFormat,
}

impl AudioDecoder for AdpcmDecoder {
    fn decode(&mut self, packet: &[u8], out: &mut Vec<i16>) -> Result<(), Error> {
        expect_packet(packet.len(), self.format.packet_bytes())?;

        let channels = self.format.channels.max(1) as usize;
        let mut state: Vec<Adapt> = (0..channels)
            .map(|channel| Adapt::read(&packet[channel * ADPCM_STATE_BYTES..]))
            .collect();

        let nibbles = &packet[channels * ADPCM_STATE_BYTES..];
        let wanted = self.format.packet_samples();
        out.clear();
        out.reserve(wanted);

        for index in 0..wanted {
            let byte = nibbles[index / 2];
            let code = if index % 2 == 0 {
                byte & 0x0f
            } else {
                byte >> 4
            };
            out.push(state[index % channels].decode(code));
        }

        Ok(())
    }

    fn codec(&self) -> AudioCodec {
        AudioCodec::Adpcm4
    }
}

// ------------------------------------------------------------------ shared

fn expect_samples(got: usize, wanted: usize) -> Result<(), Error> {
    if got == wanted {
        Ok(())
    } else {
        Err(Error::Packet {
            wanted,
            got,
            what: "samples",
        })
    }
}

fn expect_packet(got: usize, wanted: usize) -> Result<(), Error> {
    if got == wanted {
        Ok(())
    } else {
        Err(Error::Packet {
            wanted,
            got,
            what: "bytes",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pravera_core::audio::{PACKET_FRAMES, PACKET_SAMPLES};

    /// A packet of something with structure: a tone, so quantiser error is
    /// audible in the numbers rather than hidden in noise.
    fn tone(offset: usize, amplitude: f32) -> Vec<i16> {
        (0..PACKET_SAMPLES)
            .map(|i| {
                let t = (offset + i / 2) as f32 / 48_000.0;
                (amplitude * (t * 440.0 * std::f32::consts::TAU).sin()) as i16
            })
            .collect()
    }

    fn round_trip(codec: AudioCodec, samples: &[i16]) -> Vec<i16> {
        let format = AudioFormat::new(codec);
        let mut packet = Vec::new();
        encoder(format)
            .encode(samples, &mut packet)
            .expect("encode");
        assert_eq!(packet.len(), format.packet_bytes());

        let mut out = Vec::new();
        decoder(format).decode(&packet, &mut out).expect("decode");
        out
    }

    #[test]
    fn pcm_gives_back_exactly_what_it_was_given() {
        let samples = tone(0, 20_000.0);
        assert_eq!(round_trip(AudioCodec::Pcm16, &samples), samples);
    }

    #[test]
    fn pcm_survives_the_extremes_of_the_range() {
        // `i16::MIN` has no positive counterpart, which is where naive
        // conversions overflow.
        let mut samples = vec![0i16; PACKET_SAMPLES];
        samples[0] = i16::MIN;
        samples[1] = i16::MAX;
        samples[2] = -1;
        assert_eq!(round_trip(AudioCodec::Pcm16, &samples), samples);
    }

    /// Signal-to-noise ratio in decibels, against the original.
    fn snr(original: &[i16], decoded: &[i16]) -> f64 {
        let signal: f64 = original.iter().map(|s| (*s as f64).powi(2)).sum();
        let noise: f64 = original
            .iter()
            .zip(decoded)
            .map(|(a, b)| ((*a as f64) - (*b as f64)).powi(2))
            .sum();
        if noise == 0.0 {
            return f64::INFINITY;
        }
        10.0 * (signal / noise).log10()
    }

    #[test]
    fn adpcm_reproduces_a_tone_closely_enough_to_hear_it_as_the_same_tone() {
        // Measured on a warm encoder, which is the state it spends the whole
        // session in. The cold first packet is worse and has its own test.
        let format = AudioFormat::new(AudioCodec::Adpcm4);
        let mut encoder = encoder(format);
        let mut decoder = decoder(format);

        let mut original = Vec::new();
        let mut decoded = Vec::new();
        for index in 0..8 {
            original = tone(index * PACKET_FRAMES, 20_000.0);
            let mut packet = Vec::new();
            encoder.encode(&original, &mut packet).expect("encode");
            decoder.decode(&packet, &mut decoded).expect("decode");
        }

        assert_eq!(decoded.len(), original.len());
        let ratio = snr(&original, &decoded);
        assert!(
            ratio > 30.0,
            "ADPCM managed only {ratio:.1} dB, which is audible on a steady tone"
        );
    }

    #[test]
    fn the_first_packet_after_silence_is_worse_but_not_broken() {
        // A cold encoder starts at the finest quantiser step and has to climb
        // to the signal, which costs a few samples of visible slew. It is
        // worth measuring rather than assuming: if this ever collapses, audio
        // will click at the start of every burst on an otherwise quiet host.
        let samples = tone(0, 20_000.0);
        let out = round_trip(AudioCodec::Adpcm4, &samples);
        let ratio = snr(&samples, &out);
        assert!(ratio > 20.0, "a cold packet managed only {ratio:.1} dB");
    }

    #[test]
    fn adpcm_holds_its_adaptation_across_packets() {
        // The point of writing state into each packet rather than restarting
        // from silence: a steady tone should not be re-attacked every five
        // milliseconds. The second packet, encoded with a warm encoder,
        // must come out closer than the first, which started cold.
        let format = AudioFormat::new(AudioCodec::Adpcm4);
        let mut encoder = encoder(format);
        let mut decoder = decoder(format);

        let mut errors = Vec::new();
        for packet_index in 0..4 {
            let samples = tone(packet_index * PACKET_SAMPLES / 2, 20_000.0);
            let mut packet = Vec::new();
            encoder.encode(&samples, &mut packet).expect("encode");
            let mut out = Vec::new();
            decoder.decode(&packet, &mut out).expect("decode");

            let worst = samples
                .iter()
                .zip(&out)
                .map(|(a, b)| (*a as i32 - *b as i32).abs())
                .max()
                .expect("samples");
            errors.push(worst);
        }

        assert!(
            errors[3] < errors[0],
            "adaptation is not carrying over: {errors:?}"
        );
    }

    #[test]
    fn a_packet_decodes_without_the_packet_before_it() {
        // The property the whole datagram path rests on. Decode packet three
        // on a decoder that never saw one or two, and it must match what a
        // decoder that saw all three produces.
        let format = AudioFormat::new(AudioCodec::Adpcm4);
        let mut encoder = encoder(format);

        let mut packets = Vec::new();
        for index in 0..3 {
            let mut packet = Vec::new();
            encoder
                .encode(&tone(index * PACKET_SAMPLES / 2, 18_000.0), &mut packet)
                .expect("encode");
            packets.push(packet);
        }

        let mut continuous = decoder(format);
        let mut in_sequence = Vec::new();
        for packet in &packets {
            continuous.decode(packet, &mut in_sequence).expect("decode");
        }

        let mut cold = decoder(format);
        let mut alone = Vec::new();
        cold.decode(&packets[2], &mut alone).expect("decode");

        assert_eq!(
            alone, in_sequence,
            "the last packet needed its predecessors"
        );
    }

    #[test]
    fn a_packet_of_the_wrong_length_is_refused_rather_than_indexing_off_the_end() {
        // Every byte of a packet came off the network.
        for codec in [AudioCodec::Pcm16, AudioCodec::Adpcm4] {
            let format = AudioFormat::new(codec);
            let mut out = Vec::new();
            let mut decoder = decoder(format);

            assert!(decoder.decode(&[], &mut out).is_err());
            assert!(decoder.decode(&[0u8; 3], &mut out).is_err());
            assert!(decoder
                .decode(&vec![0u8; format.packet_bytes() - 1], &mut out)
                .is_err());
            assert!(decoder
                .decode(&vec![0u8; format.packet_bytes() + 1], &mut out)
                .is_err());
        }
    }

    #[test]
    fn a_step_index_past_the_table_is_clamped_rather_than_panicking() {
        // A peer can put any byte in the state header. Without the clamp this
        // indexes past a 89-entry table and takes the session down.
        let format = AudioFormat::new(AudioCodec::Adpcm4);
        let mut packet = vec![0u8; format.packet_bytes()];
        packet[2] = 250;
        packet[ADPCM_STATE_BYTES + 2] = 89;

        let mut out = Vec::new();
        decoder(format).decode(&packet, &mut out).expect("decode");
        assert_eq!(out.len(), format.packet_samples());
    }

    #[test]
    fn encoding_the_wrong_number_of_samples_is_a_refusal_not_a_partial_packet() {
        for codec in [AudioCodec::Pcm16, AudioCodec::Adpcm4] {
            let format = AudioFormat::new(codec);
            let mut out = Vec::new();
            assert!(encoder(format).encode(&[0i16; 4], &mut out).is_err());
        }
    }

    #[test]
    fn silence_stays_silent() {
        // The dullest case and the one people notice: a quiet desktop must not
        // hiss. ADPCM's minimum step is 7, so the floor is a couple of LSBs.
        let out = round_trip(AudioCodec::Adpcm4, &vec![0i16; PACKET_SAMPLES]);
        let loudest = out.iter().map(|s| s.abs()).max().expect("samples");
        assert!(loudest <= 4, "silence decoded to {loudest}");
    }

    #[test]
    fn every_codec_reports_the_codec_it_actually_is() {
        for codec in [AudioCodec::Pcm16, AudioCodec::Adpcm4] {
            let format = AudioFormat::new(codec);
            assert_eq!(encoder(format).codec(), codec);
            assert_eq!(decoder(format).codec(), codec);
        }
    }
}
