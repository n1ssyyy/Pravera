//! Packets in, sound out.
//!
//! Two halves that never touch each other directly: a [`AudioSink`] the
//! receive loop drops packets into, and a thread that takes them out in order,
//! decodes them and hands them to the speakers. Between the two sits the
//! jitter buffer, which is where all the interesting decisions are.
//!
//! ## Why this is not fed by its own receive loop
//!
//! There is exactly one `recv_media` on a session, and the video path already
//! owns it. A second reader would not get a copy of each datagram — it would
//! take datagrams *from* the first one, at random. So audio arrives through
//! the same loop that assembles video, routed on [`ChunkFlags::AUDIO`], and
//! this module is handed frames rather than going looking for them.
//!
//! That also settles the lifetimes. A stream is rebuilt every time the host
//! reconfigures; the sound should not stop because somebody switched monitor.
//! [`AudioStream`] therefore outlives the video stream, and hands out a cheap
//! [`AudioSink`] that each new video stream carries.
//!
//! ## What a jitter buffer is actually for
//!
//! Packets leave the host every five milliseconds exactly. They arrive in
//! clumps, out of order, and sometimes not at all. Playing them the instant
//! they land would produce a gap every time the network hiccuped; holding them
//! forever would produce sound that lags the picture by however long the
//! buffer is. The buffer is the smallest amount of delay that absorbs the
//! path's variation — [`PRIME_PACKETS`] of it, 30 ms — and everything below is
//! about keeping it near that figure without ever letting it grow unbounded.
//!
//! A lost packet is *concealed* rather than skipped: five milliseconds of
//! silence goes in its place, so the audio after it stays at the right length.
//! Skipping would shorten the stream by five milliseconds each time and walk
//! the sound steadily ahead of the picture over a lossy session.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};
use pravera_audio::Speaker;
use pravera_core::AudioFormat;
use pravera_proto::frame::Frame;
use tracing::{debug, warn};

/// Packets buffered before playback starts.
///
/// 30 ms. Enough to ride out the reordering and clumping a wireless hop
/// produces, small enough that it is not itself the reason audio trails the
/// picture. On a direct link almost none of it is ever used.
const PRIME_PACKETS: usize = 6;

/// The most packets held before the oldest are thrown away.
///
/// 120 ms. Reaching this means the client is receiving faster than it is
/// playing, which happens when playback stalls; the answer is to skip forward
/// rather than to build a delay that never comes back down.
const MAX_PACKETS: usize = 24;

/// Consecutive concealed packets before the buffer gives up waiting.
///
/// Four, so a burst of loss up to 20 ms is papered over. Past that the packets
/// are not late, they are gone, and continuing to conceal would hold back
/// audio that has already arrived.
const CONCEAL_RUN: u32 = 4;

/// How long the playback thread waits before re-checking the stop flag.
const POLL: Duration = Duration::from_millis(5);

/// What the audio path has seen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioStats {
    pub packets_received: u64,
    /// Packets that were not the length the format calls for, or that the
    /// decoder refused. Either corruption past QUIC's checks, or a peer
    /// sending nonsense.
    pub packets_rejected: u64,
    /// Packets that arrived after their turn had already passed. A steady
    /// count means the buffer is shorter than the path's variation.
    pub packets_late: u64,
    /// Gaps filled with silence because a packet never arrived.
    pub packets_concealed: u64,
    /// Packets thrown away because the buffer was full.
    pub packets_dropped: u64,
    pub frames_played: u64,
    /// How much audio is waiting, in milliseconds. The client's own share of
    /// the delay between the host playing a sound and this machine making it.
    pub buffered_millis: u64,
    /// Times playback ran dry and had to fill the buffer again.
    pub restarts: u64,
}

#[derive(Debug, Default)]
struct Counters {
    packets_received: AtomicU64,
    packets_rejected: AtomicU64,
    packets_late: AtomicU64,
    packets_concealed: AtomicU64,
    packets_dropped: AtomicU64,
    frames_played: AtomicU64,
    restarts: AtomicU64,
}

