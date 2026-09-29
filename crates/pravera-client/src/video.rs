//! Datagrams in, pictures out.
//!
//! Three stages, deliberately separated because they have different needs:
//!
//! 1. an async task that reads datagrams and reassembles them — cheap, but has
//!    to be on the runtime because that is where the socket is,
//! 2. a thread that decodes — several milliseconds of solid CPU per frame,
//!    which would stall every other task if it ran on a runtime worker,
//! 3. a mailbox holding exactly one decoded frame, which the UI reads whenever
//!    it happens to be drawing.
//!
//! ## The two seams are not the same, and treating them alike breaks the picture
//!
//! The seam between decode and the window is a mailbox: one slot, newest wins.
//! A queue there would turn a throughput problem into a latency problem — if
//! drawing falls behind, every later picture waits behind the backlog and the
//! view drifts further into the past for as long as the session lasts.
//! Overwriting means the viewer sees the newest picture the machine produced,
//! and falling behind costs frame rate rather than latency. Nobody notices a
//! dropped frame; everybody notices a cursor that lags.
//!
//! The seam between the network and decode looks identical and behaves in the
//! opposite way, because **encoded frames are not interchangeable**. A decoded
//! picture is a complete image and a newer one supersedes it. An encoded
//! predicted frame is a description of the difference from the frame before it,
//! so discarding one does not cost that frame — it costs every frame after it,
//! until the next keyframe. Overwriting there buys a few milliseconds and pays
//! seconds of frozen or smeared video, which is why that seam is a short queue
//! ([`FrameQueue`]) and not a mailbox.
//!
//! The distinction only shows up once the host is fast. While the encoder was
//! the bottleneck the decoder was never behind, the slot was always empty, and
//! nothing was ever overwritten.
//!
//! The host makes the mailbox choice on the capture side, where it is correct
//! for the same reason it is correct at the drawing end: a captured frame
//! nobody has encoded yet is a complete picture, and a newer one supersedes it.
//!
//! ## Losing chunks is normal
//!
//! Media rides unreliable datagrams, so a lost packet is not an error — it is
//! the design. A frame missing a chunk is discarded and the decoder is left
//! with a gap, which for H.264 means visible corruption until the next
//! keyframe. [`VideoStream::wants_keyframe`] is how the client notices and asks
//! for one; the alternative is a picture that stays broken.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use pravera_codec::{DecodedFrame, EncodedFrame, VideoDecoder};
use pravera_core::FrameFormat;
use pravera_proto::frame::{ChunkFlags, ChunkHeader, Frame, Reassembler};
use pravera_transport::Session;
use tracing::{debug, warn};

/// How long a reader waits before checking whether the stream is still alive.
const POLL: Duration = Duration::from_millis(50);

/// Frames that may be dropped for lost chunks before asking for a keyframe.
///
/// One incomplete frame is a single lost packet, and the picture recovers on
/// its own within a frame or two. A run of them means the decoder is working
/// from a broken reference and everything it produces from here is wrong, so
/// the cost of a keyframe is worth paying.
const LOSS_BEFORE_KEYFRAME: u64 = 2;

/// Frames the decoder may be handed without producing a picture before a
/// keyframe is worth asking for.
///
/// Counting evicted frames is not enough on its own. The reassembler only
/// learns a frame was incomplete when a *later* frame displaces it, so if the
/// chunk that went missing belonged to the only frame the host has sent — which
/// is the normal state of a machine nobody is touching — nothing displaces it
/// and nobody ever asks. The decoder is the end of the line and knows the thing
/// that matters: it is being given frames and cannot make a picture from any of
/// them.
///
/// Two rather than one because a stream can legitimately open with a frame the
/// decoder cannot use — a keyframe still being reassembled while a later frame
/// completes first — and one spurious keyframe request per session is a waste
/// worth avoiding.
const UNUSABLE_BEFORE_KEYFRAME: u64 = 2;

/// Frames dropped in a row while resynchronising before asking again.
///
/// Higher than the loss thresholds because this is a backstop rather than the
/// mechanism: in a healthy stream the keyframe already asked for arrives well
/// inside this, and asking twice for the same one costs a full frame of
/// bandwidth. Low enough that a request which genuinely went missing costs a
/// moment rather than the whole session.
const SKIPPED_BEFORE_KEYFRAME: u64 = 60;

/// Minimum gap between keyframe requests.
///
/// The loss counters above refire indefinitely against a host that never sends
/// a recognizable keyframe (refused, ignored, or unmarked) — at 60fps that was
/// a control message every frame for the life of the session. The flag already
/// coalesces bursts; this bounds the sustained rate. Generous on purpose: a
/// keyframe is large, and asking twice for the same one costs bandwidth while
/// a genuinely missing one is re-asked half a second later.
const KEYFRAME_REQUEST_EVERY: Duration = Duration::from_millis(500);

/// Set the keyframe flag unless one was set recently. Returns whether this
/// call actually asked. Both pipeline stages share this so neither can spin
/// the control stream on its own.
fn ask_keyframe(flag: &AtomicBool, last: &mut Instant) -> bool {
    if last.elapsed() < KEYFRAME_REQUEST_EVERY {
        return false;
    }
    *last = Instant::now();
    flag.store(true, Ordering::Release);
    true
}

