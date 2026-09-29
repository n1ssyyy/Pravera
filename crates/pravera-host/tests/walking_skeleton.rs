//! The whole loop, end to end, in one process.
//!
//! Capture a screen, encode it, cut it into QUIC datagrams, send it over a real
//! connection, reassemble it, decode it, and look at the pixels. Then click on
//! it and check the click arrives.
//!
//! Nothing here is mocked except the two things that would make the test
//! unrunnable rather than more honest: the screen is `SyntheticSource` (so the
//! picture is known in advance and the test does not depend on what happens to
//! be on the developer's desktop) and the input sink is `RecordingSink` (so the
//! test does not fight the developer for their mouse pointer). Both sit behind
//! the same traits the real backends implement, and everything between them —
//! the encoder, the wire format, the transport, the decoder, the permission
//! checks — is the real thing.
//!
//! This is the P1 milestone the plan describes as a walking skeleton: "connect
//! two machines, confirm a moving picture and a working click". Running it in
//! one process rather than two is the only shortcut, and it exercises the same
//! code path.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pravera_auth::Permission;
use pravera_capture::{CaptureSource, SyntheticSource};
use pravera_client::{Client, ClientConfig};
use pravera_core::{Codec, PixelFormat, QualityProfile, Resolution};
use pravera_crypto::Identity;
use pravera_host::{monitors, Agent, HostConfig, HostSession, MemoryStore};
use pravera_input::{Injected, RecordingSink};
use pravera_proto::{InputEvent, MonitorId, PointerButton};
use pravera_transport::{PeerAddress, Reachability, Transport};

/// Every wait is bounded. A pipeline bug usually presents as a hang, and a
/// hung test says nothing about which stage stopped.
const PATIENCE: Duration = Duration::from_secs(20);

/// The synthetic screen. Small on purpose: the point is to prove the path, and
/// a 4K encode would make this test slow enough that nobody runs it.
const SCREEN: Resolution = Resolution::new(640, 360);

async fn within<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(PATIENCE, future).await {
        Ok(value) => value,
        Err(_) => panic!("{what} did not finish within {PATIENCE:?}"),
    }
}

fn loopback(transport: &Transport) -> PeerAddress {
    // Loopback only. `local_address` reports every address the machine has,
    // including a LAN and (on this developer's machine) a Tailscale address,
    // and a test that dials those depends on the network it runs on.
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
        .add("watcher", "just-look", "viewer")
        .expect("adding the viewer");
    store
}

/// A host serving one connection with the real pipeline behind it.
struct Host {
    address: PeerAddress,
    /// What the agent injected. Shared with the running session.
    injected: Arc<parking_lot::Mutex<Vec<Injected>>>,
    _transport: Transport,
}

/// A `RecordingSink` whose events are visible from the test thread.
///
/// The agent owns its sink, and the assertions live out here, so the events
/// have to be readable from both. Wrapping rather than reaching inside keeps
/// `RecordingSink` itself free of synchronisation nobody else needs.
struct SharedSink {
    inner: RecordingSink,
    seen: Arc<parking_lot::Mutex<Vec<Injected>>>,
}

impl pravera_input::InputSink for SharedSink {
    fn name(&self) -> &'static str {
        "shared-recording"
    }

    fn pointer_to(&mut self, x: i32, y: i32) -> pravera_input::Result<()> {
        self.inner.pointer_to(x, y)?;
        self.drain();
        Ok(())
    }

    fn pointer_by(&mut self, dx: i32, dy: i32) -> pravera_input::Result<()> {
        self.inner.pointer_by(dx, dy)?;
        self.drain();
        Ok(())
    }

    fn button(&mut self, button: PointerButton, pressed: bool) -> pravera_input::Result<()> {
        self.inner.button(button, pressed)?;
        self.drain();
        Ok(())
    }

    fn scroll(&mut self, dx: f32, dy: f32) -> pravera_input::Result<()> {
        self.inner.scroll(dx, dy)?;
        self.drain();
        Ok(())
    }

    fn key(&mut self, code: pravera_proto::KeyCode, pressed: bool) -> pravera_input::Result<()> {
        self.inner.key(code, pressed)?;
        self.drain();
        Ok(())
    }

    fn text(&mut self, text: &str) -> pravera_input::Result<()> {
        self.inner.text(text)?;
        self.drain();
        Ok(())
    }
}

