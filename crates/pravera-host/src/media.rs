//! Capture, encode, and put the result on the wire.
//!
//! One thread per streamed display, running a loop that is deliberately
//! simple: take the newest frame, compress it, cut it into datagrams, send
//! them. Nothing in it waits on anything else.
//!
//! ## Why a thread rather than a task
//!
//! Both halves of the work are blocking and CPU-bound. Capture arrives on a
//! compositor callback and is handed over through a condition variable; the
//! software encoder occupies a core for milliseconds at a time. Neither yields
//! at a point an async runtime could use, so running them on a runtime worker
//! would stall every other task on that worker for the duration. Sending is
//! the only part that touches the network, and QUIC datagram sends do not
//! block — they queue or they fail.
//!
//! ## What happens when things fall behind
//!
//! Frames are dropped, never queued. `pravera-capture` holds exactly one
//! pending frame and folds a dropped frame's damage into its replacement, so a
//! slow encoder lowers the frame rate instead of raising latency. If a
//! datagram cannot be queued the chunk is simply lost, which is what the wire
//! format already assumes: the client notices the gap and asks for a keyframe.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use bytes::Bytes;
use pravera_capture::{CaptureOptions, CaptureSource, CapturedFrame, DisplayId, FrameStream, Recv};
use pravera_codec::{EncoderSettings, RawFrame, Scaler, VideoEncoder};
use pravera_core::{Codec, Error, PixelFormat, Result};
use pravera_input::Screen;
use pravera_proto::frame::{ChunkFlags, FrameMeta};
use pravera_proto::{Monitor, MonitorId, SessionConfig};
use pravera_transport::Session;
use tracing::{debug, info, warn};

/// How long the loop waits for a frame before looking at the stop flag.
///
/// Short enough that closing a session feels immediate, long enough that an
/// idle desktop costs a handful of wakeups a second.
const POLL: Duration = Duration::from_millis(50);

/// Consecutive send failures tolerated before giving up on the stream.
///
/// One failure is a full send buffer and resolves itself. A hundred in a row
/// is a connection that is gone but has not admitted it yet.
const SEND_FAILURE_LIMIT: u64 = 100;

/// How long after stream start `DatagramsUnsupported` does not count toward
/// the failure limit. The QUIC path takes a moment to settle after the
/// handshake (`Route::default()` — kind/rtt `None`), and `max_datagram_size`
/// is `None` in that window. Killing the stream that would have worked a
/// second later is the early-start race that left `datagrams 0` with a live
/// control channel.
const DATAGRAM_SETTLE: Duration = Duration::from_secs(3);

/// How long a still desktop goes before the last picture is sent again.
///
/// Capture is damage-driven — Windows Graphics Capture and PipeWire both
/// deliver on change rather than on a clock — so a machine nobody is touching
/// produces no frames at all. Waiting for one is therefore waiting for
/// something that is not coming, and two things break outright without a
/// repeat: a client that joins an idle machine has nothing to show, which is
/// exactly the case an unattended host exists for; and a client that lost
/// chunks asks for a keyframe that no capture callback will ever arrive to
/// carry, so the picture stays broken until somebody physically moves the
/// host's mouse.
///
/// Half a second is the longest a person should sit in front of nothing. It
/// costs almost nothing to pay: a picture identical to the last one encodes to
/// a few hundred bytes of skipped macroblocks.
const IDLE_REPEAT: Duration = Duration::from_millis(500);

/// How long capture may produce nothing whatsoever before it is worth saying so.
///
/// Different from an idle desktop, which produces one frame and then stops:
/// this is a backend that has never delivered anything, so there is nothing to
/// repeat and nothing will ever appear at the far end. The viewer sees the same
/// blank screen either way, and this log is the only place the difference is
/// visible.
const SILENCE: Duration = Duration::from_secs(5);