/// What the receive path has seen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VideoStats {
    pub datagrams_received: u64,
    /// Datagrams that were not a valid chunk. Either corruption that got past
    /// QUIC's own checks, or a peer sending nonsense.
    pub datagrams_rejected: u64,
    pub frames_assembled: u64,
    /// Frames abandoned because a chunk never arrived.
    pub frames_incomplete: u64,
    pub frames_decoded: u64,
    /// Frames the decoder was given but produced no picture from — ordinary
    /// while it waits for a keyframe to start from.
    pub frames_pending: u64,
    pub frames_failed: u64,
    /// Decoded frames overwritten before the UI read them. Not a fault: it
    /// means frames arrived faster than the window redrew.
    pub frames_superseded: u64,
    /// Times the decoder fell far enough behind that the queue was emptied and
    /// the stream restarted from a keyframe.
    ///
    /// Each one is a visible skip in the picture. A climbing count means this
    /// machine cannot decode as fast as the host encodes, which is a reason to
    /// ask for a smaller picture rather than something that will settle.
    pub decoder_overruns: u64,
    /// Assembled frames that carried sound rather than picture.
    ///
    /// Counted apart from the rest because the two travel the same path and
    /// `frames_assembled` counts both. A session where every assembled frame
    /// is audio looks identical, by that count alone, to one where video is
    /// arriving and failing — and they are opposite problems.
    pub frames_audio: u64,
    /// Frames handed to the decode queue, whether or not it kept them.
    pub frames_offered: u64,
    /// What the decoder said the first time it refused a frame, if it has.
    ///
    /// Carried all the way to the screen rather than only to the log. A person
    /// looking at a black window is the one who needs this sentence, and they
    /// are the least likely of anybody to go looking for a log file.
    pub decode_error: Option<String>,
}

#[derive(Default)]
struct Counters {
    /// The first refusal, kept verbatim. First rather than latest: once a
    /// decoder is lost every later message is a consequence of this one.
    decode_error: parking_lot::Mutex<Option<String>>,
    frames_audio: AtomicU64,
    frames_offered: AtomicU64,
    datagrams_received: AtomicU64,
    datagrams_rejected: AtomicU64,
    frames_assembled: AtomicU64,
    frames_incomplete: AtomicU64,
    frames_decoded: AtomicU64,
    frames_pending: AtomicU64,
    frames_failed: AtomicU64,
    frames_superseded: AtomicU64,
    decoder_overruns: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> VideoStats {
        VideoStats {
            datagrams_received: self.datagrams_received.load(Ordering::Relaxed),
            datagrams_rejected: self.datagrams_rejected.load(Ordering::Relaxed),
            frames_assembled: self.frames_assembled.load(Ordering::Relaxed),
            frames_incomplete: self.frames_incomplete.load(Ordering::Relaxed),
            frames_decoded: self.frames_decoded.load(Ordering::Relaxed),
            frames_pending: self.frames_pending.load(Ordering::Relaxed),
            frames_failed: self.frames_failed.load(Ordering::Relaxed),
            frames_superseded: self.frames_superseded.load(Ordering::Relaxed),
            decoder_overruns: self.decoder_overruns.load(Ordering::Relaxed),
            frames_audio: self.frames_audio.load(Ordering::Relaxed),
            frames_offered: self.frames_offered.load(Ordering::Relaxed),
            decode_error: self.decode_error.lock().clone(),
        }
    }
}

/// How many encoded frames may wait for the decoder.
///
/// This queue exists because encoded frames are **not** interchangeable the way
/// decoded ones are. A decoded picture that nobody drew is worth nothing once a
/// newer one exists, so the display side keeps only the latest. An encoded
/// frame is the opposite: every predicted frame is described relative to the
/// one before it, so dropping a single frame invalidates every frame after it
/// until the next keyframe — which at a five-second keyframe interval is five
/// seconds of a frozen or smeared picture bought to save one frame of delay.
///
/// Six frames is a tenth of a second at sixty. Deep enough to ride out a
/// decoder that stalls briefly, shallow enough that the delay it can add is
/// smaller than the delay of losing the chain.
const DECODE_QUEUE: usize = 6;

/// A short queue of encoded frames waiting for the decoder.
///
/// Overflow is handled by cutting cleanly rather than by dropping one frame:
/// once the queue is full the decoder is behind for a reason that will not fix
/// itself in one frame, and every frame kept from before the cut is one the
/// decoder cannot use anyway. So the queue is emptied, a keyframe is asked for,
/// and what arrives in between is discarded unread — one visible skip instead
/// of seconds of failing frames.
struct FrameQueue {
    inner: Mutex<Queued>,
    ready: Condvar,
}

#[derive(Default)]
struct Queued {
    frames: std::collections::VecDeque<EncodedFrame>,
    /// Set after an overflow. Everything before the next keyframe is
    /// undecodable, so it is not worth queueing.
    resyncing: bool,
}

/// What happened to a frame that was offered to the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Queueing {
    /// Queued for the decoder.
    Accepted,
    /// Dropped while waiting for a keyframe to start again from.
    Skipped,
    /// The queue was full. It has been emptied and a keyframe is needed.
    Overflowed,
}

