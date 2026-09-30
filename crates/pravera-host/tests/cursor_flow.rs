//! The host's cursor, end to end: a scripted pointer on the host, a real client
//! at the other end of a real QUIC connection.
//!
//! The pointer is scripted rather than the developer's own, for the reason the
//! walking skeleton uses a synthetic screen: a test that follows whatever the
//! mouse is doing depends on someone not touching it. Everything from the
//! tracker down to the client's state is the real thing.
//!
//! The second test is the compatibility promise: a current viewer dialling a
//! host that only speaks protocol version 2 connects, gets a picture, and is
//! not offered a cursor stream that host would never open.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use pravera_capture::{CaptureSource, SyntheticSource};
use pravera_client::{Client, ClientConfig, CursorState, CursorStream};
use pravera_core::{PixelFormat, QualityProfile, Resolution};
use pravera_crypto::Identity;
use pravera_host::cursor::{CursorSource, RawShape, Sample, ShapeState};
use pravera_host::{monitors, Agent, HostConfig, HostSession, MemoryStore};
use pravera_input::RecordingSink;
use pravera_proto::MonitorId;
use pravera_transport::{PeerAddress, Reachability, Transport};

const PATIENCE: Duration = Duration::from_secs(20);
const SCREEN: Resolution = Resolution::new(640, 360);

async fn within<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(PATIENCE, future).await {
        Ok(value) => value,
        Err(_) => panic!("{what} did not finish within {PATIENCE:?}"),
    }
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

/// A pointer the test moves by writing to `at`.
struct Script {
    at: Arc<Mutex<(i32, i32)>>,
}

impl CursorSource for Script {
    fn sample(&mut self) -> Sample {
        Sample {
            position: Some(*self.at.lock()),
            shape: ShapeState::Handle(7),
        }
    }

    fn shape(&mut self, handle: u64) -> Option<RawShape> {
        (handle == 7).then(|| RawShape {
            width: 8,
            height: 8,
            hot_x: 2,
            hot_y: 3,
            rgba: [200, 100, 50, 255].repeat(64),
        })
    }

    fn arrow(&mut self) -> Option<u64> {
        Some(7)
    }
}

struct Host {
    address: PeerAddress,
    pointer: Arc<Mutex<(i32, i32)>>,
    _transport: Transport,
}

/// A host serving one connection, speaking no protocol newer than `newest`.
async fn host(newest: u16) -> Host {
    let transport = Transport::bind_up_to(&Identity::generate(), Reachability::LocalOnly, newest)
        .await
        .expect("binding the host");
    let address = loopback(&transport);

    let source: Arc<dyn CaptureSource> = Arc::new(SyntheticSource::new(SCREEN, 60));
    let config = Arc::new(HostConfig {
        host_name: "cursor flow".into(),
        codecs: pravera_codec::encodable(),
        chroma_formats: vec![PixelFormat::Nv12],
        monitors: monitors(source.as_ref()).expect("describing the synthetic display"),
        ..HostConfig::default()
    });
    let mut store = MemoryStore::new();
    store.add("driver", "drive-it", "operator").unwrap();

    let pointer = Arc::new(Mutex::new((100, 50)));
    {
        let (transport, pointer) = (transport.clone(), pointer.clone());
        tokio::spawn(async move {
            let session = transport
                .accept()
                .await
                .expect("the endpoint closed before a client arrived")
                .expect("accepting the connection");
            let mut agent = Agent::with_sink(session.clone(), source, Box::new(RecordingSink::new()))
                .with_cursor_source(Some(Box::new(Script { at: pointer })));
            let mut host = HostSession::new(config, store, session.peer_key());
            pravera_host::serve(&session, &mut host, &mut agent).await
        });
    }

    Host {
        address,
        pointer,
        _transport: transport,
    }
}

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

/// Positions are the reverse of the input mapping, which spans `width - 1`, so
/// a pixel lands within one picture pixel of where it was on the desktop.
fn near(got: (i32, i32), want: (i32, i32)) -> bool {
    (got.0 - want.0).abs() <= 1 && (got.1 - want.1).abs() <= 1
}

fn assert_near(got: (i32, i32), want: (i32, i32)) {
    assert!(near(got, want), "{got:?} is not near {want:?}");
}

