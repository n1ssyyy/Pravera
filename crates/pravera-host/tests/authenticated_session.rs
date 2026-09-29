//! A real client talking to a real host over a real QUIC connection.
//!
//! `session.rs` tests the rules by calling the state machine directly. This
//! file tests that the rules survive the wire: the same refusals, but reached
//! through iroh, postcard framing and the control stream, with the host running
//! its own loop in its own task.
//!
//! The plan lists this as a manual step for P3 ("log in as `viewer` and confirm
//! input is refused host-side; as `operator` confirm it works"). It is cheaper
//! to run it every build.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pravera_core::{Codec, PixelFormat, QualityProfile, Resolution};
use pravera_crypto::Identity;
use pravera_host::{HostConfig, HostSession, MemoryStore, SessionHooks};
use pravera_proto::{
    AuthResult, ClientMessage, Credentials, Hello, HostMessage, InputEvent, Monitor, MonitorId,
    PointerButton, ProtocolError, SessionConfig, SessionRequest, VERSION,
};
use pravera_transport::{ClientControl, PeerAddress, Reachability, Transport};

const PATIENCE: Duration = Duration::from_secs(15);

async fn within<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(PATIENCE, future).await {
        Ok(value) => value,
        Err(_) => panic!("{what} did not finish within {PATIENCE:?}"),
    }
}

// ------------------------------------------------------------------ harness

/// What the host actually did, as opposed to what it said.
///
/// A refusal that still injects the input would pass every reply-shaped
/// assertion. This is how the tests check the other half.
#[derive(Debug, Default)]
struct Recorded {
    injected: Vec<InputEvent>,
    streams: Vec<SessionConfig>,
    keyframes: usize,
}

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Recorded>>);

impl Recorder {
    fn read<T>(&self, f: impl FnOnce(&Recorded) -> T) -> T {
        f(&self.0.lock().expect("the recorder was poisoned"))
    }
}

impl SessionHooks for Recorder {
    fn inject(&mut self, event: InputEvent) {
        self.0.lock().unwrap().injected.push(event);
    }

    fn stream(&mut self, config: &SessionConfig) {
        self.0.lock().unwrap().streams.push(config.clone());
    }

    fn keyframe(&mut self) {
        self.0.lock().unwrap().keyframes += 1;
    }
}

fn monitors() -> Vec<Monitor> {
    vec![
        Monitor {
            id: MonitorId::PRIMARY,
            name: "primary".into(),
            resolution: Resolution::new(2560, 1440),
            position: (0, 0),
            scale: 1.0,
            primary: true,
        },
        Monitor {
            id: MonitorId(1),
            name: "secondary".into(),
            resolution: Resolution::new(1920, 1080),
            position: (2560, 0),
            scale: 1.0,
            primary: false,
        },
    ]
}

fn store() -> MemoryStore {
    let mut store = MemoryStore::new();
    store.add("looker", "look-only", "viewer").unwrap();
    store.add("driver", "drive-it", "operator").unwrap();
    store
}

fn loopback(transport: &Transport) -> PeerAddress {
    let ports: Vec<SocketAddr> = transport
        .bound_sockets()
        .into_iter()
        .filter(|socket| socket.is_ipv4())
        .map(|socket| SocketAddr::new(Ipv4Addr::LOCALHOST.into(), socket.port()))
        .collect();
    assert!(!ports.is_empty(), "the endpoint bound no IPv4 socket");
    PeerAddress::at(transport.peer_key(), ports)
}

/// A host serving one connection, and the client's end of its control stream.
struct Harness {
    control: ClientControl,
    recorder: Recorder,
    // Held so the endpoints and the connection stay alive for the test's
    // duration. Dropping a Session closes the connection under the host.
    _host: Transport,
    _client: Transport,
    _session: pravera_transport::Session,
}

async fn start() -> Harness {
    let host_transport = Transport::bind(&Identity::generate(), Reachability::LocalOnly)
        .await
        .expect("binding the host");
    let client_transport = Transport::bind(&Identity::generate(), Reachability::LocalOnly)
        .await
        .expect("binding the client");
    let address = loopback(&host_transport);

    let recorder = Recorder::default();
    let config = Arc::new(HostConfig {
        host_name: "workshop".into(),
        codecs: vec![Codec::H265, Codec::H264],
        // A host whose hardware can actually code 4:4:4, so the Quality
        // profile's preferred chroma is a real offer rather than a claim it
        // would have to walk back. A host without it is covered by the unit
        // tests, which assert it advertises what it can produce instead.
        chroma_formats: vec![PixelFormat::Yuv444, PixelFormat::Nv12],
        monitors: monitors(),
        ..HostConfig::default()
    });

    {
        let transport = host_transport.clone();
        let mut hooks = recorder.clone();
        tokio::spawn(async move {
            let session = transport
                .accept()
                .await
                .expect("the endpoint closed before a client arrived")
                .expect("accepting the connection");
            let mut host = HostSession::new(config, store(), session.peer_key());
            pravera_host::serve(&session, &mut host, &mut hooks).await
        });
    }

    let session = within("the dial", client_transport.connect(&address))
        .await
        .expect("connecting over loopback");
    let control = session
        .open_control()
        .await
        .expect("opening the control stream");

    Harness {
        control,
        recorder,
        _host: host_transport,
        _client: client_transport,
        _session: session,
    }
}