/// What the playback thread should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Next {
    /// Decode and play this.
    Packet(Vec<u8>),
    /// The packet that belongs here never arrived. Play silence.
    Conceal,
    /// Nothing is waiting. The device is left to run out, which is silence.
    Empty,
}

#[derive(Default)]
struct Buffer {
    /// Packets by the sender's frame number, so reordering sorts itself out.
    packets: BTreeMap<u32, Vec<u8>>,
    /// The frame number playback wants next. `None` until primed.
    next: Option<u32>,
    consecutive_conceals: u32,
    closed: bool,
}

/// The jitter buffer, shared between the receive loop and playback.
struct Jitter {
    inner: Mutex<Buffer>,
    ready: Condvar,
    counters: Counters,
    /// Sample frames in one packet, for turning a packet count into a delay.
    packet_micros: u64,
}

impl Jitter {
    fn push(&self, frame_id: u32, data: Vec<u8>) {
        let mut buffer = self.inner.lock();
        if buffer.closed {
            return;
        }
        self.counters
            .packets_received
            .fetch_add(1, Ordering::Relaxed);

        // A packet whose turn has passed cannot be played: the audio after it
        // has already gone to the device. Counting them is how a person finds
        // out the buffer is shorter than the path needs.
        if buffer.next.is_some_and(|next| frame_id < next) {
            self.counters.packets_late.fetch_add(1, Ordering::Relaxed);
            return;
        }

        buffer.packets.insert(frame_id, data);

        while buffer.packets.len() > MAX_PACKETS {
            // Oldest first. Skipping forward costs one audible jump; letting
            // the buffer grow costs a delay that never comes back down.
            let Some(&oldest) = buffer.packets.keys().next() else {
                break;
            };
            buffer.packets.remove(&oldest);
            self.counters
                .packets_dropped
                .fetch_add(1, Ordering::Relaxed);
            if buffer.next.is_some_and(|next| next <= oldest) {
                buffer.next = Some(oldest + 1);
            }
        }

        self.ready.notify_one();
    }

    /// Take whatever should be played next.
    fn take(&self) -> Next {
        let mut buffer = self.inner.lock();

        let Some(next) = buffer.next else {
            // Not primed. Wait for enough to absorb the path's variation
            // before starting, or there is nothing to absorb it with.
            if buffer.packets.len() < PRIME_PACKETS {
                return Next::Empty;
            }
            let first = *buffer.packets.keys().next().expect("primed");
            buffer.next = Some(first);
            return self.take_from(&mut buffer, first);
        };

        self.take_from(&mut buffer, next)
    }

    fn take_from(&self, buffer: &mut Buffer, next: u32) -> Next {
        if let Some(data) = buffer.packets.remove(&next) {
            buffer.next = Some(next.wrapping_add(1));
            buffer.consecutive_conceals = 0;
            return Next::Packet(data);
        }

        let Some(&waiting) = buffer.packets.keys().next() else {
            // Nothing at all. Un-prime, so playback fills the buffer again
            // rather than starting from a single packet into an empty one.
            buffer.next = None;
            buffer.consecutive_conceals = 0;
            self.counters.restarts.fetch_add(1, Ordering::Relaxed);
            return Next::Empty;
        };

        if buffer.consecutive_conceals >= CONCEAL_RUN {
            // Long enough. Whatever was missing is not late, it is gone.
            buffer.next = Some(waiting.wrapping_add(1));
            buffer.consecutive_conceals = 0;
            let data = buffer.packets.remove(&waiting).expect("just looked");
            return Next::Packet(data);
        }

        buffer.next = Some(next.wrapping_add(1));
        buffer.consecutive_conceals += 1;
        self.counters
            .packets_concealed
            .fetch_add(1, Ordering::Relaxed);
        Next::Conceal
    }

    /// Wait until something is worth taking, or the timeout expires.
    fn wait(&self, timeout: Duration) {
        let mut buffer = self.inner.lock();
        if buffer.packets.is_empty() {
            self.ready.wait_for(&mut buffer, timeout);
        }
    }

    fn close(&self) {
        let mut buffer = self.inner.lock();
        buffer.closed = true;
        buffer.packets.clear();
        self.ready.notify_all();
    }