impl SharedSink {
    fn drain(&mut self) {
        self.seen.lock().extend(self.inner.take());
    }
}

/// Turn on logging when `RUST_LOG` is set.
///
/// This test spans four threads and two tasks, so when it fails the useful
/// information is in the host's log rather than in the assertion.
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

async fn host() -> Host {
    logging();
    let transport = Transport::bind(&Identity::generate(), Reachability::LocalOnly)
        .await
        .expect("binding the host");
    let address = loopback(&transport);

    let source: Arc<dyn CaptureSource> = Arc::new(SyntheticSource::new(SCREEN, 60));
    let config = Arc::new(HostConfig {
        host_name: "walking skeleton".into(),
        // What this host can really do. `encodable()` is the encoder list, and
        // claiming anything else would negotiate a session that then produces
        // nothing.
        codecs: pravera_codec::encodable(),
        chroma_formats: vec![PixelFormat::Nv12],
        monitors: monitors(source.as_ref()).expect("describing the synthetic display"),
        ..HostConfig::default()
    });

    let injected = Arc::new(parking_lot::Mutex::new(Vec::new()));

    {
        let (transport, source, injected) = (transport.clone(), source.clone(), injected.clone());
        tokio::spawn(async move {
            let session = transport
                .accept()
                .await
                .expect("the endpoint closed before a client arrived")
                .expect("accepting the connection");

            let sink = Box::new(SharedSink {
                inner: RecordingSink::new(),
                seen: injected,
            });
            let mut agent = Agent::with_sink(session.clone(), source, sink);
            let mut host = HostSession::new(config, store(), session.peer_key());
            pravera_host::serve(&session, &mut host, &mut agent).await
        });
    }

    Host {
        address,
        injected,
        _transport: transport,
    }
}

/// Connect to a host.
///
/// Returns the transport as well, which the caller must hold: an endpoint that
/// goes out of scope closes every connection it made, and the failure looks
/// like the host hanging up for no reason.
async fn client_for(host: &Host) -> (Client, Transport) {
    let transport = Transport::bind(&Identity::generate(), Reachability::LocalOnly)
        .await
        .expect("binding the client");
    let config = ClientConfig {
        client_name: "test client".into(),
        codecs: pravera_codec::encodable(),
    };
    let client = within(
        "the connection",
        Client::connect(&transport, &host.address, &config),
    )
    .await
    .expect("connecting over loopback");
    (client, transport)
}

/// How often the test looks for a new frame.
///
/// Roughly a display refresh, because that is what the real consumer is: iced
/// draws, takes whatever is in the mailbox, and draws again.
const REDRAW: Duration = Duration::from_millis(16);

/// Wait for a decoded frame, or say what the pipeline managed instead.
///
/// Polls rather than blocking on the mailbox. `VideoStream::wait` blocks the
/// calling thread, which is right for a UI thread and wrong here: on a
/// single-threaded runtime it would block the very task that receives the
/// datagrams, and the test would report an empty pipeline that was in fact
/// working perfectly.
async fn first_picture(video: &pravera_client::VideoStream) -> pravera_codec::DecodedFrame {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if let Some(frame) = video.latest() {
            return frame;
        }
        assert!(
            video.is_running(),
            "the video stream ended: {:?}",
            video.stats()
        );
        tokio::time::sleep(REDRAW).await;
    }
    panic!(
        "no picture arrived within {PATIENCE:?}; the pipeline got as far as {:?}",
        video.stats()
    );
}

/// The next frame that differs from `previous`.
async fn next_different(
    video: &pravera_client::VideoStream,
    previous: &pravera_codec::DecodedFrame,
) -> Option<pravera_codec::DecodedFrame> {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if let Some(frame) = video.latest() {
            if frame.pixels != previous.pixels {
                return Some(frame);
            }
        }
        tokio::time::sleep(REDRAW).await;
    }
    None
}