/// How many frames a hardware encoder may swallow, and for how long, without
/// producing anything before it is replaced by the software one.
///
/// An asynchronous encoder legitimately holds a few frames before the first
/// comes out, and an idle desktop offers only two a second, so both bounds
/// have to pass. A working encoder never gets near them; one whose driver has
/// gone quiet (it takes frames and never returns any) otherwise leaves the
/// viewer on a black screen for as long as the session lasts, with every
/// frame counted as "skipped" and no error anywhere.
const STALL_FRAMES: u64 = 30;
const STALL_TIME: Duration = Duration::from_secs(2);

/// Set once a hardware encoder has stalled in this process, so later sessions
/// start on software instead of spending their first seconds finding out
/// again. Cleared by a restart, which is also when a driver update lands.
static HARDWARE_STALLED: AtomicBool = AtomicBool::new(false);

/// The host's displays, described the way the protocol describes them.
///
/// This is the only place `pravera-capture`'s view of a screen becomes the
/// protocol's, and the ids carry across unchanged because both sides agree
/// that zero is the primary display.
pub fn monitors(source: &dyn CaptureSource) -> Result<Vec<Monitor>> {
    Ok(source
        .displays()?
        .into_iter()
        .map(|display| Monitor {
            id: MonitorId(display.id.0),
            name: display.name,
            resolution: display.resolution,
            position: display.position,
            scale: display.scale,
            primary: display.primary,
        })
        .collect())
}

/// What a running stream has done so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamStats {
    /// Frames the encoder was given.
    pub frames_encoded: u64,
    /// Frames capture produced but nobody read in time. A steady climb means
    /// the encoder cannot keep up with the display.
    pub frames_dropped: u64,
    /// Frames rate control chose not to emit.
    pub frames_skipped: u64,
    pub chunks_sent: u64,
    pub bytes_sent: u64,
    /// Chunks that could not be queued. These are simply lost.
    pub send_failures: u64,
}

#[derive(Default)]
struct Counters {
    frames_encoded: AtomicU64,
    frames_skipped: AtomicU64,
    chunks_sent: AtomicU64,
    bytes_sent: AtomicU64,
    send_failures: AtomicU64,
    frames_dropped: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> StreamStats {
        StreamStats {
            frames_encoded: self.frames_encoded.load(Ordering::Relaxed),
            frames_dropped: self.frames_dropped.load(Ordering::Relaxed),
            frames_skipped: self.frames_skipped.load(Ordering::Relaxed),
            chunks_sent: self.chunks_sent.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            send_failures: self.send_failures.load(Ordering::Relaxed),
        }
    }
}

/// Build the negotiated encoder, falling back to software if it will not open.
///
/// The capability probe behind [`pravera_codec::encodable`] asks the operating
/// system which encoders exist; it does not prove that one of them will
/// activate and accept this resolution right now. A GPU that is already
/// encoding for another application, a driver mid-update, a display mode the
/// hardware refuses — each of those turns a listed encoder into a failure at
/// the moment a session starts.
///
/// Falling back costs frame rate. Refusing costs the session, which is worse:
/// the user is left staring at a connection that never shows anything. The
/// returned codec is the one actually in use, so what gets logged is what
/// happened rather than what was asked for.
///
/// Safe on the wire because both H.264 labels name the same bitstream and the
/// client's decoder reads either — see `pravera-codec/tests/hardware_round_trip.rs`.
fn open_encoder(settings: &EncoderSettings) -> Result<(Box<dyn VideoEncoder>, Codec)> {
    if settings.codec.is_hardware() && HARDWARE_STALLED.load(Ordering::Relaxed) {
        let software = EncoderSettings {
            codec: Codec::OpenH264,
            ..settings.clone()
        };
        let encoder = pravera_codec::encoder(&software).map_err(Error::from)?;
        return Ok((encoder, Codec::OpenH264));
    }
    match pravera_codec::encoder(settings) {
        Ok(encoder) => Ok((encoder, settings.codec)),
        Err(error) if settings.codec.is_hardware() => {
            warn!(
                codec = settings.codec.name(),
                %error,
                "the hardware encoder would not open; falling back to software"
            );
            let software = EncoderSettings {
                codec: Codec::OpenH264,
                ..settings.clone()
            };
            let encoder = pravera_codec::encoder(&software).map_err(Error::from)?;
            Ok((encoder, Codec::OpenH264))
        }
        Err(error) => Err(Error::from(error)),
    }
}