    fn buffered_millis(&self) -> u64 {
        let packets = self.inner.lock().packets.len() as u64;
        packets * self.packet_micros / 1000
    }

    fn snapshot(&self) -> AudioStats {
        AudioStats {
            packets_received: self.counters.packets_received.load(Ordering::Relaxed),
            packets_rejected: self.counters.packets_rejected.load(Ordering::Relaxed),
            packets_late: self.counters.packets_late.load(Ordering::Relaxed),
            packets_concealed: self.counters.packets_concealed.load(Ordering::Relaxed),
            packets_dropped: self.counters.packets_dropped.load(Ordering::Relaxed),
            frames_played: self.counters.frames_played.load(Ordering::Relaxed),
            buffered_millis: self.buffered_millis(),
            restarts: self.counters.restarts.load(Ordering::Relaxed),
        }
    }
}

/// Where the receive loop leaves audio packets.
///
/// Cheap to clone and safe to hold after the stream has stopped: pushing into
/// a closed buffer does nothing rather than failing.
#[derive(Clone)]
pub struct AudioSink {
    jitter: Arc<Jitter>,
    format: AudioFormat,
}

impl AudioSink {
    /// Hand over one reassembled audio frame.
    ///
    /// Rejects anything that is not the length the agreed format calls for.
    /// That check belongs here rather than in the decoder because it is the
    /// cheapest place to notice, and because a wrong length is the signature
    /// of a peer that is not sending what it said it would.
    pub fn push(&self, frame: Frame) {
        if frame.data.len() != self.format.packet_bytes() {
            self.jitter
                .counters
                .packets_rejected
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.jitter.push(frame.meta.frame_id, frame.data);
    }
}

impl std::fmt::Debug for AudioSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioSink")
            .field("codec", &self.format.codec.name())
            .finish()
    }
}

/// A running playback of one session's audio.
///
/// Dropping it stops the thread and closes the device.
pub struct AudioStream {
    format: AudioFormat,
    jitter: Arc<Jitter>,
    stop: Arc<AtomicBool>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl AudioStream {
    /// Open the speakers and start playing whatever is pushed in.
    ///
    /// Blocks until the device is open, so a machine with no output endpoint
    /// says so here rather than staying silent for no visible reason.
    pub fn start(format: AudioFormat) -> Result<AudioStream, pravera_audio::Error> {
        if !format.is_playable() {
            return Err(pravera_audio::Error::Device(
                "the host named an audio format nobody can play".into(),
            ));
        }

        let jitter = Arc::new(Jitter {
            inner: Mutex::new(Buffer::default()),
            ready: Condvar::new(),
            counters: Counters::default(),
            packet_micros: format.packet_micros() as u64,
        });
        let stop = Arc::new(AtomicBool::new(false));

        let (opened, ready) =
            std::sync::mpsc::sync_channel::<Result<String, pravera_audio::Error>>(1);

        // Opened on the thread that drives it, for the same reason the host
        // does: these are COM objects, and an apartment mismatch fails on
        // somebody else's machine rather than on this one.
        let handle = {
            let (jitter, stop) = (jitter.clone(), stop.clone());
            std::thread::Builder::new()
                .name("pravera-play".into())
                .spawn(move || play(format, jitter, stop, opened))
                .map_err(|error| {
                    pravera_audio::Error::Device(format!("could not start playback: {error}"))
                })?
        };

        match ready.recv() {
            Ok(Ok(device)) => {
                debug!(%device, codec = format.codec.name(), "playing remote audio");
                Ok(AudioStream {
                    format,
                    jitter,
                    stop,
                    handle: Mutex::new(Some(handle)),
                })
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(pravera_audio::Error::Device(
                "the playback thread died on startup".into(),
            )),
        }
    }

    /// A handle the receive loop can push packets into.
    pub fn sink(&self) -> AudioSink {
        AudioSink {
            jitter: self.jitter.clone(),
            format: self.format,
        }
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }

    pub fn stats(&self) -> AudioStats {
        self.jitter.snapshot()
    }

    pub fn is_running(&self) -> bool {
        !self.stop.load(Ordering::Acquire)
            && self
                .handle
                .lock()
                .as_ref()
                .is_some_and(|handle| !handle.is_finished())
    }

    /// Stop playing and close the device. Idempotent.
    ///
    /// Takes `&self` for the same reason [`crate::VideoStream::stop`] does:
    /// the stream is shared between the interface and the task driving the
    /// session, and either may be the one to end it.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.jitter.close();
        if let Some(handle) = self.handle.lock().take() {
            let _ = handle.join();
            debug!(stats = ?self.jitter.snapshot(), "audio playback ended");
        }
    }
}

impl Drop for AudioStream {
    fn drop(&mut self) {
        self.stop();
    }
}

impl std::fmt::Debug for AudioStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioStream")
            .field("codec", &self.format.codec.name())
            .field("running", &self.is_running())
            .field("stats", &self.stats())
            .finish()
    }
}