#[tokio::test]
async fn a_picture_travels_from_the_hosts_screen_to_the_clients_decoder() {
    let host = host().await;
    let (mut client, _endpoint) = client_for(&host).await;

    client
        .authenticate("driver", "drive-it")
        .await
        .expect("logging in");

    let config = client
        .start_session(MonitorId::PRIMARY, QualityProfile::Adaptive, None, false)
        .await
        .expect("starting the stream");

    // The host decides the format. Rendering what was asked for rather than
    // what was agreed is how a client ends up applying the wrong colour
    // conversion to a correct picture.
    assert_eq!(config.monitor, MonitorId::PRIMARY);
    assert_eq!(config.format.resolution, SCREEN);
    assert!(pravera_codec::encodable().contains(&config.format.codec));

    let video = client.video(None).expect("starting the decoder");
    let picture = first_picture(&video).await;

    assert_eq!(picture.resolution, SCREEN);
    assert_eq!(picture.format, PixelFormat::Rgba8);
    assert_eq!(picture.stride, SCREEN.width as usize * 4);
    assert_eq!(
        picture.pixels.len(),
        SCREEN.width as usize * SCREEN.height as usize * 4
    );

    // A picture of the right size that is entirely one colour would satisfy
    // every assertion above and mean the pipeline produced nothing. The
    // synthetic screen has four coloured quadrants, so a real frame cannot be
    // uniform.
    let first = &picture.pixels[..4];
    assert!(
        picture.pixels.chunks_exact(4).any(|p| p != first),
        "the decoded frame is a single flat colour, so nothing was really coded"
    );

    let stats = video.stats();
    assert!(stats.datagrams_received > 0, "{stats:?}");
    assert!(stats.frames_decoded > 0, "{stats:?}");
    assert_eq!(stats.datagrams_rejected, 0, "{stats:?}");

    client.disconnect("done").await.expect("saying goodbye");
}

#[tokio::test]
async fn the_picture_keeps_moving_rather_than_arriving_once() {
    // One frame proves the path opens. It does not prove the path stays open:
    // an encoder that fails after its first keyframe, or a reassembler that
    // never frees a slot, both pass the previous test.
    let host = host().await;
    let (mut client, _endpoint) = client_for(&host).await;
    client
        .authenticate("driver", "drive-it")
        .await
        .expect("logging in");
    client
        .start_session(MonitorId::PRIMARY, QualityProfile::Latency, None, false)
        .await
        .expect("starting the stream");

    let video = client.video(None).expect("starting the decoder");
    let first = first_picture(&video).await;
    let later = next_different(&video, &first)
        .await
        .expect("the picture never changed after the first frame");
    // The synthetic screen sweeps a bar across itself, so consecutive frames
    // differ. Equal capture timestamps would mean the same frame twice.
    assert_ne!(later.capture_micros, first.capture_micros);
    assert!(video.stats().frames_decoded >= 2, "{:?}", video.stats());

    client.disconnect("done").await.expect("saying goodbye");
}

