//! The thing that actually does what the session decided.
//!
//! [`HostSession`](crate::HostSession) rules on every client message and emits
//! an [`Effect`](crate::Effect). This is where an effect becomes a running
//! encoder or a real key press. The split matters: the state machine can be
//! tested exhaustively because it touches nothing, and this file can be small
//! because it decides nothing.
//!
//! ## It does not check permissions, on purpose
//!
//! By the time an event reaches [`Agent::inject`] the session has already
//! confirmed the peer's role allows it. Re-checking here would look like
//! defence in depth and is closer to its opposite: two places deciding the
//! same question will eventually disagree, and the day they do, the one that
//! is wrong is the one nobody was reading. There is exactly one authority, it
//! is `HostSession`, and it is not reachable by a modified client.
//!
//! ## Failures are logged, not fatal
//!
//! An unmapped key, a UIPI refusal, a display that vanished — none of these
//! should tear down a session that is otherwise fine. They are counted and
//! reported, and the session overlay is where a person finds out. The one
//! thing this must never do is fail silently: a remote desktop that ignores
//! clicks is indistinguishable from a dead network, and takes far longer to
//! work out.

use std::sync::Arc;

use pravera_capture::CaptureSource;
use pravera_core::AudioFormat;
use pravera_files::Content;
use pravera_input::{apply, InputError, InputSink, Screen};
use pravera_proto::{ClipboardUpdate, InputEvent, MonitorId, SessionConfig};
use tracing::{debug, info, warn};

use crate::audio::AudioStreamer;
use crate::media::{screen_for, StreamStats, Streamer};
use crate::{ClipboardAnswer, ClipboardAsk, SessionHooks};
use pravera_transport::Session;

/// How many injection failures to log before falling silent.
///
/// A refused injection tends to repeat for every event that follows — a hidden
/// UIPI boundary produces one per mouse move. Logging all of them turns a
/// papercut into gigabytes. The count keeps rising after the log stops, and
/// [`AgentStats`] is what the overlay reads.
const INJECT_LOG_LIMIT: u64 = 5;

/// What the agent has been asked to do and how much of it worked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentStats {
    pub events_injected: u64,
    /// Events the platform would not replay. A steady climb with `injected`
    /// flat means input is being refused, not that nobody is typing.
    pub events_refused: u64,
    /// Keys with no entry in the scan-code table. One key, not a session.
    pub keys_unmapped: u64,
    pub streams_started: u64,
    /// Streams that could not start at all: no such display, no encoder for
    /// the negotiated codec, a resolution the encoder will not take.
    pub streams_failed: u64,
    /// Audio streams that were agreed to but could not be opened. A session
    /// stays perfectly usable without sound, so this is counted rather than
    /// treated as a failure to start.
    pub audio_failed: u64,
}

/// Carries out a session's effects on this machine.
///
/// One per connection. Owns the running stream, so dropping it stops capture.
pub struct Agent {
    session: Session,
    source: Arc<dyn CaptureSource>,
    input: Box<dyn InputSink>,
    /// The current stream, if one is running. Replaced wholesale when the
    /// client changes display or profile — see [`Agent::stream`].
    streamer: Option<Streamer>,
    /// The audio tap, if the session was granted one.
    ///
    /// Independent of the video stream: switching display should not put a
    /// gap in the sound, so this survives a reconfiguration that keeps the
    /// same audio format.
    audio: Option<AudioStreamer>,
    /// The geometry pointer positions are measured against.
    ///
    /// Follows the streamed display, because that is what the client is
    /// looking at. A stale value here is the multi-monitor bug: clicks land on
    /// the display the session *used* to be showing.
    screen: Screen,
    stats: AgentStats,
    /// Counts down [`INJECT_LOG_LIMIT`]; reset whenever a stream restarts,
    /// since a new display is a new chance for things to work.
    inject_logs: u64,
    /// This machine's clipboard, opened the first time one is asked for.
    ///
    /// Lazy because most sessions never touch it, and because on a machine with
    /// no window station opening one fails — which should cost a refused
    /// clipboard request rather than a refused session.
    clipboard: Option<Box<dyn pravera_files::Clipboard>>,
    /// Set once the clipboard has been tried and would not open, so a client
    /// polling twice a second does not retry a failing platform call forever.
    clipboard_unavailable: bool,
}