fn play(
    format: AudioFormat,
    jitter: Arc<Jitter>,
    stop: Arc<AtomicBool>,
    opened: std::sync::mpsc::SyncSender<Result<String, pravera_audio::Error>>,
) {
    let mut speaker = match pravera_audio::speaker(format) {
        Ok(speaker) => speaker,
        Err(error) => {
            let _ = opened.send(Err(error));
            return;
        }
    };
    if opened.send(Ok(speaker.name().to_string())).is_err() {
        return;
    }

    let mut decoder = pravera_audio::decoder(format);
    let mut samples: Vec<i16> = Vec::with_capacity(format.packet_samples());
    let silence = vec![0i16; format.packet_samples()];
    let packet_frames = format.packet_frames as usize;

    while !stop.load(Ordering::Acquire) {
        let space = match speaker.space() {
            Ok(space) => space,
            Err(error) => {
                warn!(%error, "the speakers stopped taking audio");
                break;
            }
        };

        if space < packet_frames {
            // The device is full, which is the healthy state. Sleeping on the
            // device rather than on the buffer is what paces this loop.
            speaker.wait(POLL);
            continue;
        }

        match jitter.take() {
            Next::Packet(data) => {
                if let Err(error) = decoder.decode(&data, &mut samples) {
                    // Length was checked on the way in, so this is a codec
                    // refusing its own format — worth one line, not a session.
                    debug!(%error, "could not decode an audio packet");
                    jitter
                        .counters
                        .packets_rejected
                        .fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                if !write(speaker.as_mut(), &samples, &jitter) {
                    break;
                }
            }
            Next::Conceal => {
                if !write(speaker.as_mut(), &silence, &jitter) {
                    break;
                }
            }
            // Nothing to play. Wait on the buffer rather than the device: it
            // is the buffer that is empty, and the device will happily report
            // space forever while nothing arrives.
            Next::Empty => jitter.wait(POLL),
        }
    }

    stop.store(true, Ordering::Release);
}

/// Hand samples to the device. Returns whether playback should continue.
fn write(speaker: &mut dyn Speaker, samples: &[i16], jitter: &Jitter) -> bool {
    match speaker.write(samples) {
        Ok(frames) => {
            jitter
                .counters
                .frames_played
                .fetch_add(frames as u64, Ordering::Relaxed);
            true
        }
        Err(error) => {
            warn!(%error, "the speakers refused audio");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pravera_core::AudioCodec;

    fn jitter() -> Jitter {
        let format = AudioFormat::new(AudioCodec::Pcm16);
        Jitter {
            inner: Mutex::new(Buffer::default()),
            ready: Condvar::new(),
            counters: Counters::default(),
            packet_micros: format.packet_micros() as u64,
        }
    }

    /// A packet whose contents identify it, so ordering is checkable.
    fn packet(id: u32) -> Vec<u8> {
        vec![id as u8; 4]
    }

    fn prime(jitter: &Jitter, from: u32) {
        for id in from..from + PRIME_PACKETS as u32 {
            jitter.push(id, packet(id));
        }
    }

    #[test]
    fn nothing_plays_until_there_is_enough_to_absorb_the_path() {
        // Starting on the first packet would mean starting with an empty
        // buffer, and the next hiccup would be audible immediately.
        let jitter = jitter();
        for id in 0..PRIME_PACKETS as u32 - 1 {
            jitter.push(id, packet(id));
            assert_eq!(jitter.take(), Next::Empty);
        }

        jitter.push(PRIME_PACKETS as u32 - 1, packet(PRIME_PACKETS as u32 - 1));
        assert_eq!(jitter.take(), Next::Packet(packet(0)));
    }

    #[test]
    fn packets_play_in_the_order_they_were_sent_not_the_order_they_arrived() {
        let jitter = jitter();
        // Deliberately backwards, which is what a reordering path produces.
        for id in (0..PRIME_PACKETS as u32).rev() {
            jitter.push(id, packet(id));
        }
        for id in 0..PRIME_PACKETS as u32 {
            assert_eq!(jitter.take(), Next::Packet(packet(id)));
        }
    }

    #[test]
    fn a_missing_packet_becomes_silence_rather_than_a_shortened_stream() {
        // Skipping would make the audio five milliseconds shorter each time,
        // walking it steadily ahead of the picture over a lossy session.
        let jitter = jitter();
        prime(&jitter, 0);
        jitter.push(PRIME_PACKETS as u32, packet(PRIME_PACKETS as u32));

        assert_eq!(jitter.take(), Next::Packet(packet(0)));
        // Take packet 1 out from under playback.
        jitter.inner.lock().packets.remove(&1);
        assert_eq!(jitter.take(), Next::Conceal);
        assert_eq!(jitter.take(), Next::Packet(packet(2)));
        assert_eq!(jitter.snapshot().packets_concealed, 1);
    }

    #[test]
    fn a_long_gap_is_stepped_over_rather_than_concealed_forever() {
        // Past a few packets the missing audio is gone, not late, and holding
        // back what has already arrived only adds delay.
        let jitter = jitter();
        prime(&jitter, 0);
        assert_eq!(jitter.take(), Next::Packet(packet(0)));

        // Everything from 1 to 200 never arrives; 201 does.
        jitter.inner.lock().packets.clear();
        jitter.push(201, packet(201));

        for _ in 0..CONCEAL_RUN {
            assert_eq!(jitter.take(), Next::Conceal);
        }
        assert_eq!(jitter.take(), Next::Packet(packet(201)));
        assert_eq!(jitter.inner.lock().next, Some(202));
    }

    #[test]
    fn an_empty_buffer_fills_up_again_before_playing() {
        let jitter = jitter();
        prime(&jitter, 0);
        for id in 0..PRIME_PACKETS as u32 {
            assert_eq!(jitter.take(), Next::Packet(packet(id)));
        }

        assert_eq!(jitter.take(), Next::Empty);
        assert_eq!(jitter.snapshot().restarts, 1);

        // One packet is not enough to start again on.
        jitter.push(100, packet(100));
        assert_eq!(jitter.take(), Next::Empty);

        prime(&jitter, 101);
        assert_eq!(jitter.take(), Next::Packet(packet(100)));
    }

    #[test]
    fn a_packet_that_arrives_after_its_turn_is_counted_rather_than_played_late() {
        let jitter = jitter();
        prime(&jitter, 0);
        assert_eq!(jitter.take(), Next::Packet(packet(0)));

        jitter.push(0, packet(0));
        assert_eq!(jitter.snapshot().packets_late, 1);
        assert_eq!(jitter.take(), Next::Packet(packet(1)));
    }

    #[test]
    fn a_buffer_that_keeps_growing_skips_forward_instead() {
        // What a stalled device produces. Letting it grow would build a delay
        // that never comes back down.
        let jitter = jitter();
        for id in 0..(MAX_PACKETS as u32 + 10) {
            jitter.push(id, packet(id));
        }

        let held = jitter.inner.lock().packets.len();
        assert_eq!(held, MAX_PACKETS);
        assert_eq!(jitter.snapshot().packets_dropped, 10);

        // Playback resumes at the oldest packet still held, not at the one
        // that was thrown away.
        assert_eq!(jitter.take(), Next::Packet(packet(10)));
    }

    #[test]
    fn the_reported_delay_is_the_audio_actually_waiting() {
        // Shown to the person as the client's share of the delay, so it has to
        // be the real buffer rather than the figure it aims for.
        let jitter = jitter();
        assert_eq!(jitter.buffered_millis(), 0);
        prime(&jitter, 0);
        assert_eq!(jitter.buffered_millis(), PRIME_PACKETS as u64 * 5);
    }

    #[test]
    fn a_closed_buffer_takes_nothing_more() {
        // The sink outlives the stream by design; pushing into a stopped one
        // must do nothing rather than resurrect it.
        let jitter = jitter();
        jitter.close();
        jitter.push(0, packet(0));
        assert_eq!(jitter.snapshot().packets_received, 0);
        assert_eq!(jitter.take(), Next::Empty);
    }

    #[test]
    fn a_packet_of_the_wrong_size_never_reaches_the_buffer() {
        // The signature of a peer sending something other than what it agreed
        // to. Cheapest to notice here, before a decoder is involved.
        let format = AudioFormat::new(AudioCodec::Pcm16);
        let sink = AudioSink {
            jitter: Arc::new(jitter()),
            format,
        };

        sink.push(Frame {
            meta: pravera_proto::frame::FrameMeta {
                frame_id: 0,
                capture_micros: 0,
                monitor: pravera_proto::MonitorId::PRIMARY,
                flags: pravera_proto::ChunkFlags::AUDIO,
            },
            data: vec![0u8; 7],
        });

        let stats = sink.jitter.snapshot();
        assert_eq!(stats.packets_rejected, 1);
        assert_eq!(stats.packets_received, 0);
    }

    #[test]
    fn a_tone_survives_every_step_between_the_two_machines() {
        // Encode, split into datagrams, reassemble, sink, buffer, decode: the
        // whole path from the host's mixer to the client's speakers with only
        // the two devices left out. Each piece has its own tests; this is the
        // one that would catch them disagreeing about a header or a length.
        let format = AudioFormat::new(AudioCodec::Pcm16);
        let mut encoder = pravera_audio::encoder(format);
        let mut reassembler = pravera_proto::Reassembler::new();
        let sink = AudioSink {
            jitter: Arc::new(jitter()),
            format,
        };

        let packets = PRIME_PACKETS as u32 + 4;
        let mut sent = Vec::new();
        for id in 0..packets {
            let samples = tone(id as usize * format.packet_samples());
            sent.extend_from_slice(&samples);

            let mut packet = Vec::new();
            encoder.encode(&samples, &mut packet).unwrap();
            let meta = pravera_proto::frame::FrameMeta {
                frame_id: id,
                capture_micros: id * format.packet_micros(),
                monitor: pravera_proto::MonitorId::PRIMARY,
                flags: pravera_proto::ChunkFlags::AUDIO,
            };
            for datagram in pravera_proto::frame::split(meta, &packet).unwrap() {
                if let Some(frame) = reassembler.push(&datagram).unwrap() {
                    sink.push(frame);
                }
            }
        }

        let mut decoder = pravera_audio::decoder(format);
        let mut heard = Vec::new();
        let mut out = Vec::new();
        loop {
            match sink.jitter.take() {
                Next::Packet(data) => {
                    decoder.decode(&data, &mut out).unwrap();
                    heard.extend_from_slice(&out);
                }
                Next::Conceal => panic!("nothing was lost, so nothing should be concealed"),
                Next::Empty => break,
            }
        }

        // PCM is lossless, so anything other than the original samples is a
        // fault in the path rather than in the codec.
        assert_eq!(heard, sent);
        let stats = sink.jitter.snapshot();
        assert_eq!(stats.packets_received, u64::from(packets));
        assert_eq!(stats.packets_rejected, 0);
        assert_eq!(stats.packets_dropped, 0);
    }

    /// A quarter-second sine at 440 Hz, sampled from `offset`, in stereo.
    fn tone(offset: usize) -> Vec<i16> {
        let format = AudioFormat::new(AudioCodec::Pcm16);
        (0..format.packet_samples())
            .map(|i| {
                let frame = (offset + i) / 2;
                let phase = frame as f32 * 440.0 * std::f32::consts::TAU / 48_000.0;
                (phase.sin() * 12_000.0) as i16
            })
            .collect()
    }
}
