//! The handoff between a capture backend and whoever consumes its frames.
//!
//! ## Why a one-frame mailbox instead of a queue
//!
//! A queue is the reflex, and it is wrong here. If the encoder falls behind,
//! every frame a queue holds is a frame the viewer will see *late*; the buffer
//! converts a throughput problem into a latency problem and hides it. So this
//! holds exactly one frame, and a new frame overwrites an unread one.
//!
//! Overwriting is not the same as discarding. The dropped frame's pixels are
//! gone, but its [`Damage`] is folded into the survivor, so the encoder still
//! learns that those regions changed. Without that fold, dropping a frame
//! would leave stale pixels on the viewer's screen with nothing scheduled to
//! repaint them — a smear that persists until the next keyframe.

use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use pravera_core::PixelFormat;

use crate::{CaptureError, CapturedFrame, Display, Result};

/// What a capture backend needs in order to be stopped.
///
/// Implemented by each backend's handle. [`FrameStream`] calls `stop` when it
/// is dropped, so a caller can never leak a capture thread by forgetting.
pub(crate) trait Runner: Send {
    /// Stop capturing and wait for the backend's thread to finish.
    ///
    /// Must be idempotent: it is called explicitly by
    /// [`FrameStream::stop`] and again from `Drop`.
    fn stop(&mut self);
}

/// The result of asking for a frame.
#[derive(Debug)]
pub enum Recv {
    Frame(CapturedFrame),

    /// Nothing new arrived in time.
    ///
    /// Not a fault, and usually not even unusual: a desktop nobody is touching
    /// produces no frames at all, and both Windows Graphics Capture and
    /// PipeWire deliver on change rather than on a clock. Treating this as an
    /// error would tear down a perfectly healthy session the moment the user
    /// stopped to read something.
    Idle,

    /// The backend has stopped and will produce nothing further.
    Ended,
}

/// How the capture has been going.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CaptureStats {
    /// Frames handed to the consumer.
    pub delivered: u64,
    /// Frames overwritten before the consumer got to them. A steady climb
    /// means the encoder cannot keep up with the display.
    pub dropped: u64,
}

impl CaptureStats {
    pub fn produced(&self) -> u64 {
        self.delivered + self.dropped
    }
}

enum Outcome {
    Running,
    Ended,
    Failed(CaptureError),
}

struct State {
    pending: Option<CapturedFrame>,
    delivered: u64,
    dropped: u64,
    outcome: Outcome,
    listening: bool,
}

pub(crate) struct Shared {
    state: Mutex<State>,
    ready: Condvar,
}

impl Shared {
    fn new() -> Shared {
        Shared {
            state: Mutex::new(State {
                pending: None,
                delivered: 0,
                dropped: 0,
                outcome: Outcome::Running,
                listening: true,
            }),
            ready: Condvar::new(),
        }
    }
}

/// Create a connected sink and stream.
///
/// The backend keeps the [`FrameSink`]; the caller gets the [`FrameStream`].
pub(crate) fn channel(display: Display, format: PixelFormat) -> (FrameSink, PendingStream) {
    let shared = std::sync::Arc::new(Shared::new());
    (
        FrameSink {
            shared: shared.clone(),
        },
        PendingStream {
            shared,
            display,
            format,
        },
    )
}

/// A stream that has no backend attached yet.
///
/// Exists so a backend can be constructed with its sink already in hand and
/// only then be bound to the stream. Without the split, the caller would have
/// to hand a half-built stream to the backend or an unstoppable stream to the
/// caller.
pub(crate) struct PendingStream {
    shared: std::sync::Arc<Shared>,
    display: Display,
    format: PixelFormat,
}

impl PendingStream {
    pub(crate) fn attach(self, runner: Box<dyn Runner>) -> FrameStream {
        FrameStream {
            shared: self.shared,
            runner: Some(runner),
            display: self.display,
            format: self.format,
        }
    }
}

/// The producing half. Lives on the backend's capture thread.
pub(crate) struct FrameSink {
    shared: std::sync::Arc<Shared>,
}

impl FrameSink {
    /// Publish a frame, replacing any the consumer has not collected.
    ///
    /// Returns `false` once nobody is listening, which is the backend's signal
    /// to shut itself down. A backend that ignores the return value keeps a
    /// thread and a GPU surface alive for a session that ended.
    pub(crate) fn put(&self, mut frame: CapturedFrame) -> bool {
        let mut state = self.shared.state.lock();
        if !state.listening {
            return false;
        }

        if let Some(stale) = state.pending.take() {
            frame.damage.absorb(&stale.damage);
            state.dropped += 1;
        }
        state.pending = Some(frame);
        drop(state);

        self.shared.ready.notify_one();
        true
    }