impl Harness {
    async fn ask(&mut self, message: ClientMessage) -> HostMessage {
        within("a request", self.control.request(&message))
            .await
            .unwrap_or_else(|e| panic!("the host did not answer: {e}"))
    }

    async fn tell(&mut self, message: ClientMessage) {
        within("a send", self.control.send(&message))
            .await
            .expect("sending");
    }

    /// Send something the host does not answer, then wait until it has
    /// definitely been processed.
    ///
    /// The control stream is ordered and the host handles messages one at a
    /// time, so a pong proves everything sent before the ping is done. This is
    /// what makes the fire-and-forget input path testable without a sleep.
    async fn tell_and_settle(&mut self, message: ClientMessage) {
        self.tell(message).await;
        let pong = self.ask(ClientMessage::Ping { nonce: 0xfeed }).await;
        assert_eq!(pong, HostMessage::Pong { nonce: 0xfeed });
    }

    async fn greet(&mut self) {
        let reply = self
            .ask(ClientMessage::Hello(Hello {
                version: VERSION,
                client_name: "laptop".into(),
                codecs: vec![Codec::H264],
            }))
            .await;
        match reply {
            HostMessage::Welcome(welcome) => assert_eq!(welcome.host_name, "workshop"),
            other => panic!("expected a welcome, got {other:?}"),
        }
    }

    async fn log_in(&mut self, username: &str, password: &str) -> AuthResult {
        let reply = self
            .ask(ClientMessage::Authenticate(Credentials {
                username: username.into(),
                password: password.into(),
            }))
            .await;
        match reply {
            HostMessage::AuthResult(result) => result,
            other => panic!("expected an auth result, got {other:?}"),
        }
    }

    async fn start_stream(&mut self, monitor: MonitorId) -> HostMessage {
        self.ask(ClientMessage::StartSession(SessionRequest {
            monitor,
            profile: QualityProfile::Adaptive,
            max_resolution: None,
            audio: false,
        }))
        .await
    }
}

fn click() -> ClientMessage {
    ClientMessage::Input(InputEvent::PointerButton {
        button: PointerButton::Left,
        pressed: true,
    })
}

fn failure(message: &HostMessage) -> Option<&ProtocolError> {
    match message {
        HostMessage::Failed(error) => Some(error),
        _ => None,
    }
}

// -------------------------------------------------------------------- tests

#[tokio::test]
async fn an_operator_completes_the_handshake_and_drives_the_host() {
    let mut client = start().await;
    client.greet().await;

    let result = client.log_in("driver", "drive-it").await;
    match result {
        AuthResult::Granted {
            username,
            role,
            permissions,
        } => {
            assert_eq!(username, "driver");
            assert_eq!(role, "operator");
            assert!(permissions.allows(pravera_auth::Permission::CONTROL));
        }
        AuthResult::Denied => panic!("the operator should have been let in"),
    }

    match client.start_stream(MonitorId::PRIMARY).await {
        HostMessage::SessionStarted(config) => {
            assert_eq!(config.format.resolution, Resolution::new(2560, 1440));
            assert_eq!(config.format.codec, Codec::H264, "the best both ends have");
        }
        other => panic!("expected a session, got {other:?}"),
    }

    client.tell_and_settle(click()).await;

    client.recorder.read(|recorded| {
        assert_eq!(
            recorded.streams.len(),
            1,
            "the pipeline should have started once"
        );
        assert_eq!(
            recorded.injected.len(),
            1,
            "the click should have been injected"
        );
    });
}

#[tokio::test]
async fn a_viewer_is_refused_control_by_the_host_over_the_wire() {
    // The one the plan calls out by name. The client is told at login that it
    // holds VIEW only, and then ignores that and sends a click anyway, which is
    // exactly what a patched client would do.
    let mut client = start().await;
    client.greet().await;

    let result = client.log_in("looker", "look-only").await;
    let AuthResult::Granted { permissions, .. } = result else {
        panic!("a viewer should still be able to connect");
    };
    assert!(permissions.allows(pravera_auth::Permission::VIEW));
    assert!(!permissions.allows(pravera_auth::Permission::CONTROL));

    assert!(matches!(
        client.start_stream(MonitorId::PRIMARY).await,
        HostMessage::SessionStarted(_)
    ));

    // Sent and not answered: input is fire-and-forget in both directions, so
    // a refusal is silent on the wire. The ping that follows proves the host
    // processed the click, since the control stream is ordered.
    client.tell_and_settle(click()).await;

    // And, crucially, nothing was injected. A refusal that still moved the
    // mouse would pass a reply-only assertion.
    client.recorder.read(|recorded| {
        assert!(
            recorded.injected.is_empty(),
            "a viewer's input reached the input sink: {:?}",
            recorded.injected
        );
    });
}