impl FrameQueue {
    fn new() -> FrameQueue {
        FrameQueue {
            inner: Mutex::new(Queued::default()),
            ready: Condvar::new(),
        }
    }

    /// Stop discarding frames and let the decoder see them, keyframe or not.
    ///
    /// The escape hatch from a wait that cannot end. Skipping non-keyframes is
    /// right while a keyframe is genuinely on its way, but it assumes the
    /// sender marks one eventually — and an encoder that never sets the flag
    /// turns that assumption into a stall no amount of asking will clear.
    ///
    /// Handing unsynchronised frames to the decoder is safe: it returns nothing
    /// until it finds a point it can start from, which costs a little work and
    /// no correctness. Refusing to hand them over costs the entire session.
    fn stop_waiting(&self) {
        self.inner.lock().resyncing = false;
    }

    fn push(&self, frame: EncodedFrame) -> Queueing {
        let mut queued = self.inner.lock();

        if frame.keyframe {
            // A keyframe stands on its own, so whatever was missed before it
            // stops mattering the moment it arrives. That is also why a full
            // queue never turns one away: it is the one frame worth more than
            // everything already in there put together, and refusing it would
            // leave the stream waiting for the next one.
            queued.resyncing = false;
            if queued.frames.len() >= DECODE_QUEUE {
                // Only when behind. In a healthy stream the frames already
                // queued are older than this one and perfectly decodable, and
                // throwing them away would put a visible skip in the picture
                // once per keyframe interval for no reason at all.
                queued.frames.clear();
            }
            queued.frames.push_back(frame);
            self.ready.notify_one();
            return Queueing::Accepted;
        }

        if queued.resyncing {
            return Queueing::Skipped;
        }

        if queued.frames.len() >= DECODE_QUEUE {
            queued.frames.clear();
            queued.resyncing = true;
            return Queueing::Overflowed;
        }

        queued.frames.push_back(frame);
        self.ready.notify_one();
        Queueing::Accepted
    }

    /// Wait for a frame, giving up after `timeout` so the caller can check
    /// whether the stream is still running.
    fn take_timeout(&self, timeout: Duration) -> Option<EncodedFrame> {
        let mut queued = self.inner.lock();
        if queued.frames.is_empty() {
            self.ready.wait_for(&mut queued, timeout);
        }
        queued.frames.pop_front()
    }
}

/// A slot holding at most one item, where a new one replaces an unread one.
struct Mailbox<T> {
    slot: Mutex<Option<T>>,
    ready: Condvar,
}

impl<T> Mailbox<T> {
    fn new() -> Mailbox<T> {
        Mailbox {
            slot: Mutex::new(None),
            ready: Condvar::new(),
        }
    }

    /// Leave an item, returning whether one was displaced.
    fn put(&self, item: T) -> bool {
        let displaced = self.slot.lock().replace(item).is_some();
        self.ready.notify_one();
        displaced
    }

    fn take(&self) -> Option<T> {
        self.slot.lock().take()
    }

    /// Wait for an item, giving up after `timeout` so the caller can check
    /// whether the stream is still running.
    fn take_timeout(&self, timeout: Duration) -> Option<T> {
        let mut slot = self.slot.lock();
        if slot.is_none() {
            self.ready.wait_for(&mut slot, timeout);
        }
        slot.take()
    }
}

/// A running decode of one session's video.
///
/// Dropping it stops both stages.
///
/// [`VideoStream::stop`] takes `&self` rather than `&mut self`, and the handles
/// live behind a lock to allow it. That is not tidiness: the stream is shared
/// between the interface and the task driving the session, and when the host
/// reconfigures, the old decoder has to stop *before* the new one starts.
/// Two streams on one connection both call `recv_media`, and they would take
/// datagrams from each other for as long as the overlap lasted.
pub struct VideoStream {
    format: FrameFormat,
    frames: Arc<Mailbox<DecodedFrame>>,
    counters: Arc<Counters>,
    stop: Arc<AtomicBool>,
    wants_keyframe: Arc<AtomicBool>,
    threads: Mutex<Option<Threads>>,
}

struct Threads {
    receiver: tokio::task::JoinHandle<()>,
    decoder: std::thread::JoinHandle<()>,
}

impl std::fmt::Debug for VideoStream {
    /// Prints what the stream is doing rather than its machinery.
    ///
    /// Manual because a `Condvar` and a pair of join handles have no useful
    /// `Debug`, and this type ends up inside interface messages that are
    /// formatted whole.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoStream")
            .field("format", &self.format)
            .field("running", &self.is_running())
            .field("stats", &self.stats())
            .finish()
    }
}

