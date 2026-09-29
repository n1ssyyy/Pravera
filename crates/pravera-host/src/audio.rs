//! Tap the host's output mix, packetise it, put it on the wire.
//!
//! A thread, for the same reason [`crate::media`] uses one: reading a WASAPI
//! buffer and running a codec are both blocking and neither yields anywhere an
//! async runtime could use. It is a much smaller thread than the video one —
//! five milliseconds of stereo is 480 samples, and encoding them costs
//! microseconds — but it wakes two hundred times a second, which is exactly
//! the shape that should not be sharing a runtime worker with anything.
//!
//! ## Why the device is opened on the thread that reads it
//!
//! WASAPI's interfaces are COM objects. An object created in one apartment and
//! used from another needs marshalling, and getting that wrong produces
//! failures that only show up on somebody else's machine. Opening on the
//! thread that will drive it removes the question entirely — at the cost of
//! one channel, so [`AudioStreamer::start`] can still report a device that
//! would not open instead of returning a handle to a thread that already died.
//!
//! ## What this does not do
//!
//! It does not synchronise with the picture. Video timestamps count from when
//! *capture* started, deep inside `pravera-capture`; audio counts from when
//! *this* thread started, and the two origins differ by however long the
//! respective devices took to open. Aligning them means giving both streams
//! one session clock and giving the client a presentation clock to schedule
//! against — which is the frame-pacing work in P7, and is where it belongs.
//!
//! Until then both streams are presented as soon as they arrive. In practice
//! audio lands a few tens of milliseconds behind the picture, which is the
//! direction people tolerate; what matters is that nothing here claims a
//! measurement it did not take.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use bytes::Bytes;
use pravera_audio::Converter;
use pravera_core::audio::AudioFormat;
use pravera_core::{Error, Result};
use pravera_proto::frame::{split, ChunkFlags, FrameMeta};
use pravera_proto::MonitorId;
use pravera_transport::Session;
use tracing::{debug, info, warn};

/// How long the loop waits on the device before looking at the stop flag.
///
/// A quarter of a packet. Short enough that ending a session is immediate,
/// long enough that an idle host costs a few hundred cheap wakeups a second.
const POLL: Duration = Duration::from_micros(1_250);

/// Consecutive send failures tolerated before the stream gives up.
///
/// Same reasoning as the video stream: one is a full send buffer, a hundred in
/// a row is a connection that has gone without saying so.
const SEND_FAILURE_LIMIT: u64 = 200;

/// The most audio held before old packets are thrown away.
///
/// Reached only if the encoder or the network stalls for a quarter of a
/// second, at which point the oldest audio is worthless: nobody wants to hear
/// what the host played 250 ms ago, they want to hear what it is playing now.
const BACKLOG_MILLIS: u64 = 250;

/// What the audio stream has done since it started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioStats {
    pub packets_sent: u64,
    pub bytes_sent: u64,
    /// Datagrams the transport would not take. A few is congestion; a steady
    /// climb is a path that cannot carry the stream.
    pub send_failures: u64,
    /// Sample frames read off the mixer. Flat means the host is silent, which
    /// is ordinary — WASAPI delivers nothing at all while nothing is playing.
    pub frames_captured: u64,
    /// Sample frames thrown away because the pipeline fell behind.
    pub frames_dropped: u64,
}

#[derive(Debug, Default)]
struct Counters {
    packets_sent: AtomicU64,
    bytes_sent: AtomicU64,
    send_failures: AtomicU64,
    frames_captured: AtomicU64,
    frames_dropped: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> AudioStats {
        AudioStats {
            packets_sent: self.packets_sent.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            send_failures: self.send_failures.load(Ordering::Relaxed),
            frames_captured: self.frames_captured.load(Ordering::Relaxed),
            frames_dropped: self.frames_dropped.load(Ordering::Relaxed),
        }
    }
}

/// A running audio stream. Dropping it stops the tap.
pub struct AudioStreamer {
    format: AudioFormat,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
    handle: Option<thread::JoinHandle<()>>,
}

