//! Getting whatever the sound card produces into the one shape the wire uses.
//!
//! The host's mixer runs at the rate and channel count the person chose in the
//! sound control panel, in whichever sample type the driver prefers: 32-bit
//! float on almost every modern Windows machine, 16- or 32-bit integer on
//! older ones, occasionally 24-bit packed into three bytes. The wire carries
//! one shape. This module is the funnel between them, and it runs on the host,
//! once, so the client never has to guess.
//!
//! ## Why the resampler is linear
//!
//! The only ratio that matters in practice is 44100 to 48000, a factor of
//! 1.088. Linear interpolation at that ratio puts its distortion products
//! close to the signal in level but high in frequency, where desktop audio has
//! almost nothing and where nobody is listening for it. A windowed-sinc
//! resampler would be better and would cost a table, a phase accumulator and a
//! week of getting the group delay right. If audio quality ever becomes the
//! complaint, this is the first place to look — and the tests below are the
//! ones that will tell you whether a replacement is actually better.

use pravera_core::audio::CHANNELS;

/// How the device hands over its samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleType {
    /// 32-bit IEEE float in `-1.0..=1.0`. What Windows mixes in.
    F32,
    I16,
    I32,
    /// Three bytes per sample, little-endian, signed.
    I24,
}

impl SampleType {
    pub const fn bytes(self) -> usize {
        match self {
            SampleType::F32 | SampleType::I32 => 4,
            SampleType::I16 => 2,
            SampleType::I24 => 3,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            SampleType::F32 => "f32",
            SampleType::I16 => "i16",
            SampleType::I32 => "i32",
            SampleType::I24 => "i24",
        }
    }

    /// Read one sample and scale it into the 16-bit range.
    ///
    /// Float is clamped before scaling: WASAPI's mixer can and does produce
    /// values outside ±1.0 when several loud streams are mixed, and letting
    /// those wrap turns a loud passage into a burst of noise.
    fn read(self, bytes: &[u8]) -> i16 {
        match self {
            SampleType::F32 => {
                let value = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                // 32767.0 rather than 32768.0, so +1.0 lands exactly on the
                // top of the range instead of wrapping to the bottom.
                (value.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
            }
            SampleType::I16 => i16::from_le_bytes([bytes[0], bytes[1]]),
            SampleType::I32 => {
                (i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) >> 16) as i16
            }
            SampleType::I24 => {
                // Sign-extend by placing the three bytes in the top of an i32.
                let value = i32::from_le_bytes([0, bytes[0], bytes[1], bytes[2]]);
                (value >> 16) as i16
            }
        }
    }
}

/// The shape a device is actually delivering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_type: SampleType,
}

impl DeviceFormat {
    /// Bytes in one sample frame, across all channels.
    pub const fn frame_bytes(self) -> usize {
        self.channels as usize * self.sample_type.bytes()
    }

    /// Whether this is a format the converter can read at all.
    pub fn is_usable(self) -> bool {
        self.channels > 0 && (8_000..=384_000).contains(&self.sample_rate)
    }
}

impl std::fmt::Display for DeviceFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} Hz, {} ch, {}",
            self.sample_rate,
            self.channels,
            self.sample_type.name()
        )
    }
}

/// Converts a device's stream into interleaved 48 kHz stereo `i16`.
///
/// Stateful across calls, because both halves have to be: the resampler keeps
/// its fractional position and the sample it needs to interpolate from, and
/// dropping either at a buffer boundary would put a discontinuity into the
/// audio every time the device delivered.
pub struct Converter {
    from: DeviceFormat,
    to_rate: u32,
    /// The last stereo frame read, held so interpolation can reach back across
    /// a call boundary.
    previous: Option<[i16; CHANNELS as usize]>,
    /// Position between `previous` and the next input frame, in 1/`to_rate`
    /// units scaled by the input rate. Kept as an integer so repeated
    /// conversion cannot drift the way a float accumulator does.
    position: u64,
}

impl Converter {
    pub fn new(from: DeviceFormat, to_rate: u32) -> Converter {
        Converter {
            from,
            to_rate,
            previous: None,
            position: 0,
        }
    }

    pub fn source_format(&self) -> DeviceFormat {
        self.from
    }

    /// Whether anything actually has to be done, beyond copying.
    pub fn is_identity(&self) -> bool {
        self.from.sample_rate == self.to_rate
            && self.from.channels == CHANNELS as u16
            && self.from.sample_type == SampleType::I16
    }