impl VideoStream {
    /// Start receiving and decoding what the host agreed to send.
    ///
    /// Must be called from inside a tokio runtime — the receive half is a task
    /// on it.
    pub fn start(
        session: Session,
        format: FrameFormat,
        audio: Option<crate::AudioSink>,
    ) -> crate::Result<VideoStream> {
        let decoder = pravera_codec::decoder(format.codec)?;

        let counters = Arc::new(Counters::default());
        let stop = Arc::new(AtomicBool::new(false));
        let wants_keyframe = Arc::new(AtomicBool::new(false));
        let frames = Arc::new(Mailbox::new());
        // The seam between the two stages. A queue rather than a mailbox: an
        // encoded frame is not interchangeable with a newer one the way a
        // decoded picture is, because every predicted frame is described
        // relative to the frame before it. Replacing an unread one here would
        // save a few milliseconds of delay and cost every frame until the next
        // keyframe.
        let encoded = Arc::new(FrameQueue::new());

        let receiver = tokio::spawn(receive(
            session,
            format,
            audio,
            encoded.clone(),
            counters.clone(),
            stop.clone(),
            wants_keyframe.clone(),
        ));

        let decoder = {
            let (encoded, frames, counters, stop, wants) = (
                encoded,
                frames.clone(),
                counters.clone(),
                stop.clone(),
                wants_keyframe.clone(),
            );
            std::thread::Builder::new()
                .name("pravera-decode".into())
                .spawn(move || decode(decoder, encoded, frames, counters, stop, wants))
                .map_err(|error| pravera_codec::CodecError::Init {
                    codec: format.codec.name(),
                    reason: format!("could not start the decode thread: {error}"),
                })?
        };

        Ok(VideoStream {
            format,
            frames,
            counters,
            stop,
            wants_keyframe,
            threads: Mutex::new(Some(Threads { receiver, decoder })),
        })
    }

    /// What the host said it is sending.
    pub fn format(&self) -> FrameFormat {
        self.format
    }

    /// The newest decoded frame, or `None` if none has arrived since the last
    /// call. Never blocks — this is what a render loop calls.
    pub fn latest(&self) -> Option<DecodedFrame> {
        self.frames.take()
    }

    /// The newest decoded frame, waiting up to `timeout` for one.
    pub fn wait(&self, timeout: Duration) -> Option<DecodedFrame> {
        self.frames.take_timeout(timeout)
    }

    /// Whether enough has been lost that a keyframe is worth asking for.
    ///
    /// Clears the flag, so a caller that acts on it does not ask again for the
    /// same loss. The request itself goes on the control stream, which this
    /// stage has no access to on purpose — deciding and asking are different
    /// jobs, and only one of them belongs next to a decoder.
    pub fn wants_keyframe(&self) -> bool {
        self.wants_keyframe.swap(false, Ordering::AcqRel)
    }

    pub fn stats(&self) -> VideoStats {
        self.counters.snapshot()
    }

    /// Whether both stages are still going.
    pub fn is_running(&self) -> bool {
        !self.stop.load(Ordering::Acquire)
            && self
                .threads
                .lock()
                .as_ref()
                .is_some_and(|t| !t.decoder.is_finished())
    }

    /// Stop both stages and wait for them. Idempotent, and callable through an
    /// `Arc` — see the note on the type.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        let Some(threads) = self.threads.lock().take() else {
            return;
        };
        threads.receiver.abort();
        // Bounded by POLL: the decode thread checks the stop flag every time
        // its wait times out.
        let _ = threads.decoder.join();
        debug!(stats = ?self.counters.snapshot(), "video stream ended");
    }
}

impl Drop for VideoStream {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One reassembler per media stream, chosen by the chunk's own flag.
///
/// Frame IDs are unique only within a stream, and the host numbers video and
/// audio independently — both start from zero every session. Sharing a
/// reassembler makes the two streams fight over the ID space: an audio packet
/// is one chunk and completes the moment it lands, so the audio stream's
/// newest-completed ID races ahead of the video's. Every video frame numbered
/// at or below it then looks like ancient history and is discarded on
/// arrival, and a video partial that does get opened is poisoned by the next
/// audio chunk, whose header describes a different frame. The session that
/// results carries sound, counts packets, and never assembles one picture.
struct Demuxer {
    video: Reassembler,
    audio: Reassembler,
}

impl Default for Demuxer {
    fn default() -> Self {
        Self::new()
    }
}

impl Demuxer {
    fn new() -> Demuxer {
        Demuxer {
            video: Reassembler::new(),
            audio: Reassembler::new(),
        }
    }

    /// Feed one datagram to the stream it claims to belong to.
    ///
    /// The flag sits in the header, so the route is decided before anything is
    /// reassembled. The reassembler parses the same header again; fourteen
    /// bytes read twice is the price of this type staying the only place that
    /// knows the routing rule.
    fn push(&mut self, datagram: &[u8]) -> pravera_proto::Result<Option<Frame>> {
        let audio = ChunkHeader::parse(datagram)?.flags.contains(ChunkFlags::AUDIO);
        let reassembler = if audio {
            &mut self.audio
        } else {
            &mut self.video
        };
        reassembler.push(datagram)
    }

