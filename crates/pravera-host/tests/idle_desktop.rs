//! A host nobody is sitting in front of.
//!
//! `walking_skeleton` streams a source that sweeps a bar across itself sixty
//! times a second. That is the right screen for proving the pipeline works and
//! the wrong one for proving anything about *stillness*: a picture that never
//! stops changing cannot show what happens when it stops.
//!
//! Stillness is the case that matters. Capture is damage-driven — Windows
//! Graphics Capture and PipeWire both deliver on change rather than on a clock
//! — so a machine nobody is touching produces exactly one frame and then goes
//! quiet. An unattended host spends nearly all of its life in that state, and
//! everything downstream that only acts when a frame arrives stops acting:
//!
//! - a client that connects sees "waiting for the first frame" forever, because
//!   the frame it is waiting for was captured before it arrived and nothing
//!   will produce another;
//! - a client that loses chunks asks for a keyframe that no capture callback
//!   will ever arrive to carry, so the picture stays broken until somebody
//!   physically moves the host's mouse.
//!
//! Both are tested here against [`SyntheticSource::still`], which is a real
//! backend going genuinely quiet rather than a mock pretending to. The last
//! test runs the same path against this machine's own screen, which is a
//! weaker assertion — a developer's desktop is not still — but the only one
//! that exercises the platform backend end to end.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pravera_capture::{CaptureSource, SyntheticSource};
use pravera_client::{Client, ClientConfig};
use pravera_core::{QualityProfile, Resolution};
use pravera_crypto::Identity;
use pravera_host::{monitors, Agent, HostConfig, HostSession, MemoryStore};
use pravera_input::RecordingSink;
use pravera_proto::MonitorId;
use pravera_transport::{PeerAddress, Reachability, Transport};

/// How long a picture is allowed to take.
///
/// Generous, because the real-screen test encodes whatever this machine's
/// display happens to be — possibly 4K — with a software encoder in a debug
/// build. Short enough that "never" still reads as a failure rather than a
/// hang.
const PATIENCE: Duration = Duration::from_secs(30);

/// How often the test looks in the decoder's mailbox. Roughly a display
/// refresh, which is what the real consumer does.
const REDRAW: Duration = Duration::from_millis(16);

/// The still screen. Small on purpose: the point is the cadence, not the
/// pixels, and a large encode makes this slow enough that nobody runs it.
const SCREEN: Resolution = Resolution::new(640, 360);

/// Long enough to cover several idle repeats, so a stream that sends once and
/// stops is distinguishable from one that keeps going.
const A_WHILE: Duration = Duration::from_secs(3);

fn logging() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "warn".into()),
            )
            .with_test_writer()
            .try_init();
    });
}

fn loopback(transport: &Transport) -> PeerAddress {
    let ports: Vec<SocketAddr> = transport
        .bound_sockets()
        .into_iter()
        .filter(SocketAddr::is_ipv4)
        .map(|socket| SocketAddr::new(Ipv4Addr::LOCALHOST.into(), socket.port()))
        .collect();
    assert!(!ports.is_empty(), "the endpoint bound no IPv4 socket");
    PeerAddress::at(transport.peer_key(), ports)
}

fn store() -> MemoryStore {
    let mut store = MemoryStore::new();
    store
        .add("driver", "drive-it", "operator")
        .expect("adding the operator");
    store
}

struct Host {
    address: PeerAddress,
    _transport: Transport,
}