impl AudioStreamer {
    /// Start sending this machine's output mix.
    ///
    /// Blocks until the device has been opened, so a machine with no audio
    /// endpoint fails here rather than going quiet for no visible reason.
    pub fn start(session: Session, format: AudioFormat) -> Result<AudioStreamer> {
        if !format.is_playable() {
            return Err(Error::Config("the audio format makes no sense".into()));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(Counters::default());
        let (opened, ready) = mpsc::sync_channel::<Result<String>>(1);

        let handle = {
            let (stop, counters) = (stop.clone(), counters.clone());
            thread::Builder::new()
                .name("pravera-audio".into())
                .spawn(move || run(session, format, stop, counters, opened))
                .map_err(|error| Error::Config(format!("could not start audio: {error}")))?
        };

        // The thread sends exactly one message before it starts looping, so a
        // disconnect here means it panicked on the way up.
        match ready.recv() {
            Ok(Ok(device)) => {
                info!(
                    device = %device,
                    codec = format.codec.name(),
                    bitrate = format.bitrate(),
                    "streaming audio"
                );
                Ok(AudioStreamer {
                    format,
                    stop,
                    counters,
                    handle: Some(handle),
                })
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(Error::Config("the audio thread died on startup".into())),
        }
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }

    pub fn stats(&self) -> AudioStats {
        self.counters.snapshot()
    }

    /// Whether the thread is still going. It ends on its own if the device
    /// disappears or the connection dies.
    pub fn is_running(&self) -> bool {
        self.handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
    }

    /// Stop and wait for the thread. Idempotent.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
            debug!(stats = ?self.counters.snapshot(), "audio ended");
        }
    }
}

impl Drop for AudioStreamer {
    fn drop(&mut self) {
        self.stop();
    }
}

impl std::fmt::Debug for AudioStreamer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioStreamer")
            .field("codec", &self.format.codec.name())
            .field("running", &self.is_running())
            .field("stats", &self.stats())
            .finish()
    }
}

fn run(
    session: Session,
    format: AudioFormat,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
    opened: mpsc::SyncSender<Result<String>>,
) {
    let mut tap = match pravera_audio::loopback() {
        Ok(tap) => tap,
        Err(error) => {
            let _ = opened.send(Err(Error::Config(format!(
                "this machine's audio could not be captured: {error}"
            ))));
            return;
        }
    };

    let device = tap.format();
    debug!(%device, "audio device opened");
    if opened.send(Ok(device.to_string())).is_err() {
        // Nobody is waiting any more, which means `start` gave up.
        return;
    }

    let mut converter = Converter::new(device, format.sample_rate);
    let mut encoder = pravera_audio::encoder(format);

    let wanted = format.packet_samples();
    let backlog = (format.sample_rate as u64 * BACKLOG_MILLIS / 1000) as usize
        * format.channels.max(1) as usize;

    let mut raw: Vec<u8> = Vec::new();
    let mut samples: Vec<i16> = Vec::new();
    let mut packet: Vec<u8> = Vec::new();
    let mut frame_id: u32 = 0;
    let mut consecutive_failures: u64 = 0;
    let started = Instant::now();
    // Same heartbeat as the video loop: one info line every 30s so a silent
    // audio pipeline (headless WASAPI capturing nothing) is distinguishable
    // from a dead thread. Audio is quieter than video — 30s, not 10s.
    let mut last_heartbeat = Instant::now();

    while !stop.load(Ordering::Acquire) {
        if last_heartbeat.elapsed() >= Duration::from_secs(30) {
            last_heartbeat = Instant::now();
            tracing::info!(
                packets_sent = counters.packets_sent.load(Ordering::Relaxed),
                send_failures = counters.send_failures.load(Ordering::Relaxed),
                frames_captured = counters.frames_captured.load(Ordering::Relaxed),
                frames_dropped = counters.frames_dropped.load(Ordering::Relaxed),
                "audio heartbeat"
            );
        }
        tap.wait(POLL);

        raw.clear();
        match tap.read(&mut raw) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(error) => {
                warn!(%error, "the audio tap failed");
                break;
            }
        }

        let before = samples.len();
        converter.push(&raw, &mut samples);
        counters.frames_captured.fetch_add(
            ((samples.len() - before) / format.channels.max(1) as usize) as u64,
            Ordering::Relaxed,
        );

        // Old audio is worse than no audio. If the pipeline has fallen far
        // enough behind that the backlog is stale, throw the stale part away
        // rather than sending the host's past at the client's present.
        if samples.len() > backlog {
            let stale = samples.len() - backlog;
            // Whole packets only, so the encoder never sees a partial one.
            let stale = stale - stale % wanted.max(1);
            if stale > 0 {
                samples.drain(..stale);
                counters.frames_dropped.fetch_add(
                    (stale / format.channels.max(1) as usize) as u64,
                    Ordering::Relaxed,
                );
                debug!(frames = stale, "dropped stale audio");
            }
        }

        while samples.len() >= wanted {
            if let Err(error) = encoder.encode(&samples[..wanted], &mut packet) {
                // Only reachable if the packet length and the format disagree,
                // which is a bug here rather than anything the peer did.
                warn!(%error, "could not encode an audio packet");
                return;
            }
            samples.drain(..wanted);

            let meta = FrameMeta {
                frame_id,
                capture_micros: started.elapsed().as_micros() as u32,
                // Audio belongs to the session, not to a display. The field is
                // in the header for video's sake; naming the primary keeps it
                // meaningful rather than arbitrary.
                monitor: MonitorId::PRIMARY,
                flags: ChunkFlags::AUDIO,
            };
            frame_id = frame_id.wrapping_add(1);

            let chunks = match split(meta, &packet) {
                Ok(chunks) => chunks,
                Err(error) => {
                    // A packet that will not fit a datagram means the format
                    // and the wire budget have drifted apart. Every packet
                    // after this one would fail the same way.
                    warn!(%error, bytes = packet.len(), "an audio packet will not fit the wire");
                    return;
                }
            };

            if !send(&session, chunks, &counters, &mut consecutive_failures) {
                return;
            }
        }
    }
}