    /// Video frames abandoned because a chunk of theirs never arrived.
    ///
    /// Audio's losses are deliberately not counted. A missing sound packet
    /// costs its own few milliseconds and nothing after it — there is no
    /// reference chain to break — so it must never be the reason a picture is
    /// thrown away and a keyframe requested.
    fn dropped_video(&self) -> u64 {
        self.video.dropped_incomplete()
    }
}

/// Read datagrams, reassemble frames, hand them on.
async fn receive(
    session: Session,
    format: FrameFormat,
    audio: Option<crate::AudioSink>,
    encoded: Arc<FrameQueue>,
    counters: Arc<Counters>,
    stop: Arc<AtomicBool>,
    wants_keyframe: Arc<AtomicBool>,
) {
    let mut demuxer = Demuxer::new();
    let mut lost_since_request = 0u64;
    // Consecutive frames dropped while waiting to resynchronise. See
    // `SKIPPED_BEFORE_KEYFRAME`.
    let mut skipped_in_a_row = 0u64;
    // Throttles `ask_keyframe` below; starts long ago so the first loss asks
    // immediately.
    let mut last_request = Instant::now() - KEYFRAME_REQUEST_EVERY * 2;

    while !stop.load(Ordering::Acquire) {
        let datagram = match session.recv_media().await {
            Ok(datagram) => datagram,
            Err(error) => {
                // `warn`, not `debug`: this is the connection dying under a
                // live control channel, and at `debug` the session just freezes
                // with the control stream still answering.
                warn!(
                    %error,
                    received = counters.datagrams_received.load(Ordering::Relaxed),
                    "media stopped arriving"
                );
                break;
            }
        };
        counters.datagrams_received.fetch_add(1, Ordering::Relaxed);

        let assembled = match demuxer.push(&datagram) {
            Ok(assembled) => assembled,
            Err(error) => {
                // A malformed datagram is one datagram. Everything here is
                // attacker-controlled, and the reassembler is where the bounds
                // checking lives, so the right response is to drop it and
                // carry on rather than to end a working session.
                counters.datagrams_rejected.fetch_add(1, Ordering::Relaxed);
                debug!(%error, "discarded a malformed datagram");
                continue;
            }
        };

        // Read after every push: the count rises when an older frame is
        // evicted to make room, which is exactly when a chunk went missing.
        let incomplete = demuxer.dropped_video();
        let previously = counters
            .frames_incomplete
            .swap(incomplete, Ordering::Relaxed);
        if incomplete > previously {
            lost_since_request += incomplete - previously;
            if lost_since_request >= LOSS_BEFORE_KEYFRAME
                && ask_keyframe(&wants_keyframe, &mut last_request)
            {
                lost_since_request = 0;
            }
        }

        let Some(frame) = assembled else {
            continue;
        };
        counters.frames_assembled.fetch_add(1, Ordering::Relaxed);

        if frame.is_audio() {
            counters.frames_audio.fetch_add(1, Ordering::Relaxed);
            // The same datagram path carries both, and this loop is the only
            // reader of it — see the note at the top of `crate::audio`. A
            // session with no audio drops these rather than decoding a sound
            // it was never told to expect.
            if let Some(audio) = &audio {
                audio.push(frame);
            }
            continue;
        }

        // A keyframe resets the reference the decoder works from, so whatever
        // was lost before it no longer matters.
        if frame.is_keyframe() {
            lost_since_request = 0;
        }

        counters.frames_offered.fetch_add(1, Ordering::Relaxed);
        match encoded.push(to_encoded(frame, format)) {
            Queueing::Accepted => skipped_in_a_row = 0,
            // Deliberately not counted as loss. These are frames the decoder
            // could not have used: they were sent before the keyframe that
            // will start the picture again.
            //
            // Counted all the same, because "waiting for a keyframe" and
            // "waiting for a keyframe that is never coming" look identical
            // from here and only one of them ends. A request that went
            // unanswered — lost on the way, or answered with a frame the host
            // did not mark — leaves this branch running forever while frames
            // pour in and the window stays black.
            Queueing::Skipped => {
                skipped_in_a_row += 1;
                if skipped_in_a_row >= SKIPPED_BEFORE_KEYFRAME {
                    skipped_in_a_row = 0;
                    lost_since_request = 0;
                    ask_keyframe(&wants_keyframe, &mut last_request);
                    // Asked again, and stopped waiting for the answer. If the
                    // host is marking keyframes at all, the request is what
                    // fixes this and the decoder throws away the few frames it
                    // cannot use. If it is not, the request will never be
                    // answered in a way this end can recognise, and letting the
                    // frames through is the only thing that ever puts a picture
                    // on the screen.
                    encoded.stop_waiting();
                    debug!(
                        "no frame has been marked as one the picture can start from; \
                         asking again and letting the decoder try regardless"
                    );
                }
            }
            Queueing::Overflowed => {
                counters.decoder_overruns.fetch_add(1, Ordering::Relaxed);
                lost_since_request = 0;
                ask_keyframe(&wants_keyframe, &mut last_request);
                debug!("the decoder fell behind; restarting the picture from a keyframe");
            }
        }
    }

    stop.store(true, Ordering::Release);
}

fn to_encoded(frame: Frame, format: FrameFormat) -> EncodedFrame {
    EncodedFrame {
        codec: format.codec,
        resolution: format.resolution,
        keyframe: frame.is_keyframe(),
        capture_micros: frame.meta.capture_micros,
        data: frame.data.into(),
    }
}

/// Decode whatever the receiver leaves, and leave the picture for the UI.
fn decode(
    mut decoder: Box<dyn VideoDecoder>,
    encoded: Arc<FrameQueue>,
    frames: Arc<Mailbox<DecodedFrame>>,
    counters: Arc<Counters>,
    stop: Arc<AtomicBool>,
    wants_keyframe: Arc<AtomicBool>,
) {
    let mut failures = 0u64;
    let mut unusable = 0u64;
    let mut last_request = Instant::now() - KEYFRAME_REQUEST_EVERY * 2;

    while !stop.load(Ordering::Acquire) {
        let Some(frame) = encoded.take_timeout(POLL) else {
            continue;
        };

        match decoder.decode(&frame) {
            Ok(Some(picture)) => {
                failures = 0;
                unusable = 0;
                counters.frames_decoded.fetch_add(1, Ordering::Relaxed);
                if frames.put(picture) {
                    counters.frames_superseded.fetch_add(1, Ordering::Relaxed);
                }
            }
            // Normal at the start of a stream and after loss: the decoder is
            // waiting for something it can start from.
            Ok(None) => {
                counters.frames_pending.fetch_add(1, Ordering::Relaxed);
                unusable += 1;
            }
            Err(error) => {
                counters.frames_failed.fetch_add(1, Ordering::Relaxed);
                failures += 1;
                unusable += 1;
                // Logged once per run rather than once per frame: a decoder
                // working from a broken reference fails on every frame until
                // the next keyframe, and that is potentially thousands.
                if failures == 1 {
                    warn!(%error, "could not decode a frame");
                    *counters.decode_error.lock() = Some(error.to_string());
                }
            }
        }

        // Frames are arriving and none of them can be shown, so the thing this
        // end is waiting for is not coming on its own. Only the host can
        // replace it.
        if unusable >= UNUSABLE_BEFORE_KEYFRAME
            && ask_keyframe(&wants_keyframe, &mut last_request)
        {
            unusable = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    // ------------------------------------------------ the encoded-frame queue

    fn encoded(keyframe: bool, at: u32) -> EncodedFrame {
        use pravera_core::{Codec, Resolution};
        EncodedFrame {
            codec: Codec::H264,
            resolution: Resolution::new(64, 64),
            keyframe,
            capture_micros: at,
            data: vec![0, 0, 0, 1, 0x41].into(),
        }
    }

    #[test]
    fn every_frame_the_decoder_can_use_reaches_it_in_order() {
        // The whole reason this is a queue. A predicted frame describes the
        // difference from the frame before it, so one missing frame is not one
        // missing picture — it is every picture until the next keyframe.
        let queue = FrameQueue::new();
        for at in 0..DECODE_QUEUE as u32 {
            assert_eq!(queue.push(encoded(at == 0, at)), Queueing::Accepted);
        }

        let taken: Vec<u32> = (0..DECODE_QUEUE)
            .filter_map(|_| queue.take_timeout(Duration::from_millis(50)))
            .map(|frame| frame.capture_micros)
            .collect();
        assert_eq!(taken, (0..DECODE_QUEUE as u32).collect::<Vec<_>>());
    }

    #[test]
    fn a_decoder_that_falls_behind_gets_a_clean_cut_rather_than_a_broken_chain() {
        // Dropping the oldest frame and carrying on would leave the decoder
        // working from a reference it never saw, failing on every frame for as
        // long as the keyframe interval — seconds of frozen picture. Emptying
        // the queue and waiting for a keyframe costs one visible skip.
        let queue = FrameQueue::new();
        for at in 0..DECODE_QUEUE as u32 {
            queue.push(encoded(false, at));
        }

        assert_eq!(queue.push(encoded(false, 99)), Queueing::Overflowed);
        assert!(
            queue.take_timeout(Duration::from_millis(1)).is_none(),
            "frames from before the cut are still queued, and none of them can be decoded"
        );

        // Everything up to the keyframe is refused, because none of it is
        // usable and queueing it would only delay the frame that is.
        assert_eq!(queue.push(encoded(false, 100)), Queueing::Skipped);
        assert_eq!(queue.push(encoded(false, 101)), Queueing::Skipped);

        assert_eq!(queue.push(encoded(true, 102)), Queueing::Accepted);
        assert_eq!(
            queue
                .take_timeout(Duration::from_millis(50))
                .map(|frame| frame.capture_micros),
            Some(102)
        );

        // And it is back to normal afterwards.
        assert_eq!(queue.push(encoded(false, 103)), Queueing::Accepted);
    }

    #[test]
    fn a_keyframe_is_never_the_frame_that_gets_turned_away() {
        // The keyframe that arrives in the same instant the queue fills is the
        // one frame that would restart the picture. Refusing it leaves the
        // stream waiting for the next one, a whole keyframe interval later.
        let queue = FrameQueue::new();
        for at in 0..DECODE_QUEUE as u32 {
            queue.push(encoded(false, at));
        }
        assert_eq!(queue.push(encoded(true, 50)), Queueing::Accepted);
        assert_eq!(
            queue
                .take_timeout(Duration::from_millis(50))
                .map(|frame| frame.capture_micros),
            Some(50),
            "the keyframe that would have restarted the picture was thrown away"
        );
    }

    #[test]
    fn a_keyframe_in_a_healthy_stream_does_not_throw_away_the_frames_before_it() {
        // Cutting to the keyframe is the right answer when the decoder is
        // behind and the wrong one when it is not: those frames are older,
        // decodable, and already paid for. Discarding them would put a visible
        // skip in the picture once per keyframe interval, for ever, on a
        // session that was working perfectly.
        let queue = FrameQueue::new();
        queue.push(encoded(true, 0));
        queue.push(encoded(false, 1));
        queue.push(encoded(true, 2));

        let taken: Vec<u32> = std::iter::from_fn(|| queue.take_timeout(Duration::from_millis(1)))
            .map(|frame| frame.capture_micros)
            .collect();
        assert_eq!(taken, vec![0, 1, 2]);
    }

    #[test]
    fn the_queue_is_short_enough_that_waiting_in_it_is_cheaper_than_losing_the_chain() {
        // The queue trades latency for continuity, and the trade only holds
        // while it is short. Six frames at sixty is a tenth of a second; a
        // broken reference chain costs the keyframe interval, which is
        // measured in seconds.
        //
        // Expressed as milliseconds rather than as bounds on the constant
        // itself, because the number that matters is the delay a viewer would
        // feel and because a bare comparison against a literal is something the
        // compiler folds away to nothing.
        let worst_delay_ms = DECODE_QUEUE as f32 / 60.0 * 1000.0;
        assert!(
            (10.0..=140.0).contains(&worst_delay_ms),
            "a full queue would hold a frame back {worst_delay_ms:.0} ms at 60 fps"
        );
    }

    #[test]
    fn a_mailbox_holds_the_newest_item_and_says_so() {
        let mailbox = Mailbox::new();

        assert!(!mailbox.put(1), "nothing was displaced by the first item");
        assert!(mailbox.put(2), "the unread item should have been displaced");

        assert_eq!(mailbox.take(), Some(2), "the older item was served");
        assert_eq!(mailbox.take(), None);
    }

    #[test]
    fn waiting_on_an_empty_mailbox_gives_up_rather_than_hanging() {
        // The decode thread relies on this to notice the stop flag. Without a
        // timeout, closing a session would block until the next frame — which,
        // on a still screen, may be never.
        let mailbox: Mailbox<u8> = Mailbox::new();
        assert_eq!(mailbox.take_timeout(Duration::from_millis(20)), None);
    }

    #[test]
    fn a_waiter_is_woken_by_an_arrival() {
        let mailbox = Arc::new(Mailbox::new());
        let writer = mailbox.clone();

        let waiting = thread::spawn(move || mailbox.take_timeout(Duration::from_secs(5)));
        // Racing the waiter deliberately: `take_timeout` has to be correct
        // whether the item arrives before or after the wait begins.
        writer.put(9u8);

        assert_eq!(waiting.join().unwrap(), Some(9));
    }

    /// A decoder that never manages a picture, which is what one looks like
    /// when the keyframe it needed to start from was lost on the way here.
    struct Starving {
        fails: bool,
    }

    impl VideoDecoder for Starving {
        fn codec(&self) -> pravera_core::Codec {
            pravera_core::Codec::OpenH264
        }

        fn decode(&mut self, _: &EncodedFrame) -> pravera_codec::Result<Option<DecodedFrame>> {
            if self.fails {
                Err(pravera_codec::CodecError::Decode("no reference".into()))
            } else {
                Ok(None)
            }
        }
    }

    fn unusable_frames_ask_for_a_keyframe(fails: bool) {
        use pravera_core::{Codec, Resolution};

        let encoded = Arc::new(FrameQueue::new());
        let counters = Arc::new(Counters::default());
        let stop = Arc::new(AtomicBool::new(false));
        let wants_keyframe = Arc::new(AtomicBool::new(false));

        let thread = {
            let (inbox, frames, counters, stop, wants) = (
                encoded.clone(),
                Arc::new(Mailbox::new()),
                counters.clone(),
                stop.clone(),
                wants_keyframe.clone(),
            );
            thread::spawn(move || {
                decode(
                    Box::new(Starving { fails }),
                    inbox,
                    frames,
                    counters,
                    stop,
                    wants,
                )
            })
        };

        // Keep handing it frames, as a host repeating a still picture would.
        // Slowly, so the decoder takes each one before the next arrives: this
        // test is about frames that cannot be decoded, not about a queue
        // overflowing, and a burst would exercise the second instead.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !wants_keyframe.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            encoded.push(EncodedFrame {
                codec: Codec::OpenH264,
                resolution: Resolution::new(64, 64),
                keyframe: false,
                capture_micros: 0,
                data: vec![0, 0, 0, 1, 0x41].into(),
            });
            thread::sleep(Duration::from_millis(10));
        }

        stop.store(true, Ordering::Release);
        thread.join().unwrap();

        assert!(
            wants_keyframe.load(Ordering::Acquire),
            "the decoder was handed frames it could not use and never asked for a keyframe: {:?}",
            counters.snapshot()
        );
    }