/// A running capture-encode-send loop for one display.
///
/// Dropping it stops the loop and waits for the thread.
pub struct Streamer {
    config: SessionConfig,
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    counters: Arc<Counters>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Streamer {
    /// Start streaming what the host and client agreed to.
    pub fn start(
        session: Session,
        source: Arc<dyn CaptureSource>,
        config: &SessionConfig,
        embed_cursor: bool,
    ) -> Result<Streamer> {
        // Headless: a box with no monitor has nothing to capture but Windows'
        // placeholders (a desktop nothing composes to, or the `Generic
        // Non-PnP Monitor` it invents for an output with no EDID) — WGC lists
        // them and never fires, and the run loop hits `SILENCE`. The capture
        // crate tells those apart from real screens by the monitor's EDID id,
        // not its name or size. Grow the virtual display then, or refuse
        // honestly; never stream a test pattern. Hosting normally did this
        // before the session was offered, so this is the belt to its braces.
        if pravera_capture::needs_virtual_display() {
            match pravera_capture::ensure_virtual_display(1920, 1080, 60) {
                Ok(shown) => {
                    tracing::info!(
                        name = %shown.name,
                        id = shown.id.0,
                        "headless host is streaming its virtual display"
                    );
                }
                Err(e) => {
                    tracing::warn!(state = %pravera_capture::virtual_display_diagnostics(), "no virtual display");
                    return Err(Error::Capture(format!(
                        "this host has no screen to share ({e}); plug in a monitor, or let Pravera install its virtual display"
                    )));
                }
            }
        }
        let settings = EncoderSettings {
            codec: config.format.codec,
            ..EncoderSettings::new(config.format.resolution, config.profile)
        };
        let encode_resolution = settings.encode_resolution();

        // Capture no faster than the encoder is budgeted to run. Capturing
        // more would produce frames whose only destination is the drop
        // counter, at the cost of a full-screen copy each.
        let options = CaptureOptions {
            format: PixelFormat::Bgra8,
            cursor: embed_cursor,
            max_fps: Some(settings.fps),
            damage: config.profile.uses_damage_regions(),
        };

        let stream = source.start(DisplayId(config.monitor.0), &options)?;
        let captured = stream.display().resolution;

        // A scaler only when the sizes genuinely differ. The encoder crops an
        // odd display itself, so a one-pixel difference is not worth a full
        // resample of every frame.
        let scaler = if captured.width & !1 != encode_resolution.width
            || captured.height & !1 != encode_resolution.height
        {
            Some(Scaler::new(encode_resolution).map_err(Error::from)?)
        } else {
            None
        };

        let (encoder, encoding_with) = open_encoder(&settings)?;

        let max_dgram = session.max_datagram_size();
        let max_chunk = session.max_chunk_payload();
        info!(
            max_datagram = ?max_dgram,
            max_chunk = ?max_chunk,
            assumed = pravera_proto::MAX_CHUNK_PAYLOAD,
            "path datagram limits"
        );
        if let Some(limit) = max_chunk {
            if limit < pravera_proto::MAX_CHUNK_PAYLOAD {
                warn!(
                    limit,
                    assumed = pravera_proto::MAX_CHUNK_PAYLOAD,
                    "this path carries smaller datagrams than the wire format assumes; \
                     chunks will be refused"
                );
            }
        } else {
            warn!("this path reports no datagram support at all; video cannot flow");
        }

        info!(
            monitor = config.monitor.0,
            display = %captured,
            encoding = %encode_resolution,
            scaled = scaler.is_some(),
            codec = encoding_with.name(),
            profile = config.profile.name(),
            bitrate = settings.bitrate,
            fps = settings.fps,
            "streaming"
        );

        let stop = Arc::new(AtomicBool::new(false));
        let keyframe = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(Counters::default());

        let handle = {
            let (stop, keyframe, counters) = (stop.clone(), keyframe.clone(), counters.clone());
            let monitor = config.monitor;
            let settings = settings.clone();
            thread::Builder::new()
                .name("pravera-stream".into())
                .spawn(move || {
                    run(Pipeline {
                        session,
                        stream,
                        encoder,
                        encoding_with,
                        settings,
                        scaler,
                        monitor,
                        embed_cursor,
                        stop,
                        keyframe,
                        counters,
                    })
                })
                .map_err(|error| Error::Capture(format!("could not start the stream: {error}")))?
        };

        Ok(Streamer {
            config: config.clone(),
            stop,
            keyframe,
            counters,
            handle: Some(handle),
        })
    }

    /// What this stream is sending.
    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// Make the next frame a keyframe.
    ///
    /// Latched, so a burst of requests from a client losing chunks costs one
    /// keyframe rather than one per request.
    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Release);
    }

    pub fn stats(&self) -> StreamStats {
        self.counters.snapshot()
    }

    /// Whether the loop is still going. It ends on its own when the display
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
            debug!(stats = ?self.counters.snapshot(), "stream ended");
        }
    }
}