impl Agent {
    /// Build an agent for one connection.
    ///
    /// Fails only if this platform has no input backend at all. A host that
    /// cannot capture is still worth starting — the failure surfaces when a
    /// stream is requested, with a message naming the display.
    pub fn new(session: Session, source: Arc<dyn CaptureSource>) -> Result<Agent, InputError> {
        let input = pravera_input::sink()?;
        info!(backend = input.name(), "input ready");
        if !input.injects_true_relative() {
            // Said once, at startup, rather than discovered mid-match. The UI
            // shows the same thing on the session overlay.
            debug!(
                backend = input.name(),
                "this backend cannot inject raw relative motion; games reading \
                 raw input will not see mouse movement"
            );
        }
        Ok(Agent::with_sink(session, source, input))
    }

    /// Build an agent around a specific sink.
    ///
    /// The seam tests use, with a `RecordingSink` — a test suite that fights
    /// the developer for the mouse pointer is a test suite nobody runs.
    pub fn with_sink(
        session: Session,
        source: Arc<dyn CaptureSource>,
        input: Box<dyn InputSink>,
    ) -> Agent {
        Agent {
            session,
            source,
            input,
            streamer: None,
            audio: None,
            // Until a stream starts there is nothing to measure against. A
            // pointer event cannot arrive first — the client has to be told a
            // resolution before it can render, let alone click on it.
            screen: Screen::new((0, 0), pravera_core::Resolution::new(1, 1)),
            stats: AgentStats::default(),
            inject_logs: 0,
            clipboard: None,
            clipboard_unavailable: false,
        }
    }

    pub fn stats(&self) -> AgentStats {
        self.stats
    }

    /// What the running stream has sent, if there is one.
    pub fn stream_stats(&self) -> Option<StreamStats> {
        self.streamer.as_ref().map(Streamer::stats)
    }

    /// What the audio tap has sent, if the session has one.
    pub fn audio_stats(&self) -> Option<crate::AudioStats> {
        self.audio.as_ref().map(AudioStreamer::stats)
    }

    /// Whether sound is actually being sent, as opposed to agreed to.
    pub fn is_hearing(&self) -> bool {
        self.audio.as_ref().is_some_and(AudioStreamer::is_running)
    }

    /// The configuration currently being streamed.
    pub fn streaming(&self) -> Option<&SessionConfig> {
        self.streamer.as_ref().map(Streamer::config)
    }

    /// Whether the stream is still alive.
    ///
    /// A stream ends on its own when the display is unplugged or the
    /// connection dies. The driver polls this so the client can be told,
    /// rather than left watching a picture that stopped updating.
    pub fn is_streaming(&self) -> bool {
        self.streamer.as_ref().is_some_and(Streamer::is_running)
    }

    /// Whether relative pointer motion reaches games that read raw input.
    pub fn injects_true_relative(&self) -> bool {
        self.input.injects_true_relative()
    }

    /// Stop everything the session was sending. Idempotent; the agent stays
    /// usable for input.
    pub fn stop(&mut self) {
        self.stop_video();
        if let Some(mut audio) = self.audio.take() {
            audio.stop();
        }
    }

    /// Stop only the picture, leaving any audio running.
    ///
    /// What a reconfiguration wants: switching monitor or profile replaces the
    /// capture pipeline, and taking the sound down with it would put an
    /// audible gap in the middle of a session for no reason.
    fn stop_video(&mut self) {
        if let Some(mut streamer) = self.streamer.take() {
            streamer.stop();
        }
    }