    #[test]
    fn a_decoder_waiting_for_a_keyframe_eventually_asks_for_one() {
        // The hole this closes: the receive path only notices a lost chunk when
        // a later frame displaces the incomplete one. On a host nobody is
        // touching, the frame that lost a chunk may be the only frame there
        // is, so nothing displaces it, nothing is counted as lost, and the
        // session sits on "waiting for the first frame" forever.
        unusable_frames_ask_for_a_keyframe(false);
    }

    #[test]
    fn a_decoder_failing_on_every_frame_asks_for_one_too() {
        // Same situation, different symptom: some decoders error on a frame
        // whose reference they never saw rather than returning nothing.
        unusable_frames_ask_for_a_keyframe(true);
    }

    #[test]
    fn a_reassembled_frame_keeps_its_capture_time_and_keyframe_flag() {
        // The capture timestamp is what an end-to-end latency figure is
        // measured from. Losing it here would leave the UI with nothing to
        // report but a plausible-looking guess.
        use pravera_core::{Codec, PixelFormat, Resolution};
        use pravera_proto::frame::{ChunkFlags, FrameMeta};
        use pravera_proto::MonitorId;

        let format = FrameFormat {
            resolution: Resolution::new(1280, 720),
            pixel_format: PixelFormat::Nv12,
            codec: Codec::OpenH264,
        };
        let frame = Frame {
            meta: FrameMeta {
                frame_id: 12,
                capture_micros: 34_567,
                monitor: MonitorId::PRIMARY,
                flags: ChunkFlags::KEYFRAME,
            },
            data: vec![1, 2, 3],
        };

        let encoded = to_encoded(frame, format);
        assert_eq!(encoded.capture_micros, 34_567);
        assert!(encoded.keyframe);
        assert_eq!(encoded.resolution, Resolution::new(1280, 720));
        assert_eq!(encoded.codec, Codec::OpenH264);
        assert_eq!(&encoded.data[..], &[1, 2, 3]);
    }

