//! The shape of an audio stream, as both ends agree to describe it.
//!
//! Deliberately small. Everything platform-shaped — devices, buffers, the
//! WASAPI mix format — lives in `pravera-audio`; what is here is only what has
//! to travel in the handshake so the client knows what is arriving.
//!
//! ## Why the wire is always 48 kHz stereo
//!
//! The host's mixer runs at whatever the person set in the sound control panel:
//! 44.1 kHz, 48 kHz, occasionally 96, in stereo or 7.1. Carrying that through
//! would push the conversion onto the client, which is the one machine that
//! cannot test it. So the host converts once, at the source, and the wire
//! carries a single shape. [`AudioFormat`] still names it rather than assuming
//! it, because a client that renders what it is *told* stays correct when this
//! changes, and one that hardcodes 48 kHz does not.
//!
//! ## Why not Opus
//!
//! The protocol was written expecting Opus, and it is still the right codec.
//! Neither binding on crates.io can be built here: `audiopus` links a prebuilt
//! `libopus.lib` dated January 2020 that ships inside the crate — an opaque
//! third-party binary decoding attacker-controlled bytes, which is the same
//! objection that ruled out `hwcodec` — and `magnum-opus` builds from source
//! through CMake, which cannot find the MSVC toolchain that `cc` finds without
//! trouble. Rather than ship the blob or a build that breaks on a machine
//! without a Developer Command Prompt, audio travels as PCM or as ADPCM, both
//! of which are a few dozen lines of Rust that can be read and tested. The
//! codec is named on the wire, so adding Opus later is an encoder, a decoder
//! and one more variant.

use serde::{Deserialize, Serialize};

/// Samples per second on the wire.
pub const SAMPLE_RATE: u32 = 48_000;

/// Channels on the wire.
pub const CHANNELS: u8 = 2;

/// How much audio one packet carries.
///
/// Five milliseconds, chosen so that one packet fits in one datagram and never
/// has to be reassembled: 240 frames of stereo 16-bit PCM is 960 bytes, inside
/// the 1086 a chunk may hold. A packet that spanned two datagrams would turn
/// every single lost datagram into a lost packet, doubling the audible cost of
/// loss for no gain.
///
/// It also sets the floor on added latency. Everything downstream — the
/// jitter buffer, the device period — is a multiple of this.
pub const PACKET_MILLIS: u32 = 5;

/// Sample frames in one packet. A *frame* here is one sample per channel.
pub const PACKET_FRAMES: usize = (SAMPLE_RATE * PACKET_MILLIS / 1000) as usize;

/// Individual samples in one packet, across all channels.
pub const PACKET_SAMPLES: usize = PACKET_FRAMES * CHANNELS as usize;

/// How audio is compressed for the wire.
///
/// Both are self-contained per packet: nothing in one packet depends on the
/// one before it. That is the property that matters on a datagram path, where
/// a lost packet must cost exactly its own five milliseconds and nothing after.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AudioCodec {
    /// Signed 16-bit little-endian, interleaved. Transparent, 1.5 Mbps.
    Pcm16,
    /// IMA ADPCM at four bits a sample: 4:1 on the samples, 394 kbps
    /// once the per-packet adaptive state is counted.
    Adpcm4,
}

impl AudioCodec {
    pub const fn name(self) -> &'static str {
        match self {
            AudioCodec::Pcm16 => "PCM 16",
            AudioCodec::Adpcm4 => "ADPCM",
        }
    }

    /// Bits carried per sample, not counting per-packet overhead.
    pub const fn bits_per_sample(self) -> u32 {
        match self {
            AudioCodec::Pcm16 => 16,
            AudioCodec::Adpcm4 => 4,
        }
    }

    /// Exactly how many bytes one packet of this codec occupies.
    ///
    /// `samples` counts every sample across every channel. ADPCM adds three
    /// bytes per channel: the adaptive state the decoder needs to start from,
    /// written into every packet so no packet depends on the one before it.
    pub const fn packet_bytes(self, samples: usize, channels: u8) -> usize {
        match self {
            AudioCodec::Pcm16 => samples * 2,
            AudioCodec::Adpcm4 => channels as usize * ADPCM_STATE_BYTES + samples.div_ceil(2),
        }
    }

    /// Whether the far end hears exactly what was played.
    pub const fn is_lossless(self) -> bool {
        matches!(self, AudioCodec::Pcm16)
    }
}

/// Bytes of adaptive state ADPCM writes per channel, per packet.
///
/// A 16-bit predictor and the index into the step table. Carrying it means the
/// encoder can keep adapting across packet boundaries — which is what stops a
/// click every five milliseconds — while each packet still decodes alone.
pub const ADPCM_STATE_BYTES: usize = 3;