/// A host serving one connection from whatever screen it is given.
async fn host(source: Arc<dyn CaptureSource>) -> Host {
    logging();
    let transport = Transport::bind(&Identity::generate(), Reachability::LocalOnly)
        .await
        .expect("binding the host");
    let address = loopback(&transport);

    let config = Arc::new(HostConfig {
        host_name: "still host".into(),
        codecs: pravera_codec::encodable(),
        monitors: monitors(source.as_ref()).expect("describing the host's displays"),
        ..HostConfig::default()
    });

    {
        let transport = transport.clone();
        tokio::spawn(async move {
            let session = transport
                .accept()
                .await
                .expect("the endpoint closed before a client arrived")
                .expect("accepting the connection");

            // Recording rather than injecting: nothing here sends input, and a
            // test that could move the developer's mouse is a test nobody runs
            // twice.
            let mut agent =
                Agent::with_sink(session.clone(), source, Box::new(RecordingSink::new()));
            let mut host = HostSession::new(config, store(), session.peer_key());
            pravera_host::serve(&session, &mut host, &mut agent).await
        });
    }

    Host {
        address,
        _transport: transport,
    }
}

/// Connect and log in. The transport comes back because dropping it closes
/// every connection it made, and that failure looks like the host hanging up.
async fn client_for(host: &Host) -> (Client, Transport) {
    let transport = Transport::bind(&Identity::generate(), Reachability::LocalOnly)
        .await
        .expect("binding the client");
    let config = ClientConfig {
        client_name: "idle desktop test".into(),
        codecs: pravera_codec::encodable(),
    };
    let mut client = Client::connect(&transport, &host.address, &config)
        .await
        .expect("connecting over loopback");
    client
        .authenticate("driver", "drive-it")
        .await
        .expect("logging in");
    (client, transport)
}

/// Wait for a decoded frame, saying how far the pipeline got if none comes.
async fn first_picture(video: &pravera_client::VideoStream) -> pravera_codec::DecodedFrame {
    let began = Instant::now();
    let deadline = began + PATIENCE;

    while Instant::now() < deadline {
        if let Some(frame) = video.latest() {
            return frame;
        }
        assert!(
            video.is_running(),
            "the stream ended before a picture arrived: {:?}",
            video.stats()
        );
        tokio::time::sleep(REDRAW).await;
    }

    panic!(
        "no picture in {PATIENCE:?}; the pipeline got as far as {:?}",
        video.stats()
    );
}

/// Watch for `frames` more decoded frames, or give up.
async fn count_more(
    video: &pravera_client::VideoStream,
    frames: u64,
    patience: Duration,
) -> Result<Duration, String> {
    let began = Instant::now();
    let deadline = began + patience;
    let target = video.stats().frames_decoded + frames;

    while Instant::now() < deadline {
        let _ = video.latest();
        if video.stats().frames_decoded >= target {
            return Ok(began.elapsed());
        }
        tokio::time::sleep(REDRAW).await;
    }

    Err(format!(
        "only {} frames decoded in {patience:?}, wanted {target}: {:?}",
        video.stats().frames_decoded,
        video.stats()
    ))
}

#[tokio::test]
async fn a_client_joining_a_still_desktop_still_gets_a_picture() {
    // The bug this exists for. The still source paints once, before the client
    // has finished connecting, and then reports nothing but idle. A streamer
    // that only encodes what capture volunteers therefore sends nothing at
    // all, and the client sits on "waiting for the first frame" until it gives
    // up — on a machine whose desktop is, from the far end, perfectly fine.
    let host = host(Arc::new(SyntheticSource::still(SCREEN))).await;
    let (mut client, _endpoint) = client_for(&host).await;

    let config = client
        .start_session(MonitorId::PRIMARY, QualityProfile::Adaptive, None, false)
        .await
        .expect("starting the stream");

    let video = client.video(None).expect("starting the decoder");
    let picture = first_picture(&video).await;

    assert_eq!(picture.resolution, config.format.resolution);
    assert_eq!(
        picture.pixels.len(),
        picture.resolution.width as usize * picture.resolution.height as usize * 4,
        "the decoded frame is not the size it claims"
    );

    // A frame of the right size that is entirely one colour would satisfy
    // everything above while meaning nothing was really coded. The synthetic
    // screen has coloured corner markers, so a real picture of it cannot be
    // uniform.
    let first = &picture.pixels[..4];
    assert!(
        picture.pixels.chunks_exact(4).any(|p| p != first),
        "the picture is a single flat colour, so nothing was really coded"
    );

    client.disconnect("done").await.expect("saying goodbye");
}