    #[test]
    fn stats_start_at_zero_and_are_all_distinct_counters() {
        // Two fields sharing a counter is a mistake that looks like working
        // telemetry until someone tries to read it.
        let counters = Counters::default();
        assert_eq!(counters.snapshot(), VideoStats::default());

        counters.frames_decoded.fetch_add(3, Ordering::Relaxed);
        counters.frames_failed.fetch_add(1, Ordering::Relaxed);
        let stats = counters.snapshot();

        assert_eq!(stats.frames_decoded, 3);
        assert_eq!(stats.frames_failed, 1);
        assert_eq!(stats.frames_pending, 0);
        assert_eq!(stats.frames_superseded, 0);
    }

    // ------------------------------------------------------------- the demuxer

    /// Chunks of one video frame, split the way the host splits them.
    fn video_chunks(id: u32, keyframe: bool) -> Vec<Vec<u8>> {
        use pravera_proto::MonitorId;
        use pravera_proto::frame::{FrameMeta, MAX_CHUNK_PAYLOAD, split};

        let meta = FrameMeta {
            frame_id: id,
            capture_micros: id * 1000,
            monitor: MonitorId::PRIMARY,
            flags: if keyframe {
                ChunkFlags::KEYFRAME
            } else {
                ChunkFlags::empty()
            },
        };
        split(meta, &vec![7u8; MAX_CHUNK_PAYLOAD * 2 + 10])
            .expect("a frame of this size always splits")
    }