/// What the host agreed to send, and what the client should expect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u8,
    pub codec: AudioCodec,
    /// Sample frames in one packet, so the client can size its buffers from
    /// the handshake rather than from a constant it has to keep in step.
    pub packet_frames: u32,
}

impl AudioFormat {
    pub const fn new(codec: AudioCodec) -> AudioFormat {
        AudioFormat {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            codec,
            packet_frames: PACKET_FRAMES as u32,
        }
    }

    /// Individual samples in one packet, across all channels.
    pub const fn packet_samples(self) -> usize {
        self.packet_frames as usize * self.channels as usize
    }

    /// Exactly how many bytes one packet occupies on the wire.
    pub const fn packet_bytes(self) -> usize {
        self.codec
            .packet_bytes(self.packet_samples(), self.channels)
    }

    /// Bits per second this stream costs, overhead included.
    ///
    /// Exact rather than nominal, because both codecs are fixed-rate — which
    /// is half of why they are here. A variable-rate codec would need a
    /// measurement, and this crate does not take measurements.
    pub const fn bitrate(self) -> u32 {
        let micros = self.packet_micros() as u64;
        if micros == 0 {
            return 0;
        }
        (self.packet_bytes() as u64 * 8 * 1_000_000 / micros) as u32
    }

    /// How long one packet lasts, in microseconds.
    ///
    /// Zero if the format claims a rate of zero, which only a malformed
    /// handshake produces; the caller treats that as a format it cannot play.
    pub const fn packet_micros(self) -> u32 {
        if self.sample_rate == 0 {
            return 0;
        }
        (self.packet_frames as u64 * 1_000_000 / self.sample_rate as u64) as u32
    }

    /// Whether this is a format anything here can actually render.
    ///
    /// Checked on the client because every field came off the wire. A packet
    /// of zero frames would divide by zero downstream; an implausible channel
    /// count or rate would size a buffer from a number the peer chose.
    pub fn is_playable(self) -> bool {
        (8_000..=192_000).contains(&self.sample_rate)
            && (1..=2).contains(&self.channels)
            && (1..=4800).contains(&self.packet_frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_format_describes_the_wire() {
        let format = AudioFormat::new(AudioCodec::Pcm16);
        assert_eq!(format.sample_rate, SAMPLE_RATE);
        assert_eq!(format.channels, CHANNELS);
        assert_eq!(format.packet_samples(), PACKET_SAMPLES);
        assert_eq!(format.packet_micros(), PACKET_MILLIS * 1000);
    }

    #[test]
    fn adpcm_costs_about_a_quarter_of_what_pcm_costs() {
        let pcm = AudioFormat::new(AudioCodec::Pcm16);
        let adpcm = AudioFormat::new(AudioCodec::Adpcm4);

        assert_eq!(pcm.packet_bytes(), PACKET_SAMPLES * 2);
        assert_eq!(pcm.bitrate(), 1_536_000);

        // A quarter of the samples plus six bytes of adaptive state, which is
        // what a packet pays to be decodable without the one before it.
        assert_eq!(
            adpcm.packet_bytes(),
            PACKET_SAMPLES / 2 + 2 * ADPCM_STATE_BYTES
        );
        assert_eq!(adpcm.bitrate(), 393_600);

        assert!(AudioCodec::Pcm16.is_lossless());
        assert!(!AudioCodec::Adpcm4.is_lossless());
    }

    #[test]
    fn a_stream_that_cannot_be_timed_reports_no_bitrate() {
        // Rather than dividing by zero, or quoting a figure nothing measured.
        let mut format = AudioFormat::new(AudioCodec::Pcm16);
        format.sample_rate = 0;
        assert_eq!(format.bitrate(), 0);
    }

    #[test]
    fn a_format_that_would_divide_by_zero_is_not_playable() {
        // Every field here came off the wire, so each one is a peer's choice.
        let mut format = AudioFormat::new(AudioCodec::Pcm16);
        assert!(format.is_playable());

        format.packet_frames = 0;
        assert!(!format.is_playable());
        assert_eq!(format.packet_samples(), 0);

        format = AudioFormat::new(AudioCodec::Pcm16);
        format.sample_rate = 0;
        assert!(!format.is_playable());
        assert_eq!(format.packet_micros(), 0);

        format = AudioFormat::new(AudioCodec::Pcm16);
        format.channels = 32;
        assert!(!format.is_playable());
    }
}