/// Put one packet's chunks on the wire. Returns whether to keep going.
fn send(
    session: &Session,
    chunks: Vec<Vec<u8>>,
    counters: &Counters,
    consecutive_failures: &mut u64,
) -> bool {
    for chunk in chunks {
        let size = chunk.len();
        match session.send_media(Bytes::from(chunk)) {
            Ok(()) => {
                *consecutive_failures = 0;
                counters.packets_sent.fetch_add(1, Ordering::Relaxed);
                counters
                    .bytes_sent
                    .fetch_add(size as u64, Ordering::Relaxed);
            }
            Err(error) => {
                *consecutive_failures += 1;
                counters.send_failures.fetch_add(1, Ordering::Relaxed);
                if *consecutive_failures >= SEND_FAILURE_LIMIT {
                    warn!(%error, "giving up on a connection that will not take audio");
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use pravera_core::AudioCodec;

    #[test]
    fn a_format_nobody_could_play_is_refused_before_a_thread_is_spawned() {
        // `start` takes a format that arrived through the state machine. A
        // packet of zero frames would spin the loop forever on a `while
        // samples.len() >= 0`.
        let mut format = AudioFormat::new(AudioCodec::Pcm16);
        format.packet_frames = 0;
        assert!(!format.is_playable());
    }

    #[test]
    fn the_backlog_is_a_quarter_second_of_whatever_the_format_is() {
        // The figure that decides when old audio is thrown away. If it were
        // computed from a constant rather than from the format, a session at
        // any other rate would either drop constantly or never drop at all.
        let format = AudioFormat::new(AudioCodec::Pcm16);
        let backlog =
            (format.sample_rate as u64 * BACKLOG_MILLIS / 1000) as usize * format.channels as usize;
        assert_eq!(backlog, 48_000 / 4 * 2);
        assert!(backlog > format.packet_samples() * 10);
    }

    #[test]
    fn stats_start_at_nothing_rather_than_at_something_invented() {
        assert_eq!(Counters::default().snapshot(), AudioStats::default());
    }
}
