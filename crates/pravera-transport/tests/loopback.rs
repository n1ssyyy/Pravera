//! Two endpoints on one machine, talking over a real QUIC connection.
//!
//! Nothing here is mocked. Each test binds two iroh endpoints with real device
//! identities, dials one from the other over loopback, and exercises the
//! channels the protocol actually uses. It is the cheapest place to catch the
//! integration problems that unit tests structurally cannot.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use bytes::Bytes;
use pravera_core::Codec;
use pravera_crypto::Identity;
use pravera_proto::frame::{split, FrameMeta};
use pravera_proto::{
    ChunkFlags, ClientMessage, Hello, HostMessage, MonitorId, Reassembler, Welcome,
    MAX_CHUNK_PAYLOAD, SAFE_DATAGRAM,
};
use pravera_transport::{PeerAddress, PeerKey, Reachability, RouteKind, Session, Transport};

/// Every await in these tests is bounded. A transport bug usually shows up as a
/// hang, and a hung test that eventually gets killed by CI tells you nothing;
/// a timeout names the step that stopped.
const PATIENCE: Duration = Duration::from_secs(10);

async fn within<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(PATIENCE, future).await {
        Ok(value) => value,
        Err(_) => panic!("{what} did not finish within {PATIENCE:?}"),
    }
}

async fn bind() -> Transport {
    Transport::bind(&Identity::generate(), Reachability::LocalOnly)
        .await
        .expect("binding a local endpoint")
}

/// The address to dial for an endpoint in this same process.
///
/// Built from the bound sockets rather than [`Transport::local_address`],
/// because that reports every address the machine has: a LAN address, and on
/// this developer's machine a Tailscale address too. Loopback is the only one
/// guaranteed to be routable to ourselves and the only one that keeps the test
/// from depending on the network it happens to run on.
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

/// Dial `host` from `client` and return both ends of the same connection.
async fn connected(host: &Transport, client: &Transport) -> (Session, Session) {
    let accepting = {
        let host = host.clone();
        tokio::spawn(async move {
            host.accept()
                .await
                .expect("endpoint closed while accepting")
        })
    };

    let from_client = within("the dial", client.connect(&loopback(host)))
        .await
        .expect("connecting over loopback");
    let from_host = within("the accept", accepting)
        .await
        .expect("the accept task panicked")
        .expect("accepting the incoming connection");

    (from_host, from_client)
}

#[tokio::test]
async fn a_handshake_proves_both_device_identities() {
    // The core guarantee the rest of Pravera is built on: completing a QUIC
    // handshake with a peer means that peer holds the private key for the
    // device ID shown in the interface. No certificates, no enrolment.
    let host = bind().await;
    let client = bind().await;
    let (host_side, client_side) = connected(&host, &client).await;

    assert_eq!(client_side.peer_key(), host.peer_key());
    assert_eq!(host_side.peer_key(), client.peer_key());
    assert_eq!(client_side.peer_device_id(), host.device_id());
    assert_eq!(host_side.peer_device_id(), client.device_id());
    assert_ne!(host.device_id(), client.device_id());
}

#[tokio::test]
async fn the_right_address_with_the_wrong_key_does_not_connect() {
    // Addresses are hints supplied by discovery, which may be stale or hostile.
    // Identity is the key. Dialling the host's real socket while expecting a
    // different device must fail rather than quietly connect to whoever is
    // actually listening there.
    let host = bind().await;
    let client = bind().await;

    let _listening = {
        let host = host.clone();
        tokio::spawn(async move { host.accept().await })
    };

    let impostor = PeerKey::from_bytes(Identity::generate().public_key());
    let mut wrong = loopback(&host);
    assert_ne!(impostor, wrong.key, "the test needs two different keys");
    wrong.key = impostor;

    let outcome = within("the misdirected dial", client.connect(&wrong)).await;
    assert!(
        outcome.is_err(),
        "connected to a machine that does not hold the expected key"
    );
}

#[tokio::test]
async fn the_control_stream_carries_the_handshake_in_order() {
    let host = bind().await;
    let client = bind().await;
    let (host_side, client_side) = connected(&host, &client).await;

    // Dropping the last Session handle closes the QUIC connection, discarding
    // anything the peer has not read yet. The host task below owns one and ends
    // as soon as it has replied, so the test holds a second handle to keep the
    // connection alive until the client has actually read the welcome. A real
    // host keeps the session in its session table for the same reason.
    let _keepalive = host_side.clone();

    let host_task = tokio::spawn(async move {
        let mut control = host_side
            .accept_control()
            .await
            .expect("accepting the control stream");
        let opening = control.recv().await.expect("reading the first message");
        control
            .send(&HostMessage::Welcome(Welcome {
                version: pravera_proto::VERSION,
                host_name: "workshop".into(),
                codecs: vec![Codec::H264],
            }))
            .await
            .expect("sending the welcome");
        opening
    });

    let mut control = client_side
        .open_control()
        .await
        .expect("opening the control stream");

    let hello = ClientMessage::Hello(Hello {
        version: pravera_proto::VERSION,
        client_name: "laptop".into(),
        codecs: vec![Codec::H265, Codec::H264],
    });
    let reply = within("the handshake exchange", control.request(&hello))
        .await
        .expect("the host should answer a hello");

    let seen_by_host = within("the host task", host_task)
        .await
        .expect("the host task panicked");
    assert_eq!(seen_by_host, hello, "the host read a different message");

    match reply {
        HostMessage::Welcome(welcome) => {
            assert_eq!(welcome.version, pravera_proto::VERSION);
            assert_eq!(welcome.host_name, "workshop");
        }
        other => panic!("expected a welcome, got {other:?}"),
    }
}