    /// One audio packet: a single chunk, the way speech actually travels.
    fn audio_chunk(id: u32) -> Vec<u8> {
        use pravera_proto::MonitorId;
        use pravera_proto::frame::{FrameMeta, split};

        let meta = FrameMeta {
            frame_id: id,
            capture_micros: id,
            monitor: MonitorId::PRIMARY,
            flags: ChunkFlags::AUDIO,
        };
        split(meta, b"sound").expect("one small packet").remove(0)
    }

    #[test]
    fn audio_and_video_numbering_from_zero_do_not_collide() {
        // The regression behind "sending sound but no picture": both streams
        // count from zero, and one shared reassembler let the audio stream's
        // completed IDs declare the video stream's frames ancient history.
        // Audio arrives first here, as it does on a real session, because
        // sound is not held back by a picture being cut into pieces.
        let mut demuxer = Demuxer::new();
        let mut assembled_video = Vec::new();

        for id in 0..20u32 {
            assert!(demuxer.push(&audio_chunk(id)).unwrap().is_some());

            let mut done = None;
            for datagram in video_chunks(id, id == 0) {
                if let Some(frame) = demuxer.push(&datagram).unwrap() {
                    done = Some(frame);
                }
            }
            let frame = done.unwrap_or_else(|| panic!("video frame {id} never assembled"));
            assert!(!frame.is_audio());
            assert_eq!(frame.meta.frame_id, id);
            assembled_video.push(frame.meta.frame_id);
        }

        assert_eq!(assembled_video, (0..20).collect::<Vec<_>>());

        // The keyframe flag survives the trip, so the queue can start from
        // the one frame that stands on its own.
        let keyframe = {
            let mut demuxer = Demuxer::new();
            let mut frame = None;
            for datagram in video_chunks(0, true) {
                frame = demuxer.push(&datagram).unwrap().or(frame);
            }
            frame.expect("assembled")
        };
        assert!(keyframe.is_keyframe());
    }

    #[test]
    fn lost_video_is_counted_and_lost_audio_is_not() {
        // Video partials beyond the cap are evicted, and each eviction is one
        // lost chunk the loss counter must see. The audio frames interleaved
        // here complete normally and must contribute nothing: a dropped sound
        // packet never justifies throwing away a picture.
        let mut demuxer = Demuxer::new();
        for id in 0..8u32 {
            let first = video_chunks(id, false).remove(0);
            demuxer.push(&first).unwrap();
            assert!(demuxer.push(&audio_chunk(id)).unwrap().is_some());
        }

        assert_eq!(
            demuxer.dropped_video(),
            4,
            "four frames were evicted from four pending slots"
        );
    }
}