    /// Bring the audio tap in line with what the session agreed to.
    ///
    /// Deliberately a no-op when the format has not changed. A client
    /// switching monitors reconfigures the video stream several times a
    /// session, and tearing the sound down and back up each time would put an
    /// audible gap in it for no reason.
    fn hear(&mut self, wanted: Option<AudioFormat>) {
        let running = self.audio.as_ref().map(AudioStreamer::format);
        if running == wanted && self.audio.as_ref().is_none_or(AudioStreamer::is_running) {
            return;
        }

        if let Some(mut audio) = self.audio.take() {
            audio.stop();
        }
        let Some(format) = wanted else {
            return;
        };

        match AudioStreamer::start(self.session.clone(), format) {
            Ok(audio) => self.audio = Some(audio),
            Err(error) => {
                self.stats.audio_failed += 1;
                // Not fatal, and not sent to the client. A session without
                // sound is worth having; one refused over a sound card is not.
                warn!(%error, codec = format.codec.name(), "could not start audio");
            }
        }
    }

    /// Recompute the pointer geometry for the display being streamed.
    ///
    /// Best effort: if the display list cannot be read the previous geometry
    /// stays, which is wrong by at most one reconfiguration and is better than
    /// resetting to the origin.
    fn follow(&mut self, monitor: MonitorId) {
        match self.source.displays() {
            Ok(displays) => match displays.iter().find(|d| d.id.0 == monitor.0) {
                Some(display) => self.screen = screen_for(display),
                None => warn!(
                    monitor = monitor.0,
                    "streaming a display that is not in the display list; \
                     pointer positions may land on the wrong screen"
                ),
            },
            Err(error) => warn!(%error, "could not re-read the displays"),
        }
    }
}

impl Agent {
    /// This machine's clipboard, opened on first use.
    ///
    /// A platform that has none, or a session with no window station, answers
    /// `None` once and thereafter without retrying. A client polls this on a
    /// timer, and a failing platform call repeated twice a second for an hour
    /// is a log nobody can read past.
    fn clipboard(&mut self) -> Option<&mut (dyn pravera_files::Clipboard + 'static)> {
        if self.clipboard_unavailable {
            return None;
        }
        if self.clipboard.is_none() {
            match pravera_files::clipboard::open() {
                Ok(clipboard) => self.clipboard = Some(clipboard),
                Err(error) => {
                    warn!(%error, "this machine has no clipboard Pravera can reach");
                    self.clipboard_unavailable = true;
                    return None;
                }
            }
        }
        self.clipboard.as_deref_mut()
    }
}

impl SessionHooks for Agent {
    fn inject(&mut self, event: InputEvent) {
        match apply(self.input.as_mut(), &event, self.screen) {
            Ok(()) => self.stats.events_injected += 1,
            Err(InputError::UnmappedKey(usage)) => {
                self.stats.keys_unmapped += 1;
                debug!(usage = format!("{usage:#06x}"), "no mapping for this key");
            }
            Err(error) => {
                self.stats.events_refused += 1;
                if self.inject_logs < INJECT_LOG_LIMIT {
                    self.inject_logs += 1;
                    let last = self.inject_logs == INJECT_LOG_LIMIT;
                    warn!(
                        %error,
                        backend = self.input.name(),
                        further_failures_silenced = last,
                        "could not replay an input event"
                    );
                }
            }
        }
    }

    fn stream(&mut self, config: &SessionConfig) {
        // Stop the old picture first. Two capture sessions on one display is
        // not an error on Windows, it just costs twice as much and delivers
        // each frame to whichever loop asks first. Audio is left alone —
        // `hear` below decides whether it has anything to do.
        self.stop_video();
        self.follow(config.monitor);
        self.inject_logs = 0;

        self.hear(config.audio);

        match Streamer::start(self.session.clone(), self.source.clone(), config) {
            Ok(streamer) => {
                self.stats.streams_started += 1;
                self.streamer = Some(streamer);
            }
            Err(error) => {
                self.stats.streams_failed += 1;
                // The client has already been sent `SessionStarted` — the
                // session replies before it acts, so the frames it is waiting
                // for will simply never arrive. Logging loudly here is what
                // makes that diagnosable.
                warn!(
                    %error,
                    monitor = config.monitor.0,
                    codec = config.format.codec.name(),
                    resolution = %config.format.resolution,
                    "could not start the stream"
                );
            }
        }
    }