#[tokio::test]
async fn a_viewer_cannot_reach_the_second_display() {
    let mut client = start().await;
    client.greet().await;
    assert!(matches!(
        client.log_in("looker", "look-only").await,
        AuthResult::Granted { .. }
    ));

    let listing = client.ask(ClientMessage::ListMonitors).await;
    assert_eq!(failure(&listing), Some(&ProtocolError::PermissionDenied));

    // Guessing the id instead of asking for the list must not work either.
    let guessed = client.start_stream(MonitorId(1)).await;
    assert_eq!(failure(&guessed), Some(&ProtocolError::PermissionDenied));

    client
        .recorder
        .read(|recorded| assert!(recorded.streams.is_empty()));
}

#[tokio::test]
async fn nothing_works_before_logging_in() {
    let mut client = start().await;
    client.greet().await;

    for message in [
        ClientMessage::ListMonitors,
        ClientMessage::StartSession(SessionRequest {
            monitor: MonitorId::PRIMARY,
            profile: QualityProfile::Adaptive,
            max_resolution: None,
            audio: false,
        }),
    ] {
        let reply = client.ask(message.clone()).await;
        assert_eq!(
            failure(&reply),
            Some(&ProtocolError::Unauthenticated),
            "{message:?} was answered before login"
        );
    }

    // The fire-and-forget pair go unanswered rather than refused, so they are
    // sent and then chased with a ping to prove they were seen and dropped.
    for message in [click(), ClientMessage::RequestKeyframe] {
        client.tell_and_settle(message).await;
    }

    client.recorder.read(|recorded| {
        assert!(recorded.injected.is_empty());
        assert!(recorded.streams.is_empty());
    });
}

#[tokio::test]
async fn a_refusal_does_not_say_whether_the_account_exists() {
    // Three different reasons, one answer. Run over the wire because the
    // collapse has to survive serialisation, not just the state machine.
    let mut wrong_password = start().await;
    wrong_password.greet().await;
    let a = wrong_password.log_in("driver", "not-the-password").await;

    let mut no_such_user = start().await;
    no_such_user.greet().await;
    let b = no_such_user
        .log_in("nobody-at-all", "not-the-password")
        .await;

    assert_eq!(a, AuthResult::Denied);
    assert_eq!(a, b, "an unknown account was distinguishable on the wire");
}

#[tokio::test]
async fn three_wrong_passwords_end_the_connection() {
    let mut client = start().await;
    client.greet().await;

    assert_eq!(client.log_in("driver", "wrong").await, AuthResult::Denied);
    assert_eq!(client.log_in("driver", "wrong").await, AuthResult::Denied);
    // The third refusal is still delivered, and then the host hangs up.
    assert_eq!(client.log_in("driver", "wrong").await, AuthResult::Denied);

    let after = within("the hangup", client.control.recv()).await;
    assert!(
        after.is_err(),
        "the connection stayed open after the attempt limit: {after:?}"
    );
}

#[tokio::test]
async fn a_client_speaking_the_wrong_version_is_told_which_one_is_wanted() {
    let mut client = start().await;
    let reply = client
        .ask(ClientMessage::Hello(Hello {
            version: VERSION + 7,
            client_name: "from-the-future".into(),
            codecs: vec![Codec::H264],
        }))
        .await;

    assert_eq!(
        failure(&reply),
        Some(&ProtocolError::VersionMismatch {
            ours: VERSION,
            theirs: VERSION + 7,
        })
    );
}

#[tokio::test]
async fn an_operator_can_switch_display_and_profile_mid_session() {
    let mut client = start().await;
    client.greet().await;
    client.log_in("driver", "drive-it").await;
    client.start_stream(MonitorId::PRIMARY).await;

    match client.ask(ClientMessage::SelectMonitor(MonitorId(1))).await {
        HostMessage::SessionStarted(config) => {
            assert_eq!(config.monitor, MonitorId(1));
            assert_eq!(config.format.resolution, Resolution::new(1920, 1080));
        }
        other => panic!("expected a reconfiguration, got {other:?}"),
    }

    match client
        .ask(ClientMessage::SetProfile(QualityProfile::Quality))
        .await
    {
        HostMessage::SessionStarted(config) => {
            assert_eq!(config.profile, QualityProfile::Quality);
            assert_eq!(config.format.pixel_format, PixelFormat::Yuv444);
        }
        other => panic!("expected a reconfiguration, got {other:?}"),
    }

    client.tell_and_settle(ClientMessage::RequestKeyframe).await;

    client.recorder.read(|recorded| {
        assert_eq!(
            recorded.streams.len(),
            3,
            "start, monitor switch and profile change each reconfigure the pipeline"
        );
        assert_eq!(recorded.keyframes, 1);
    });
}

#[tokio::test]
async fn saying_goodbye_ends_the_session_cleanly() {
    let mut client = start().await;
    client.greet().await;
    client.log_in("driver", "drive-it").await;

    client
        .tell(ClientMessage::Goodbye {
            reason: "window closed".into(),
        })
        .await;

    let after = within("the close", client.control.recv()).await;
    assert!(
        after.is_err(),
        "the host kept the session open after goodbye"
    );
}