impl Drop for Streamer {
    fn drop(&mut self) {
        self.stop();
    }
}

impl std::fmt::Debug for Streamer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Streamer")
            .field("monitor", &self.config.monitor.0)
            .field("running", &self.is_running())
            .field("stats", &self.stats())
            .finish()
    }
}

/// Everything the loop owns. A struct so the thread body takes one argument
/// rather than eight.
struct Pipeline {
    session: Session,
    stream: FrameStream,
    encoder: Box<dyn VideoEncoder>,
    /// The codec `encoder` actually is: hardware until it stalls.
    encoding_with: Codec,
    /// What `encoder` was opened with, so its replacement matches it.
    settings: EncoderSettings,
    scaler: Option<Scaler>,
    monitor: MonitorId,
    /// Whether the host's cursor is drawn into the picture. False when the
    /// viewer draws it itself from the cursor stream.
    embed_cursor: bool,
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    counters: Arc<Counters>,
}

/// Whether an idle desktop's last picture is due to be sent again.
///
/// `attempted` is when the loop last had something to encode, not when a
/// datagram last went out: a frame the encoder declined to emit still means the
/// pipeline is awake, and retrying it on the next poll would spin.
fn repeat_due(attempted: Option<Instant>, keyframe: &AtomicBool) -> bool {
    // A keyframe request cannot wait for the desktop to move. Until one is
    // sent the client keeps showing whatever the loss broke.
    if keyframe.load(Ordering::Acquire) {
        return true;
    }
    match attempted {
        // Nothing has been offered to the encoder yet, so whoever is at the
        // far end is looking at nothing and waiting on precisely this.
        None => true,
        Some(at) => at.elapsed() >= IDLE_REPEAT,
    }
}

/// Replace a hardware encoder that has stopped producing video with the
/// software one, mid-stream. Returns `false` when even that will not open.
///
/// Safe on the wire for the same reason [`open_encoder`]'s fallback is: both
/// H.264 labels are one bitstream and the client decodes either. The software
/// encoder opens with a keyframe, so the far end resynchronises on its first
/// frame.
fn fall_back_to_software(pipeline: &mut Pipeline, why: &str) -> bool {
    let software = EncoderSettings {
        codec: Codec::OpenH264,
        ..pipeline.settings.clone()
    };
    match pravera_codec::encoder(&software) {
        Ok(encoder) => {
            warn!(
                monitor = pipeline.monitor.0,
                codec = pipeline.encoding_with.name(),
                reason = why,
                driver = pipeline.encoder.diagnostics().unwrap_or_default(),
                "the hardware encoder stopped producing video; switching to software encoding"
            );
            HARDWARE_STALLED.store(true, Ordering::Relaxed);
            pipeline.encoder = encoder;
            pipeline.encoding_with = Codec::OpenH264;
            true
        }
        Err(error) => {
            warn!(%error, "the software encoder would not open either; the stream ends");
            false
        }
    }
}