    /// Convert one device buffer, appending to `out`.
    ///
    /// A trailing partial frame is ignored rather than being padded with
    /// silence: WASAPI delivers whole frames, so a partial one means the
    /// buffer length was miscomputed, and half a frame of made-up audio would
    /// hide that rather than showing it.
    pub fn push(&mut self, bytes: &[u8], out: &mut Vec<i16>) {
        let stride = self.from.frame_bytes();
        if stride == 0 {
            return;
        }

        for frame in bytes.chunks_exact(stride) {
            let stereo = self.downmix(frame);
            self.resample(stereo, out);
        }
    }

    /// Fold however many channels the device has into two.
    ///
    /// Mono is duplicated. More than two takes the first two, which for a 5.1
    /// or 7.1 mixer are front left and front right — the pair a stereo
    /// listener is meant to hear. Averaging the surrounds in would be louder
    /// and muddier, and would put the centre channel's dialogue at the wrong
    /// level relative to everything else.
    fn downmix(&self, frame: &[u8]) -> [i16; CHANNELS as usize] {
        let width = self.from.sample_type.bytes();
        let read = |channel: usize| self.from.sample_type.read(&frame[channel * width..]);

        match self.from.channels {
            0 => [0, 0],
            1 => {
                let mono = read(0);
                [mono, mono]
            }
            _ => [read(0), read(1)],
        }
    }