    fn keyframe(&mut self) {
        match &self.streamer {
            Some(streamer) => streamer.request_keyframe(),
            // Not an error. A client that lost the first chunks of a stream
            // asks for a keyframe, and the stream may already have ended.
            None => debug!("keyframe requested with nothing streaming"),
        }
    }

    fn clipboard(&mut self, ask: &ClipboardAsk) -> ClipboardAnswer {
        let Some(clipboard) = self.clipboard() else {
            return ClipboardAnswer::Unavailable;
        };

        match ask {
            ClipboardAsk::Read { since } => {
                // The cheap question first. On Windows this is a counter read
                // with no lock; opening the clipboard to discover that nothing
                // has changed would contend with every other program on the
                // machine, twice a second, for the length of the session.
                match clipboard.seq() {
                    Ok(now) if now == *since => {
                        return ClipboardAnswer::Update(ClipboardUpdate::Unchanged)
                    }
                    Ok(_) => {}
                    Err(error) => {
                        debug!(%error, "could not tell whether the clipboard had changed");
                        return ClipboardAnswer::Unavailable;
                    }
                }

                match clipboard.read() {
                    // The sequence comes from the read, not from the check
                    // above: another program may have changed the clipboard in
                    // between, and the client should be told about what it is
                    // actually being sent.
                    Ok((seq, Content::Text(text))) => {
                        ClipboardAnswer::Update(ClipboardUpdate::Text { seq, text })
                    }
                    // An image, a file list, or nothing. Not a failure — and
                    // the sequence still moves, so the client stops asking
                    // about this particular change.
                    Ok((seq, Content::Uncarried)) => {
                        ClipboardAnswer::Update(ClipboardUpdate::Uncarried { seq })
                    }
                    Err(error) => {
                        debug!(%error, "the clipboard would not open for reading");
                        ClipboardAnswer::Unavailable
                    }
                }
            }

            ClipboardAsk::Write { text } => match clipboard.write(text) {
                Ok(seq) => ClipboardAnswer::Written(seq),
                Err(error) => {
                    debug!(%error, "the clipboard would not take the text");
                    ClipboardAnswer::Unavailable
                }
            },
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use pravera_capture::SyntheticSource;
    use pravera_core::Resolution;
    use pravera_input::{Injected, RecordingSink};
    use pravera_proto::{KeyCode, PointerButton};

    use super::*;

    /// An agent with no transport, for the parts that never reach one.
    ///
    /// `Streamer::start` needs a live `Session`; input and geometry do not.
    /// Splitting the two lets the input path be tested without standing up a
    /// QUIC connection for every assertion.
    struct Harness {
        input: Box<RecordingSink>,
        screen: Screen,
        stats: AgentStats,
    }

    impl Harness {
        fn new(screen: Screen) -> Harness {
            Harness {
                input: Box::new(RecordingSink::new()),
                screen,
                stats: AgentStats::default(),
            }
        }

        fn refusing() -> Harness {
            Harness {
                input: Box::new(RecordingSink::refusing("UIPI")),
                screen: Screen::new((0, 0), Resolution::new(1920, 1080)),
                stats: AgentStats::default(),
            }
        }

        /// The body of [`Agent::inject`], against a sink we can read back.
        fn inject(&mut self, event: InputEvent) {
            match apply(self.input.as_mut(), &event, self.screen) {
                Ok(()) => self.stats.events_injected += 1,
                Err(InputError::UnmappedKey(_)) => self.stats.keys_unmapped += 1,
                Err(_) => self.stats.events_refused += 1,
            }
        }
    }

    #[test]
    fn a_click_arrives_where_the_client_pointed() {
        let mut harness = Harness::new(Screen::new((0, 0), Resolution::new(1920, 1080)));

        harness.inject(InputEvent::PointerMoveAbsolute { x: 0.25, y: 0.75 });
        harness.inject(InputEvent::PointerButton {
            button: PointerButton::Left,
            pressed: true,
        });
        harness.inject(InputEvent::PointerButton {
            button: PointerButton::Left,
            pressed: false,
        });

        assert_eq!(
            harness.input.events(),
            [
                Injected::PointerTo { x: 480, y: 809 },
                Injected::Button {
                    button: PointerButton::Left,
                    down: true
                },
                Injected::Button {
                    button: PointerButton::Left,
                    down: false
                },
            ]
        );
        assert_eq!(harness.stats.events_injected, 3);
    }

    #[test]
    fn the_geometry_follows_the_display_being_streamed() {
        // The bug: the client switches to the second monitor, the picture
        // changes, and every click still lands on the first one.
        let source = SyntheticSource::multi(2, Resolution::new(1920, 1080), 60);
        let displays = source.displays().unwrap();

        let mut harness = Harness::new(screen_for(&displays[0]));
        harness.inject(InputEvent::PointerMoveAbsolute { x: 0.0, y: 0.0 });

        harness.screen = screen_for(&displays[1]);
        harness.inject(InputEvent::PointerMoveAbsolute { x: 0.0, y: 0.0 });

        let events = harness.input.take();
        assert_eq!(events[0], Injected::PointerTo { x: 0, y: 0 });
        assert_ne!(
            events[1],
            Injected::PointerTo { x: 0, y: 0 },
            "the second display's origin was ignored"
        );
        assert_eq!(
            events[1],
            Injected::PointerTo {
                x: displays[1].position.0,
                y: displays[1].position.1
            }
        );
    }

    #[test]
    fn an_unmapped_key_is_counted_separately_from_a_refusal() {
        // They mean different things. One key with no scan code is a gap in a
        // table; a refusal means nothing at all is getting through, and only
        // one of those is worth telling the operator about.
        let mut harness = Harness::new(Screen::new((0, 0), Resolution::new(800, 600)));

        harness.inject(InputEvent::Key {
            code: KeyCode(0x0000),
            pressed: true,
        });
        harness.inject(InputEvent::Key {
            code: KeyCode(0x0004),
            pressed: true,
        });

        assert_eq!(harness.stats.keys_unmapped, 0, "the recorder maps nothing");
        assert_eq!(harness.stats.events_injected, 2);
    }

    #[test]
    fn a_platform_that_refuses_everything_is_counted_rather_than_ignored() {
        let mut harness = Harness::refusing();

        for _ in 0..3 {
            harness.inject(InputEvent::PointerMoveAbsolute { x: 0.5, y: 0.5 });
        }

        assert_eq!(harness.stats.events_refused, 3);
        assert_eq!(harness.stats.events_injected, 0);
        assert!(harness.input.events().is_empty());
    }

    #[test]
    fn a_malformed_event_never_reaches_the_platform() {
        // `HostSession` validates before dispatching, so this is the second
        // line. It exists because the cost of being wrong is a pointer parked
        // in a corner with nothing logged.
        let mut harness = Harness::new(Screen::new((0, 0), Resolution::new(1920, 1080)));

        harness.inject(InputEvent::PointerMoveAbsolute {
            x: f32::NAN,
            y: 0.5,
        });

        assert!(harness.input.events().is_empty());
        assert_eq!(harness.stats.events_injected, 0);
        assert_eq!(harness.stats.events_refused, 1);
    }

    #[test]
    fn the_screen_for_a_display_is_its_own_position_and_size() {
        let source = SyntheticSource::multi(3, Resolution::new(2560, 1440), 60);
        let displays = source.displays().unwrap();

        for display in &displays {
            let screen = screen_for(display);
            assert_eq!(screen.origin, display.position);
            assert_eq!(screen.resolution, display.resolution);
        }
    }
}