#[tokio::test]
async fn a_click_travels_back_and_lands_where_the_client_pointed() {
    let host = host().await;
    let (mut client, _endpoint) = client_for(&host).await;
    client
        .authenticate("driver", "drive-it")
        .await
        .expect("logging in");
    client
        .start_session(MonitorId::PRIMARY, QualityProfile::Adaptive, None, false)
        .await
        .expect("starting the stream");

    client
        .send_input(InputEvent::PointerMoveAbsolute { x: 0.5, y: 0.5 })
        .await
        .expect("sending a move");
    client
        .send_input(InputEvent::PointerButton {
            button: PointerButton::Left,
            pressed: true,
        })
        .await
        .expect("sending a press");
    client
        .send_input(InputEvent::PointerButton {
            button: PointerButton::Left,
            pressed: false,
        })
        .await
        .expect("sending a release");

    // Input is fire-and-forget, so there is nothing to await. A ping is a real
    // round-trip on the same ordered stream, which means every input sent
    // before it has already been handled by the time it returns.
    within("the round trip", client.ping())
        .await
        .expect("pinging the host");

    let injected = host.injected.lock().clone();
    assert_eq!(
        injected,
        [
            // The centre of a 640x360 screen at the desktop origin.
            Injected::PointerTo { x: 320, y: 180 },
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

    client.disconnect("done").await.expect("saying goodbye");
}

#[tokio::test]
async fn a_viewer_sees_the_screen_and_cannot_touch_it() {
    // The permission boundary, exercised through the real stack rather than
    // against the state machine alone. A client that ignores its own hint is
    // exactly the client this has to hold against.
    let host = host().await;
    let (mut client, _endpoint) = client_for(&host).await;

    let grant = client
        .authenticate("watcher", "just-look")
        .await
        .expect("logging in");
    assert!(grant.permissions.contains(Permission::VIEW));
    assert!(!grant.permissions.contains(Permission::CONTROL));

    client
        .start_session(MonitorId::PRIMARY, QualityProfile::Adaptive, None, false)
        .await
        .expect("a viewer may watch");

    let video = client.video(None).expect("starting the decoder");
    first_picture(&video).await;

    // `send_input` refuses nothing locally — the hint is advisory — so this
    // reaches the host, which is where the decision is made.
    client
        .send_input(InputEvent::PointerButton {
            button: PointerButton::Left,
            pressed: true,
        })
        .await
        .expect("the client sends it regardless");
    within("the round trip", client.ping())
        .await
        .expect("pinging the host");

    assert!(
        host.injected.lock().is_empty(),
        "a viewer's click reached the host's input backend: {:?}",
        host.injected.lock()
    );

    client.disconnect("done").await.expect("saying goodbye");
}

#[tokio::test]
async fn a_wrong_password_is_refused_without_saying_why() {
    let host = host().await;
    let (mut client, _endpoint) = client_for(&host).await;

    let error = client
        .authenticate("driver", "not-the-password")
        .await
        .expect_err("a wrong password must not be accepted");

    assert!(matches!(error, pravera_client::ClientError::Denied));
    // The message a person sees must not distinguish a wrong password from a
    // username that does not exist, or the login becomes a way to enumerate
    // accounts.
    let shown = error.to_string();
    assert!(!shown.contains("driver"), "{shown}");
    assert!(!shown.to_lowercase().contains("exist"), "{shown}");
    assert!(!error.is_worth_retrying());

    let error = client
        .authenticate("nobody-at-all", "not-the-password")
        .await
        .expect_err("a missing account must be refused too");
    assert_eq!(
        error.to_string(),
        shown,
        "the two refusals read differently"
    );
}

#[tokio::test]
async fn nothing_streams_before_logging_in() {
    let host = host().await;
    let (mut client, _endpoint) = client_for(&host).await;

    let error = client
        .start_session(MonitorId::PRIMARY, QualityProfile::Adaptive, None, false)
        .await
        .expect_err("a stream before authentication must be refused");
    assert!(matches!(error, pravera_client::ClientError::TooSoon(_)));

    assert!(
        client.video(None).is_err(),
        "a decoder started with no stream"
    );
    assert!(client.permissions().is_empty());
}

#[tokio::test]
async fn the_client_can_be_told_the_host_changed_the_terms() {
    // Switching profile restarts the pipeline, and the client has to render
    // what comes back rather than what it asked for. This host codes 4:2:0
    // only, so asking for Quality gets the profile and not its preferred
    // chroma — and saying so is the honest answer.
    let host = host().await;
    let (mut client, _endpoint) = client_for(&host).await;
    client
        .authenticate("driver", "drive-it")
        .await
        .expect("logging in");
    client
        .start_session(MonitorId::PRIMARY, QualityProfile::Latency, None, false)
        .await
        .expect("starting the stream");

    let config = client
        .set_profile(QualityProfile::Quality)
        .await
        .expect("changing the profile");

    assert_eq!(config.profile, QualityProfile::Quality);
    assert_eq!(
        config.format.pixel_format,
        PixelFormat::Nv12,
        "the host promised chroma its encoder cannot produce"
    );
    assert_eq!(
        client.streaming().map(|c| c.profile),
        Some(QualityProfile::Quality)
    );

    let video = client.video(None).expect("starting the decoder");
    first_picture(&video).await;

    client.disconnect("done").await.expect("saying goodbye");
}

#[tokio::test]
async fn a_ping_measures_the_control_stream_and_nothing_else() {
    // The figure the UI shows has to be a real measurement. This asserts it is
    // one — that it moved at all, and that it is not an implausible constant.
    let host = host().await;
    let (mut client, _endpoint) = client_for(&host).await;
    client
        .authenticate("driver", "drive-it")
        .await
        .expect("logging in");

    let rtt = within("the round trip", client.ping())
        .await
        .expect("pinging the host");

    assert!(rtt > Duration::ZERO, "a round trip took no time at all");
    assert!(rtt < PATIENCE, "{rtt:?}");

    // And that the host advertised only encoders it really has.
    let offered = client.welcome().codecs.clone();
    assert_eq!(offered, pravera_codec::encodable());
    assert!(offered.contains(&Codec::OpenH264));
}