    /// The capture finished on its own terms.
    pub(crate) fn end(&self) {
        self.finish(Outcome::Ended);
    }

    /// The capture died. The consumer sees this error exactly once, after any
    /// frame still waiting for it.
    pub(crate) fn fail(&self, error: CaptureError) {
        self.finish(Outcome::Failed(error));
    }

    fn finish(&self, outcome: Outcome) {
        let mut state = self.shared.state.lock();
        if matches!(state.outcome, Outcome::Running) {
            state.outcome = outcome;
        }
        drop(state);
        self.shared.ready.notify_all();
    }

    pub(crate) fn is_listening(&self) -> bool {
        self.shared.state.lock().listening
    }
}

/// The consuming half. Dropping it stops the capture.
pub struct FrameStream {
    shared: std::sync::Arc<Shared>,
    runner: Option<Box<dyn Runner>>,
    display: Display,
    format: PixelFormat,
}

impl FrameStream {
    /// The display being captured.
    pub fn display(&self) -> &Display {
        &self.display
    }

    /// The pixel layout every frame from this stream will use.
    ///
    /// Fixed for the life of the stream, and known before the first frame
    /// arrives, so the encoder can be built up front rather than lazily on a
    /// frame that may be seconds away.
    pub fn format(&self) -> PixelFormat {
        self.format
    }

    pub fn stats(&self) -> CaptureStats {
        let state = self.shared.state.lock();
        CaptureStats {
            delivered: state.delivered,
            dropped: state.dropped,
        }
    }

    /// Wait up to `timeout` for the next frame.
    ///
    /// A pending frame is always delivered before an end-of-stream or an
    /// error, so the last frame of a session is never lost to the news that
    /// the session ended.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Recv> {
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.state.lock();

        loop {
            if let Some(frame) = state.pending.take() {
                state.delivered += 1;
                return Ok(Recv::Frame(frame));
            }

            match state.outcome {
                Outcome::Running => {}
                Outcome::Ended => return Ok(Recv::Ended),
                Outcome::Failed(_) => {
                    let Outcome::Failed(error) =
                        std::mem::replace(&mut state.outcome, Outcome::Ended)
                    else {
                        unreachable!("just matched Failed while holding the lock")
                    };
                    return Err(error);
                }
            }

            if self
                .shared
                .ready
                .wait_until(&mut state, deadline)
                .timed_out()
            {
                return Ok(Recv::Idle);
            }
        }
    }

    /// Stop the capture and wait for the backend to finish.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        {
            let mut state = self.shared.state.lock();
            state.listening = false;
            state.pending = None;
        }
        self.shared.ready.notify_all();

        if let Some(mut runner) = self.runner.take() {
            runner.stop();
        }
    }
}

impl Drop for FrameStream {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl std::fmt::Debug for FrameStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameStream")
            .field("display", &self.display.name)
            .field("format", &self.format)
            .field("stats", &self.stats())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use bytes::Bytes;
    use pravera_core::{Rect, Resolution};

    use super::*;
    use crate::{Damage, DisplayId};

    struct NoopRunner {
        stops: Arc<AtomicUsize>,
    }