/// Wait until the cursor state satisfies `wanted`.
async fn cursor_where(
    cursor: &CursorStream,
    what: &str,
    wanted: impl Fn(&CursorState) -> bool,
) -> CursorState {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if let Some((_, state)) = cursor.latest() {
            if wanted(&state) {
                return state;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the cursor never {what}; last seen {:?}", cursor.latest());
}

#[tokio::test]
async fn a_version_three_viewer_receives_the_hosts_cursor_shape_and_position() {
    let host = host(pravera_proto::VERSION).await;
    let (mut client, _endpoint) = client_for(&host).await;
    assert_eq!(client.protocol_version(), pravera_proto::VERSION);
    let cursor = client.cursor().expect("a version 3 host has a cursor stream");

    client.authenticate("driver", "drive-it").await.unwrap();
    client
        .start_session(MonitorId::PRIMARY, QualityProfile::Adaptive, None, false)
        .await
        .unwrap();

    // The shape crossed, and the position is in the picture's pixels: the
    // pointer at desktop (100, 50) on a display streamed 1:1.
    let first = cursor_where(&cursor, "appeared", |s| s.visible && s.image.is_some()).await;
    let image = first.image.expect("checked above");
    assert_eq!((image.width, image.height), (8, 8));
    assert_eq!((image.hot_x, image.hot_y), (2, 3));
    assert_eq!(image.rgba.len(), 8 * 8 * 4);
    assert_near((first.x, first.y), (100, 50));

    // And it follows the host's pointer.
    *host.pointer.lock() = (320, 180);
    let moved = cursor_where(&cursor, "followed the pointer", |s| near((s.x, s.y), (320, 180))).await;
    assert!(moved.visible);
    assert_eq!(moved.image.unwrap().id, image.id, "the same shape is not resent under a new id");

    // Off the streamed display is reported, once, as not visible.
    *host.pointer.lock() = (5000, 180);
    cursor_where(&cursor, "left the display", |s| !s.visible).await;

    client.disconnect("done").await.unwrap();
}

#[tokio::test]
async fn a_current_viewer_still_works_against_a_host_that_only_speaks_version_two() {
    let host = host(2).await;
    let (mut client, _endpoint) = client_for(&host).await;

    assert_eq!(client.protocol_version(), 2);
    assert!(
        client.cursor().is_none(),
        "a version 2 host never opens a cursor stream"
    );

    client.authenticate("driver", "drive-it").await.unwrap();
    let config = client
        .start_session(MonitorId::PRIMARY, QualityProfile::Adaptive, None, false)
        .await
        .unwrap();
    assert_eq!(config.format.resolution, SCREEN);

    // The whole pipeline still runs at version 2.
    let video = client.video(None).expect("starting the decoder");
    let deadline = Instant::now() + PATIENCE;
    while video.latest().is_none() {
        assert!(Instant::now() < deadline, "no picture from a version 2 host");
        tokio::time::sleep(Duration::from_millis(16)).await;
    }

    client.disconnect("done").await.unwrap();
}

/// The same flow with the machine's real pointer instead of a script, so the
/// Windows source, the conversion and the transport are all the real thing.
///
/// Moves the developer's mouse (to the top left of the desktop, where the
/// synthetic 640x360 display is), which is why it is not run with the rest.
///
/// `cargo test -p pravera-host --test cursor_flow -- --ignored real_pointer`
#[cfg(windows)]
#[tokio::test]
#[ignore = "moves the real mouse pointer; run by hand"]
async fn real_pointer_crosses_with_its_real_image() {
    use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;

    let transport = Transport::bind(&Identity::generate(), Reachability::LocalOnly)
        .await
        .unwrap();
    let address = loopback(&transport);
    let source: Arc<dyn CaptureSource> = Arc::new(SyntheticSource::new(SCREEN, 60));
    let config = Arc::new(HostConfig {
        host_name: "real pointer".into(),
        codecs: pravera_codec::encodable(),
        chroma_formats: vec![PixelFormat::Nv12],
        monitors: monitors(source.as_ref()).unwrap(),
        ..HostConfig::default()
    });
    let mut store = MemoryStore::new();
    store.add("driver", "drive-it", "operator").unwrap();
    {
        let transport = transport.clone();
        tokio::spawn(async move {
            let session = transport.accept().await.unwrap().unwrap();
            // The default cursor source: the platform's own.
            let mut agent =
                Agent::with_sink(session.clone(), source, Box::new(RecordingSink::new()));
            let mut host = HostSession::new(config, store, session.peer_key());
            pravera_host::serve(&session, &mut host, &mut agent).await
        });
    }
    let host = Host {
        address,
        pointer: Arc::new(Mutex::new((0, 0))),
        _transport: transport,
    };
    let (mut client, _endpoint) = client_for(&host).await;
    let cursor = client.cursor().unwrap();
    client.authenticate("driver", "drive-it").await.unwrap();
    client
        .start_session(MonitorId::PRIMARY, QualityProfile::Adaptive, None, false)
        .await
        .unwrap();

    unsafe { SetCursorPos(200, 100).unwrap() };
    let seen = cursor_where(&cursor, "reached (200, 100)", |s| {
        s.visible && s.image.is_some() && near((s.x, s.y), (200, 100))
    })
    .await;
    let image = seen.image.unwrap();
    println!(
        "real cursor: {}x{} hotspot {},{} at {},{}",
        image.width, image.height, image.hot_x, image.hot_y, seen.x, seen.y
    );
    assert!(image.rgba.chunks_exact(4).any(|p| p[3] != 0));
}