fn run(mut pipeline: Pipeline) {
    let mut frame_id: u32 = 0;
    let mut consecutive_failures: u64 = 0;

    // The last thing capture produced, kept so a still desktop has something
    // to send. Cheap to hold: `pixels` is a `Bytes`, so this is a refcount
    // rather than a second copy of the screen.
    let mut last: Option<CapturedFrame> = None;
    let mut attempted: Option<Instant> = None;
    let started = Instant::now();
    let mut reported_silence = false;
    // Heartbeat: one info line every 10s with the counters that distinguish
    // "capture silent" from "encoder skipping" from "sends failing". Without
    // it a stuck pipeline is indistinguishable from a dead process in the log.
    let mut last_heartbeat = Instant::now();
    // Frames handed to the encoder since it last produced one, and when that
    // was: the two halves of the stall check.
    let mut offered_since_output: u64 = 0;
    let mut last_output = Instant::now();

    while !pipeline.stop.load(Ordering::Acquire) {
        if last_heartbeat.elapsed() >= Duration::from_secs(10) {
            last_heartbeat = Instant::now();
            let s = pipeline.counters.snapshot();
            info!(
                monitor = pipeline.monitor.0,
                codec = pipeline.encoding_with.name(),
                has_frame = last.is_some(),
                frames_encoded = s.frames_encoded,
                frames_skipped = s.frames_skipped,
                frames_dropped = s.frames_dropped,
                chunks_sent = s.chunks_sent,
                send_failures = s.send_failures,
                "stream heartbeat"
            );
        }
        match pipeline.stream.recv_timeout(POLL) {
            Ok(Recv::Frame(frame)) => last = Some(frame),

            // A desktop nobody is touching. Send the last picture again if the
            // far end has been left looking at nothing for long enough.
            Ok(Recv::Idle) => {
                if last.is_none() {
                    if !reported_silence && started.elapsed() >= SILENCE {
                        reported_silence = true;
                        warn!(
                            monitor = pipeline.monitor.0,
                            screen = pipeline.stream.display().name,
                            "capture has produced no frames at all, so there is nothing to send"
                        );
                        // A WGC stream that never fires on a machine with no
                        // real screen is a placeholder: make one attempt to
                        // grow the virtual display and capture that instead,
                        // then leave the silence warning above as the
                        // diagnosis. There is no test-pattern fallback.
                        if pravera_capture::needs_virtual_display() {
                            match pravera_capture::ensure_virtual_display(1920, 1080, 60) {
                                Ok(shown) => {
                                    info!(
                                        monitor = pipeline.monitor.0,
                                        name = %shown.name,
                                        "capture was silent on a placeholder; re-opening it on the virtual display"
                                    );
                                    let opts = pravera_capture::CaptureOptions {
                                        format: pravera_core::PixelFormat::Bgra8,
                                        cursor: pipeline.embed_cursor,
                                        max_fps: Some(60),
                                        damage: false,
                                    };
                                    // By the id capture numbers it under now:
                                    // the placeholder it replaces is gone from
                                    // the list, so the old id may name nothing.
                                    if let Ok(src) = pravera_capture::source() {
                                        match src.start(shown.id, &opts) {
                                            Ok(s) => {
                                                pipeline.stream = s;
                                                continue;
                                            }
                                            Err(e) => warn!(%e, "could not capture the virtual display"),
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        %e,
                                        state = %pravera_capture::virtual_display_diagnostics(),
                                        "capture is silent and no virtual display could be shown"
                                    );
                                }
                            }
                        }
                    }
                    continue;
                }
                if !repeat_due(attempted, &pipeline.keyframe) {
                    continue;
                }
            }

            Ok(Recv::Ended) => {
                // `warn`, not `debug`: the session is still up and the client
                // keeps showing the last picture (or nothing) with the control
                // channel answering. A display going away mid-session is the
                // thing to look at first.
                warn!(
                    monitor = pipeline.monitor.0,
                    "capture ended; the picture is gone while the session lives"
                );
                break;
            }
            Err(error) => {
                warn!(%error, "capture failed");
                break;
            }
        }

        // Set by the `Frame` arm above, and the `Idle` arm does not reach here
        // without it.
        let Some(captured) = last.as_ref() else {
            continue;
        };
        attempted = Some(Instant::now());

        if pipeline.keyframe.swap(false, Ordering::AcqRel) {
            pipeline.encoder.request_keyframe();
        }

        let raw = RawFrame {
            resolution: captured.resolution,
            format: captured.format,
            stride: captured.stride,
            pixels: &captured.pixels,
            capture_micros: captured.capture_micros(),
        };

        let prepared = match &mut pipeline.scaler {
            Some(scaler) => match scaler.scale(&raw) {
                Ok(scaled) => scaled,
                Err(error) => {
                    warn!(%error, "could not scale a frame");
                    break;
                }
            },
            None => raw,
        };

        let encoded = match pipeline.encoder.encode(prepared) {
            Ok(Some(frame)) => {
                offered_since_output = 0;
                last_output = Instant::now();
                frame
            }
            Ok(None) => {
                pipeline
                    .counters
                    .frames_skipped
                    .fetch_add(1, Ordering::Relaxed);
                offered_since_output += 1;
                if pipeline.encoding_with.is_hardware()
                    && offered_since_output >= STALL_FRAMES
                    && last_output.elapsed() >= STALL_TIME
                {
                    let why = format!(
                        "took {offered_since_output} frames over {:.1}s and produced nothing",
                        last_output.elapsed().as_secs_f32()
                    );
                    if !fall_back_to_software(&mut pipeline, &why) {
                        break;
                    }
                    offered_since_output = 0;
                    last_output = Instant::now();
                }
                continue;
            }
            Err(error) => {
                warn!(%error, "could not encode a frame");
                // A hardware encoder failing mid-stream is a driver problem the
                // session does not have to share: the software encoder makes
                // the same bitstream.
                if pipeline.encoding_with.is_hardware()
                    && fall_back_to_software(&mut pipeline, &error.to_string())
                {
                    offered_since_output = 0;
                    last_output = Instant::now();
                    continue;
                }
                break;
            }
        };
        pipeline
            .counters
            .frames_encoded
            .fetch_add(1, Ordering::Relaxed);

        let meta = FrameMeta {
            frame_id,
            capture_micros: encoded.capture_micros,
            monitor: pipeline.monitor,
            flags: if encoded.keyframe {
                ChunkFlags::KEYFRAME
            } else {
                ChunkFlags::empty()
            },
        };
        frame_id = frame_id.wrapping_add(1);

        // Dynamic chunk sizing: query the live path limit per frame and use
        // `min(MAX, live)`. After `Direct TimedOut → Relay` migration the
        // limit shrinks and static `1086` chunks all fail `DatagramTooLarge`
        // while small audio packets still fit — the exact "control+audio work,
        // video 0" asymmetry. The receiver accepts any `<= MAX`, so smaller
        // chunks need no wire change.
        let live_limit = pipeline
            .session
            .max_chunk_payload()
            .unwrap_or(pravera_proto::MAX_CHUNK_PAYLOAD)
            .min(pravera_proto::MAX_CHUNK_PAYLOAD);
        let chunks = match pravera_proto::frame::split_with(meta, &encoded.data, live_limit) {
            Ok(chunks) => chunks,
            Err(error) => {
                // A frame too large for the wire format to describe. Dropping
                // it and asking the encoder to start again from a keyframe is
                // the only recovery: every later frame refers back to this one.
                warn!(%error, bytes = encoded.data.len(), "a frame would not fit the wire format");
                pipeline.encoder.request_keyframe();
                continue;
            }
        };

        for chunk in chunks {
            let size = chunk.len();
            match pipeline.session.send_media(Bytes::from(chunk)) {
                Ok(()) => {
                    consecutive_failures = 0;
                    pipeline
                        .counters
                        .chunks_sent
                        .fetch_add(1, Ordering::Relaxed);
                    pipeline
                        .counters
                        .bytes_sent
                        .fetch_add(size as u64, Ordering::Relaxed);
                }
                Err(error) => {
                    let is_unsupported = matches!(
                        error,
                        pravera_transport::TransportError::DatagramsUnsupported
                    );
                    let is_too_large = matches!(
                        error,
                        pravera_transport::TransportError::DatagramTooLarge { .. }
                    );
                    // First occurrence gets the full context: size/limit/route
                    // is what distinguishes "relay MTU shrank" from "dead".
                    if consecutive_failures == 0 || is_too_large {
                        warn!(
                            %error,
                            route = ?pipeline.session.route().kind,
                            max_datagram = ?pipeline.session.max_datagram_size(),
                            frame = frame_id.wrapping_sub(1),
                            "video datagram send failed"
                        );
                    }
                    // Settling grace: `DatagramsUnsupported` in the first 3s
                    // is the path selecting, not a dead connection.
                    if is_unsupported && started.elapsed() < DATAGRAM_SETTLE {
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    consecutive_failures += 1;
                    pipeline
                        .counters
                        .send_failures
                        .fetch_add(1, Ordering::Relaxed);
                    if consecutive_failures >= SEND_FAILURE_LIMIT {
                        warn!(
                            %error,
                            chunks_sent = pipeline.counters.chunks_sent.load(Ordering::Relaxed),
                            send_failures = pipeline.counters.send_failures.load(Ordering::Relaxed),
                            route = ?pipeline.session.route().kind,
                            "giving up on a connection that will not take datagrams"
                        );
                        return;
                    }
                }
            }
        }
    }

    // Read once at the end rather than on every frame: the capture layer keeps
    // this count itself, and the only consumer is the summary.
    pipeline
        .counters
        .frames_dropped
        .store(pipeline.stream.stats().dropped, Ordering::Relaxed);
}

/// Where a streamed display sits, for turning normalised pointer positions
/// back into desktop coordinates.
///
/// The definition lives in `pravera-input`, next to the sinks that consume it.
/// Re-exported here because the geometry comes from the capture layer, which
/// is this module's business, and the agent needs both halves in one place.
pub fn screen_for(display: &pravera_capture::Display) -> Screen {
    Screen::new(display.position, display.resolution)
}

#[cfg(test)]
mod tests {
    use pravera_capture::SyntheticSource;
    use pravera_core::Resolution;

    use super::*;

    #[test]
    fn synthetic_displays_become_monitors_the_protocol_can_name() {
        let source = SyntheticSource::multi(3, Resolution::new(1280, 720), 60);
        let monitors = monitors(&source).unwrap();

        assert_eq!(monitors.len(), 3);
        assert_eq!(monitors[0].id, MonitorId::PRIMARY);
        assert!(monitors[0].primary);
        assert_eq!(monitors[1].id, MonitorId(1));
        assert!(!monitors[1].primary);
        assert_eq!(monitors[2].resolution, Resolution::new(1280, 720));
    }

    #[test]
    fn display_ids_and_monitor_ids_are_the_same_numbers() {
        // The protocol addresses displays by these ids. If the two layers ever
        // numbered them differently, a client asking for monitor 1 would be
        // shown monitor 2 and nothing would look wrong from either end.
        let source = SyntheticSource::multi(4, Resolution::new(800, 600), 60);
        let displays = source.displays().unwrap();
        let monitors = monitors(&source).unwrap();

        for (display, monitor) in displays.iter().zip(&monitors) {
            assert_eq!(display.id.0, monitor.id.0);
            assert_eq!(display.name, monitor.name);
            assert_eq!(display.primary, monitor.primary);
        }
    }

    #[test]
    fn a_display_becomes_the_screen_its_pointer_positions_are_measured_against() {
        // The origin has to come from the display's own position, not from
        // zero: on a multi-monitor host, measuring the second display from the
        // desktop origin puts every click on the first one.
        let source = SyntheticSource::multi(2, Resolution::new(1920, 1080), 60);
        let displays = source.displays().unwrap();

        let second = screen_for(&displays[1]);
        assert_eq!(second.origin, displays[1].position);
        assert_eq!(second.resolution, Resolution::new(1920, 1080));
        assert_eq!(second.to_desktop(0.0, 0.0), displays[1].position);
    }
}