#[tokio::test]
async fn several_control_messages_arrive_in_the_order_they_were_sent() {
    // The control stream is reliable and ordered, and input events depend on
    // that: a click that arrives before the pointer move that positioned it
    // lands somewhere else entirely.
    let host = bind().await;
    let client = bind().await;
    let (host_side, client_side) = connected(&host, &client).await;

    let sent: Vec<ClientMessage> = (0..64).map(|nonce| ClientMessage::Ping { nonce }).collect();

    let host_task = tokio::spawn(async move {
        let mut control = host_side.accept_control().await.expect("control stream");
        let mut received = Vec::new();
        for _ in 0..64 {
            received.push(control.recv().await.expect("reading a message"));
        }
        received
    });

    let mut control = client_side.open_control().await.expect("control stream");
    for message in &sent {
        control.send(message).await.expect("sending");
    }

    let received = within("the host reading 64 messages", host_task)
        .await
        .expect("the host task panicked");
    assert_eq!(received, sent);
}

#[tokio::test]
async fn a_media_frame_crosses_as_datagrams_and_reassembles() {
    let host = bind().await;
    let client = bind().await;
    let (host_side, client_side) = connected(&host, &client).await;

    // The chunk size the protocol picked has to actually fit inside what QUIC
    // will carry. This assertion is why SAFE_DATAGRAM is 1100 and not 1200:
    // the first version of this test measured 1162 here, and every media send
    // would have failed at runtime.
    let limit = host_side
        .max_datagram_size()
        .expect("a loopback path must carry datagrams");
    assert!(
        SAFE_DATAGRAM <= limit,
        "the protocol budgets {SAFE_DATAGRAM} byte datagrams but the path carries only {limit}"
    );
    assert!(host_side.max_chunk_payload().unwrap() >= MAX_CHUNK_PAYLOAD);

    let original: Vec<u8> = (0..MAX_CHUNK_PAYLOAD * 5 + 91)
        .map(|i| (i % 251) as u8)
        .collect();
    let meta = FrameMeta {
        frame_id: 1,
        capture_micros: 4_242,
        monitor: MonitorId::PRIMARY,
        flags: ChunkFlags::KEYFRAME,
    };
    let datagrams = split(meta, &original).expect("splitting the frame");
    assert_eq!(datagrams.len(), 6);

    for datagram in &datagrams {
        host_side
            .send_media(Bytes::from(datagram.clone()))
            .expect("sending a chunk");
    }

    let mut reassembler = Reassembler::new();
    let frame = within("the frame arriving", async {
        loop {
            let datagram = client_side.recv_media().await.expect("receiving a chunk");
            if let Some(frame) = reassembler.push(&datagram).expect("a well-formed chunk") {
                return frame;
            }
        }
    })
    .await;

    assert_eq!(frame.meta, meta);
    assert_eq!(frame.data, original);
    assert!(frame.is_keyframe());
    assert_eq!(reassembler.dropped_incomplete(), 0);
}

#[tokio::test]
async fn an_oversized_datagram_is_refused_before_it_reaches_the_network() {
    let host = bind().await;
    let client = bind().await;
    let (host_side, _client_side) = connected(&host, &client).await;

    let too_big = Bytes::from(vec![0u8; 64 * 1024]);
    let error = host_side
        .send_media(too_big)
        .expect_err("should be refused");
    assert!(
        matches!(
            error,
            pravera_transport::TransportError::DatagramTooLarge { .. }
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test]
async fn a_session_with_relays_disabled_is_never_reported_as_relayed() {
    // Reachability::LocalOnly promises no relays. If that promise were wrong,
    // the interface would show a relay badge for a cable connection, and worse,
    // traffic would be leaving the local network when the operator was told it
    // would not.
    let host = bind().await;
    let client = bind().await;
    let (_host_side, client_side) = connected(&host, &client).await;

    let route = within("a path being selected", async {
        loop {
            let route = client_side.route();
            if route.kind.is_some() {
                return route;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;

    assert_eq!(route.kind, Some(RouteKind::Direct));
    assert!(route.is_direct());
    assert!(
        route.rtt.is_some(),
        "a selected path must report a measured round trip, not a guess"
    );
}

#[tokio::test]
async fn a_peer_hanging_up_is_reported_as_a_closed_stream_not_a_failure() {
    // Distinguishing "the session ended" from "the session broke" is what keeps
    // an ordinary disconnect out of the error log.
    let host = bind().await;
    let client = bind().await;
    let (host_side, client_side) = connected(&host, &client).await;

    let host_task = tokio::spawn(async move {
        let mut control = host_side.accept_control().await.expect("control stream");
        let _ = control.recv().await.expect("the opening message");
        control.finish().expect("finishing the send side");
        // Hold the session open so the client sees a finished stream rather
        // than a dropped connection.
        tokio::time::sleep(Duration::from_millis(500)).await;
    });

    let mut control = client_side.open_control().await.expect("control stream");
    control
        .send(&ClientMessage::Ping { nonce: 1 })
        .await
        .expect("sending");

    let error = within("the close being noticed", control.recv())
        .await
        .expect_err("the host sent nothing, so this must not succeed");
    assert!(
        matches!(error, pravera_transport::TransportError::StreamClosed),
        "a clean hangup was reported as {error:?}"
    );

    host_task.await.expect("the host task panicked");
}