    impl Runner for NoopRunner {
        fn stop(&mut self) {
            self.stops.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn display() -> Display {
        Display {
            id: DisplayId::PRIMARY,
            name: "test".into(),
            resolution: Resolution::new(8, 8),
            position: (0, 0),
            scale: 1.0,
            primary: true,
            refresh_hz: 60,
        }
    }

    fn frame(micros: u64, damage: Damage) -> CapturedFrame {
        CapturedFrame {
            display: DisplayId::PRIMARY,
            format: PixelFormat::Bgra8,
            resolution: Resolution::new(8, 8),
            stride: 32,
            pixels: Bytes::from(vec![0u8; 32 * 8]),
            elapsed: Duration::from_micros(micros),
            damage,
        }
    }

    fn pair() -> (FrameSink, FrameStream, Arc<AtomicUsize>) {
        let stops = Arc::new(AtomicUsize::new(0));
        let (sink, pending) = channel(display(), PixelFormat::Bgra8);
        let stream = pending.attach(Box::new(NoopRunner {
            stops: stops.clone(),
        }));
        (sink, stream, stops)
    }

    #[test]
    fn a_frame_put_in_comes_straight_back_out() {
        let (sink, stream, _) = pair();
        assert!(sink.put(frame(100, Damage::Full)));

        let Ok(Recv::Frame(got)) = stream.recv_timeout(Duration::from_secs(1)) else {
            panic!("expected a frame");
        };
        assert_eq!(got.elapsed, Duration::from_micros(100));
        assert_eq!(stream.stats().delivered, 1);
    }

    #[test]
    fn an_unread_frame_is_replaced_by_the_newer_one() {
        // Latency is the whole point: a viewer wants the current screen, not a
        // faithful replay of every screen it missed.
        let (sink, stream, _) = pair();
        sink.put(frame(100, Damage::Full));
        sink.put(frame(200, Damage::Full));

        let Ok(Recv::Frame(got)) = stream.recv_timeout(Duration::from_secs(1)) else {
            panic!("expected a frame");
        };
        assert_eq!(got.elapsed, Duration::from_micros(200));
        assert_eq!(
            stream.stats(),
            CaptureStats {
                delivered: 1,
                dropped: 1
            }
        );
        assert_eq!(stream.stats().produced(), 2);
    }

    #[test]
    fn a_dropped_frame_leaves_its_damage_behind() {
        // Otherwise the region the dropped frame changed is never re-encoded,
        // and the viewer keeps looking at pixels from two frames ago.
        let (sink, stream, _) = pair();
        sink.put(frame(100, Damage::regions(vec![Rect::new(0, 0, 4, 4)])));
        sink.put(frame(200, Damage::regions(vec![Rect::new(4, 4, 4, 4)])));

        let Ok(Recv::Frame(got)) = stream.recv_timeout(Duration::from_secs(1)) else {
            panic!("expected a frame");
        };
        let Damage::Regions(rects) = &got.damage else {
            panic!("expected regions, got {:?}", got.damage);
        };
        assert!(
            rects.contains(&Rect::new(0, 0, 4, 4)),
            "lost the older damage"
        );
        assert!(rects.contains(&Rect::new(4, 4, 4, 4)));
    }

    #[test]
    fn a_quiet_desktop_reads_as_idle_not_as_a_failure() {
        let (_sink, stream, _) = pair();
        assert!(matches!(
            stream.recv_timeout(Duration::from_millis(10)),
            Ok(Recv::Idle)
        ));
    }

    #[test]
    fn the_last_frame_arrives_before_the_news_that_the_stream_ended() {
        let (sink, stream, _) = pair();
        sink.put(frame(100, Damage::Full));
        sink.end();

        assert!(matches!(
            stream.recv_timeout(Duration::from_secs(1)),
            Ok(Recv::Frame(_))
        ));
        assert!(matches!(
            stream.recv_timeout(Duration::from_secs(1)),
            Ok(Recv::Ended)
        ));
    }

    #[test]
    fn a_failure_is_reported_once_and_then_the_stream_is_simply_over() {
        let (sink, stream, _) = pair();
        sink.fail(CaptureError::Lost);

        assert!(matches!(
            stream.recv_timeout(Duration::from_secs(1)),
            Err(CaptureError::Lost)
        ));
        assert!(matches!(
            stream.recv_timeout(Duration::from_secs(1)),
            Ok(Recv::Ended)
        ));
    }

    #[test]
    fn the_first_ending_is_the_one_that_is_reported() {
        // A backend that fails and then tidily ends must not have its failure
        // overwritten by the tidy part.
        let (sink, stream, _) = pair();
        sink.fail(CaptureError::Lost);
        sink.end();

        assert!(matches!(
            stream.recv_timeout(Duration::from_secs(1)),
            Err(CaptureError::Lost)
        ));
    }

    #[test]
    fn dropping_the_stream_stops_the_backend_and_tells_it_to_stop_producing() {
        let (sink, stream, stops) = pair();
        assert!(sink.is_listening());

        drop(stream);

        assert_eq!(
            stops.load(Ordering::SeqCst),
            1,
            "the runner was not stopped"
        );
        assert!(!sink.is_listening());
        assert!(
            !sink.put(frame(1, Damage::Full)),
            "the sink still accepts frames"
        );
    }

    #[test]
    fn stopping_explicitly_does_not_stop_the_backend_twice() {
        let (_sink, stream, stops) = pair();
        stream.stop();
        assert_eq!(stops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_waiting_consumer_wakes_the_moment_a_frame_lands() {
        let (sink, stream, _) = pair();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            sink.put(frame(500, Damage::Full));
            sink
        });

        let started = Instant::now();
        let received = stream.recv_timeout(Duration::from_secs(5));
        let waited = started.elapsed();

        let _sink = handle.join().unwrap();
        assert!(matches!(received, Ok(Recv::Frame(_))));
        assert!(
            waited < Duration::from_secs(4),
            "waited the whole timeout instead of waking on the frame: {waited:?}"
        );
    }
}