#[tokio::test]
async fn a_still_desktop_keeps_sending_rather_than_going_silent() {
    // One picture proves a client can join. It does not prove the session
    // stays usable: a stream that stops after its first frame is
    // indistinguishable from a working one until the moment the screen changes
    // and the change never arrives.
    let host = host(Arc::new(SyntheticSource::still(SCREEN))).await;
    let (mut client, _endpoint) = client_for(&host).await;
    client
        .start_session(MonitorId::PRIMARY, QualityProfile::Adaptive, None, false)
        .await
        .expect("starting the stream");

    let video = client.video(None).expect("starting the decoder");
    first_picture(&video).await;

    // Three more on a screen that will never change again.
    count_more(&video, 3, A_WHILE)
        .await
        .expect("the stream went silent after its first frame");

    client.disconnect("done").await.expect("saying goodbye");
}

#[tokio::test]
async fn a_keyframe_can_be_asked_for_on_a_desktop_that_never_changes() {
    // What loss recovery depends on. The client asks because its picture is
    // visibly broken; if the request waits for the host's screen to move, an
    // unattended machine never repairs itself at all.
    let host = host(Arc::new(SyntheticSource::still(SCREEN))).await;
    let (mut client, _endpoint) = client_for(&host).await;
    client
        .start_session(MonitorId::PRIMARY, QualityProfile::Adaptive, None, false)
        .await
        .expect("starting the stream");

    let video = client.video(None).expect("starting the decoder");
    first_picture(&video).await;

    client
        .request_keyframe()
        .await
        .expect("asking for a keyframe");

    // Sooner than the idle repeat interval, so this measures the request being
    // acted on rather than the next repeat happening to come along.
    count_more(&video, 1, Duration::from_millis(400))
        .await
        .expect("a keyframe request went unanswered on a still desktop");

    client.disconnect("done").await.expect("saying goodbye");
}

#[tokio::test]
async fn this_machine_can_stream_its_own_screen() {
    // The platform backend, end to end, on whatever display this machine has.
    // Weaker than the tests above — a developer's desktop is not still, so
    // this cannot prove the idle path — but it is the only test that touches
    // Windows Graphics Capture or PipeWire at all.
    //
    // Skipped rather than failed on a machine with nothing plugged into a
    // graphics output: that machine genuinely cannot run this, and reporting
    // it as a broken test helps nobody.
    logging();
    let source: Arc<dyn CaptureSource> = match pravera_capture::source() {
        Ok(source) => source.into(),
        Err(error) => {
            eprintln!("skipping: this machine's screen cannot be captured: {error}");
            return;
        }
    };
    match monitors(source.as_ref()) {
        Ok(displays) if displays.is_empty() => {
            eprintln!("skipping: this machine reports no displays");
            return;
        }
        Ok(_) => {}
        Err(error) => {
            eprintln!("skipping: this machine's displays could not be read: {error}");
            return;
        }
    }

    let host = host(source).await;
    let (mut client, _endpoint) = client_for(&host).await;

    let config = client
        .start_session(MonitorId::PRIMARY, QualityProfile::Adaptive, None, false)
        .await
        .expect("starting the stream");

    let video = client.video(None).expect("starting the decoder");
    let began = Instant::now();
    let picture = first_picture(&video).await;
    eprintln!(
        "first picture of this screen after {:?}: {:?}",
        began.elapsed(),
        picture.resolution
    );

    assert_eq!(picture.resolution, config.format.resolution);
    let stats = video.stats();
    assert!(stats.datagrams_received > 0, "{stats:?}");
    assert!(stats.frames_decoded > 0, "{stats:?}");

    client.disconnect("done").await.expect("saying goodbye");
}
