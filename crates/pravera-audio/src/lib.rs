//! Desktop audio: off the host's mixer, onto the wire, out of the client's
//! speakers.
//!
//! Four pieces, in the order a sample travels through them:
//!
//! 1. [`Loopback`] taps whatever the host is playing — not a microphone, the
//!    mix itself, so a video on the host is heard on the client.
//! 2. [`convert::Converter`] folds the device's rate, channel count and sample
//!    type into the single shape the wire carries.
//! 3. [`codec`] packs five milliseconds at a time into a packet small enough
//!    for one datagram.
//! 4. [`Speaker`] plays the packets back out on the client, through a buffer
//!    deep enough to absorb the jitter of the path and no deeper.
//!
//! ## Two traits, one platform
//!
//! Only Windows is implemented. [`loopback`] and [`speaker`] return
//! [`Error::Unsupported`] elsewhere, which the host and client treat as "this
//! session has no audio" rather than as a failure — a remote desktop with no
//! sound is worth having, and one that refuses to start because of the sound
//! card is not.

pub mod codec;
pub mod convert;

#[cfg(windows)]
mod windows;

use pravera_core::audio::AudioFormat;

pub use codec::{decoder, encoder, AudioDecoder, AudioEncoder};
pub use convert::{Converter, DeviceFormat, SampleType};

/// What can go wrong between the mixer and the speakers.
///
/// Deliberately without paths, device names or driver strings in the variants
/// that a peer can provoke: an error crossing the wire must not describe the
/// host's hardware. `Device` is host-local and logged, never sent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// This platform has no implementation yet.
    #[error("{0}")]
    Unsupported(&'static str),

    /// The audio stack refused. Carries what the platform said, for the log.
    #[error("{0}")]
    Device(String),

    /// A buffer was not the length the format calls for.
    #[error("expected {wanted} {what}, got {got}")]
    Packet {
        wanted: usize,
        got: usize,
        what: &'static str,
    },
}

/// Captures what the host is playing.
///
/// A *loopback* tap, not an input device: it reads the mix on its way to the
/// speakers. Nothing on the host has to be reconfigured, and nothing the host
/// hears is missed.
pub trait Loopback: Send {
    /// The shape this device is delivering.
    fn format(&self) -> DeviceFormat;

    /// Take whatever has accumulated since the last call.
    ///
    /// Appends raw device bytes to `out`, in [`Loopback::format`]'s layout.
    /// Returns how many bytes were appended. Zero is ordinary and means the
    /// host is silent, not that anything is wrong.
    fn read(&mut self, out: &mut Vec<u8>) -> Result<usize, Error>;

    /// Block until there is something to read, or the timeout expires.
    ///
    /// Returns whether the device signalled. A false is a timeout, which the
    /// caller uses to check whether it has been told to stop.
    fn wait(&mut self, timeout: std::time::Duration) -> bool;

    fn name(&self) -> &str;
}

/// Plays audio out of the client's default device.
pub trait Speaker: Send {
    /// How many sample frames the device will accept right now.
    fn space(&mut self) -> Result<usize, Error>;

    /// Hand over interleaved samples in the session's format.
    ///
    /// Writes as many whole frames as `space` allows and returns how many
    /// frames were taken, which may be fewer than offered.
    fn write(&mut self, samples: &[i16]) -> Result<usize, Error>;

    /// Sample frames sitting in the device buffer, still to be heard.
    ///
    /// This is the client's own contribution to audio latency, and the number
    /// the jitter buffer steers against.
    fn queued(&mut self) -> Result<usize, Error>;

    /// Block until the device has room, or the timeout expires.
    fn wait(&mut self, timeout: std::time::Duration) -> bool;

    fn name(&self) -> &str;
}

/// Open a tap on whatever this machine is playing.
pub fn loopback() -> Result<Box<dyn Loopback>, Error> {
    #[cfg(windows)]
    {
        windows::open_loopback()
    }
    #[cfg(not(windows))]
    {
        Err(Error::Unsupported(
            "capturing desktop audio is not implemented on this platform yet",
        ))
    }
}

/// Open the default playback device for the agreed format.
pub fn speaker(format: AudioFormat) -> Result<Box<dyn Speaker>, Error> {
    #[cfg(windows)]
    {
        windows::open_speaker(format)
    }
    #[cfg(not(windows))]
    {
        let _ = format;
        Err(Error::Unsupported(
            "playing remote audio is not implemented on this platform yet",
        ))
    }
}

/// Audio codecs this machine can produce, best first.
///
/// Empty when the platform has no loopback tap, which is what stops a host
/// offering audio it could never send. Both codecs are pure Rust and always
/// available, so the only question is whether there is anything to encode.
pub fn capturable() -> Vec<pravera_core::AudioCodec> {
    if can_capture() {
        vec![
            pravera_core::AudioCodec::Pcm16,
            pravera_core::AudioCodec::Adpcm4,
        ]
    } else {
        Vec::new()
    }
}

/// Whether this build can capture desktop audio at all.
///
/// Asked by the host before it offers audio in the handshake, so a client is
/// never told to expect a stream that cannot exist.
pub const fn can_capture() -> bool {
    cfg!(windows)
}

/// Whether this build can play remote audio at all.
pub const fn can_play() -> bool {
    cfg!(windows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_platform_without_audio_says_so_rather_than_pretending() {
        // The host asks `can_capture` before offering audio in the handshake.
        // If it ever claimed more than the build has, a client would be
        // promised a stream that can never arrive. Nothing is opened here on a
        // platform that has devices: a test suite that grabs the sound card is
        // a test suite that fights whatever else is playing.
        assert_eq!(can_capture(), can_play());
        if !can_capture() {
            assert!(matches!(loopback(), Err(Error::Unsupported(_))));
            let format = AudioFormat::new(pravera_core::AudioCodec::Pcm16);
            assert!(matches!(speaker(format), Err(Error::Unsupported(_))));
        }
    }

    #[test]
    fn an_error_about_the_hardware_never_names_the_hardware() {
        // These strings reach a peer. A device name would describe the host's
        // machine to somebody who has not been granted anything.
        let refusals = [
            Error::Unsupported("not implemented"),
            Error::Packet {
                wanted: 960,
                got: 12,
                what: "bytes",
            },
        ];
        for error in refusals {
            let text = error.to_string();
            assert!(!text.contains('\\'), "{text} looks like a path");
            assert!(!text.contains('/'), "{text} looks like a path");
        }
    }
}