    /// Emit however many output frames this input frame is worth.
    fn resample(&mut self, next: [i16; CHANNELS as usize], out: &mut Vec<i16>) {
        if self.from.sample_rate == self.to_rate {
            // Nothing to interpolate between, so nothing is held back.
            out.extend_from_slice(&next);
            return;
        }

        let Some(previous) = self.previous else {
            // Nothing to interpolate *from* yet. The first input frame becomes
            // the left-hand endpoint and produces nothing itself.
            self.previous = Some(next);
            return;
        };

        let from = self.from.sample_rate as u64;
        let to = self.to_rate as u64;

        // Walk output positions while they fall between `previous` and `next`.
        // `position` counts in units of one input frame scaled by `to`, which
        // keeps every step exact.
        while self.position < to {
            for channel in 0..CHANNELS as usize {
                let a = previous[channel] as i64;
                let b = next[channel] as i64;
                let blended = a + (b - a) * self.position as i64 / to as i64;
                out.push(blended as i16);
            }
            self.position += from;
        }
        self.position -= to;
        self.previous = Some(next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_of(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn a_stereo_float_device_at_the_wire_rate_needs_only_scaling() {
        let format = DeviceFormat {
            sample_rate: 48_000,
            channels: 2,
            sample_type: SampleType::F32,
        };
        let mut converter = Converter::new(format, 48_000);
        assert!(!converter.is_identity());

        let mut out = Vec::new();
        converter.push(&bytes_of(&[0.0, 0.0, 1.0, -1.0, 0.5, -0.5]), &mut out);
        assert_eq!(out, vec![0, 0, 32767, -32767, 16383, -16383]);
    }

    #[test]
    fn a_float_above_full_scale_clips_instead_of_wrapping() {
        // WASAPI's mixer really does produce these when several loud streams
        // are playing. Wrapping turns a loud passage into a burst of noise.
        let format = DeviceFormat {
            sample_rate: 48_000,
            channels: 2,
            sample_type: SampleType::F32,
        };
        let mut converter = Converter::new(format, 48_000);
        let mut out = Vec::new();
        converter.push(&bytes_of(&[0.0, 0.0, 4.2, -4.2]), &mut out);
        assert_eq!(out, vec![0, 0, i16::MAX, -i16::MAX]);
    }

    #[test]
    fn a_mono_device_is_heard_in_both_ears() {
        let format = DeviceFormat {
            sample_rate: 48_000,
            channels: 1,
            sample_type: SampleType::I16,
        };
        let mut converter = Converter::new(format, 48_000);
        let mut out = Vec::new();
        let input: Vec<u8> = [1000i16, 2000, -3000]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        converter.push(&input, &mut out);
        assert_eq!(out, vec![1000, 1000, 2000, 2000, -3000, -3000]);
    }

    #[test]
    fn a_surround_device_is_folded_to_its_front_pair() {
        // Channels 0 and 1 are front left and front right in every layout
        // Windows produces. Averaging the surrounds in would be louder and
        // would put centre-channel dialogue at the wrong level.
        let format = DeviceFormat {
            sample_rate: 48_000,
            channels: 6,
            sample_type: SampleType::I16,
        };
        let mut converter = Converter::new(format, 48_000);
        let mut out = Vec::new();
        let input: Vec<u8> = [0i16; 6]
            .iter()
            .chain([100i16, 200, 300, 400, 500, 600].iter())
            .flat_map(|s| s.to_le_bytes())
            .collect();
        converter.push(&input, &mut out);
        assert_eq!(out, vec![0, 0, 100, 200]);
    }

    #[test]
    fn forty_four_one_becomes_forty_eight_at_the_right_length() {
        // The ratio that actually matters. A second of 44.1 kHz must come out
        // as a second of 48 kHz, within one frame of rounding.
        let format = DeviceFormat {
            sample_rate: 44_100,
            channels: 2,
            sample_type: SampleType::I16,
        };
        let mut converter = Converter::new(format, 48_000);

        let input: Vec<u8> = (0..44_100)
            .flat_map(|i| {
                let s = ((i % 100) as i16) * 100;
                [s.to_le_bytes(), s.to_le_bytes()]
            })
            .flatten()
            .collect();

        let mut out = Vec::new();
        converter.push(&input, &mut out);

        let frames = out.len() / 2;
        assert!(
            (47_950..=48_050).contains(&frames),
            "44.1 kHz became {frames} frames instead of 48000"
        );
    }

    #[test]
    fn resampling_across_two_buffers_matches_resampling_one() {
        // The state that has to survive a call boundary: the phase and the
        // previous frame. Losing either puts a click at every buffer edge,
        // which at a 10 ms device period is a hundred clicks a second.
        let format = DeviceFormat {
            sample_rate: 44_100,
            channels: 2,
            sample_type: SampleType::I16,
        };

        let input: Vec<u8> = (0..2_000i16)
            .flat_map(|i| [(i * 7).to_le_bytes(), (i * 7).to_le_bytes()])
            .flatten()
            .collect();

        let mut whole = Vec::new();
        Converter::new(format, 48_000).push(&input, &mut whole);

        let mut split = Vec::new();
        let mut converter = Converter::new(format, 48_000);
        let (head, tail) = input.split_at(input.len() / 2);
        converter.push(head, &mut split);
        converter.push(tail, &mut split);

        assert_eq!(whole, split);
    }

    #[test]
    fn a_partial_trailing_frame_is_ignored_rather_than_invented() {
        let format = DeviceFormat {
            sample_rate: 48_000,
            channels: 2,
            sample_type: SampleType::I16,
        };
        let mut converter = Converter::new(format, 48_000);
        let mut out = Vec::new();
        // Two whole frames plus one stray byte.
        converter.push(&[0, 0, 0, 0, 1, 0, 2, 0, 0xff], &mut out);
        assert_eq!(out, vec![0, 0, 1, 2]);
    }

    #[test]
    fn a_device_with_no_channels_produces_nothing_rather_than_dividing_by_zero() {
        let format = DeviceFormat {
            sample_rate: 48_000,
            channels: 0,
            sample_type: SampleType::I16,
        };
        assert!(!format.is_usable());
        let mut out = Vec::new();
        Converter::new(format, 48_000).push(&[1, 2, 3, 4], &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn wider_integer_samples_land_in_the_top_sixteen_bits() {
        let mut out = Vec::new();
        let format = DeviceFormat {
            sample_rate: 48_000,
            channels: 2,
            sample_type: SampleType::I32,
        };
        let input: Vec<u8> = [0i32, 0, 0x1234_5678, -0x1234_5678]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        Converter::new(format, 48_000).push(&input, &mut out);
        assert_eq!(out, vec![0, 0, 0x1234, -0x1235]);

        out.clear();
        let format = DeviceFormat {
            sample_rate: 48_000,
            channels: 2,
            sample_type: SampleType::I24,
        };
        // 0x123456 and its negation, three bytes each, little-endian.
        let input: Vec<u8> = vec![0, 0, 0, 0, 0, 0, 0x56, 0x34, 0x12, 0xaa, 0xcb, 0xed];
        Converter::new(format, 48_000).push(&input, &mut out);
        assert_eq!(out, vec![0, 0, 0x1234, -0x1235]);
    }
}
