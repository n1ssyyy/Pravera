//! Moving real files between two processes over a real QUIC connection.
//!
//! The unit tests in `pravera-files` prove the disk half and the ones in
//! `pravera-host::files` prove the permission half. Neither proves that a file
//! survives the trip. This does: a host serving its own filesystem, a client
//! dialling it over loopback, and a byte-for-byte comparison at the other end.
//!
//! It also pins the two rules that only exist once both ends are real:
//!
//! - **Permissions are enforced on the host.** A role without `FILE_READ`
//!   is refused by the host, not by a client that chose to be polite.
//! - **The bulk streams do not disturb the control stream.** A transfer runs
//!   while input and pings continue, which is the whole reason files were kept
//!   off the control stream.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pravera_client::{Client, ClientConfig};
use pravera_core::{Codec, Permission, Resolution};
use pravera_crypto::Identity;
use pravera_host::{HostConfig, HostSession, MemoryStore, SessionHooks};
use pravera_proto::{FileError, Location, Monitor, MonitorId, SessionConfig};
use pravera_transport::{PeerAddress, Reachability, Transport};

const PATIENCE: Duration = Duration::from_secs(30);

async fn within<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(PATIENCE, future).await {
        Ok(value) => value,
        Err(_) => panic!("{what} did not finish within {PATIENCE:?}"),
    }
}

/// A host that does nothing with the session hooks. Files never reach them —
/// they are served straight from the filesystem by `pravera_host::serve_files`
/// — so there is nothing here to record.
#[derive(Clone, Default)]
struct Idle;

impl SessionHooks for Idle {
    fn inject(&mut self, _: pravera_proto::InputEvent) {}
    fn stream(&mut self, _: &SessionConfig) {}
    fn keyframe(&mut self) {}
}

/// A directory that removes itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let path = std::env::temp_dir().join(format!("pravera-e2e-{name}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a scratch directory");
        Scratch(path)
    }
    fn at(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }
    fn write(&self, name: &str, bytes: &[u8]) -> String {
        let path = self.at(name);
        std::fs::write(&path, bytes).expect("write");
        path
    }
    fn path(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn store() -> MemoryStore {
    let mut store = MemoryStore::new();
    // Only `admin` holds FILE_READ and FILE_WRITE. That is the point of having
    // both accounts here.
    store.add("archivist", "keep-it-all", "admin").unwrap();
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

/// A logged-in client and the endpoints keeping it alive.
struct Linked {
    client: Client,
    _host: Transport,
    _client: Transport,
}

async fn log_in_as(username: &str, password: &str) -> Linked {
    let host_transport = Transport::bind(&Identity::generate(), Reachability::LocalOnly)
        .await
        .expect("binding the host");
    let client_transport = Transport::bind(&Identity::generate(), Reachability::LocalOnly)
        .await
        .expect("binding the client");
    let address = loopback(&host_transport);

    let config = Arc::new(HostConfig {
        host_name: "archive".into(),
        codecs: vec![Codec::H264],
        monitors: vec![Monitor {
            id: MonitorId::PRIMARY,
            name: "primary".into(),
            resolution: Resolution::new(1920, 1080),
            position: (0, 0),
            scale: 1.0,
            primary: true,
        }],
        ..HostConfig::default()
    });

    {
        let transport = host_transport.clone();
        tokio::spawn(async move {
            let session = transport
                .accept()
                .await
                .expect("the endpoint closed before a client arrived")
                .expect("accepting the connection");
            let mut host = HostSession::new(config, store(), session.peer_key());
            let mut hooks = Idle;
            pravera_host::serve(&session, &mut host, &mut hooks).await
        });
    }

    let mut client = within(
        "the dial",
        Client::connect(&client_transport, &address, &ClientConfig::default()),
    )
    .await
    .expect("connecting over loopback");

    within("the login", client.authenticate(username, password))
        .await
        .expect("logging in");

    Linked {
        client,
        _host: host_transport,
        _client: client_transport,
    }
}

// -------------------------------------------------------------------- tests

#[tokio::test]
async fn a_file_survives_the_round_trip_in_both_directions() {
    let scratch = Scratch::new("round-trip");
    // Larger than one chunk and not a multiple of it, so the last partial
    // read is exercised rather than assumed.
    let original: Vec<u8> = (0..200_017).map(|n| (n % 251) as u8).collect();
    let on_the_host = scratch.write("original.bin", &original);

    let linked = log_in_as("archivist", "keep-it-all").await;
    let session = linked.client.session();

    let down = scratch.at("downloaded.bin");
    let mut seen = Vec::new();
    let moved = within(
        "the download",
        pravera_client::files::download(session, &on_the_host, &down, false, |progress| {
            seen.push(progress)
        }),
    )
    .await
    .expect("downloading");

    assert_eq!(moved, original.len() as u64);
    assert_eq!(std::fs::read(&down).expect("the copy"), original);

    // Progress is reported as it goes rather than once at the end. A bar that
    // only moves once is not a bar.
    assert!(seen.len() > 3, "reported {} times", seen.len());
    assert_eq!(seen.first().map(|p| p.moved), Some(0));
    assert_eq!(seen.last().map(|p| p.moved), Some(original.len() as u64));
    assert!(seen.iter().all(|p| p.total == original.len() as u64));

    // And back up, to a new name.
    let up = scratch.at("uploaded.bin");
    let moved = within(
        "the upload",
        pravera_client::files::upload(session, &down, &up, false, |_| {}),
    )
    .await
    .expect("uploading");

    assert_eq!(moved, original.len() as u64);
    assert_eq!(std::fs::read(&up).expect("the copy back"), original);
}

#[tokio::test]
async fn an_empty_file_makes_the_trip_too() {
    // No body at all between the header and the hash. The one length where an
    // off-by-one in the read loop would hang rather than fail.
    let scratch = Scratch::new("empty");
    let on_the_host = scratch.write("empty.bin", b"");
    let linked = log_in_as("archivist", "keep-it-all").await;

    let down = scratch.at("copy.bin");
    let moved = within(
        "the download",
        pravera_client::files::download(
            linked.client.session(),
            &on_the_host,
            &down,
            false,
            |_| {},
        ),
    )
    .await
    .expect("downloading an empty file");

    assert_eq!(moved, 0);
    assert!(PathBuf::from(&down).is_file());
}

#[tokio::test]
async fn a_listing_arrives_with_what_is_actually_in_the_directory() {
    let scratch = Scratch::new("listing");
    scratch.write("beta.txt", b"b");
    scratch.write("alpha.txt", b"aa");
    std::fs::create_dir(scratch.at("subfolder")).expect("a subdirectory");

    let linked = log_in_as("archivist", "keep-it-all").await;
    let listing = within(
        "the listing",
        pravera_client::files::list(linked.client.session(), Location::Path(scratch.path())),
    )
    .await
    .expect("listing");

    let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["subfolder", "alpha.txt", "beta.txt"]);
    assert_eq!(listing.entries[1].size, 2);
    assert!(!listing.truncated);
    // The host joins the path, because the separator belongs to its filesystem.
    assert_eq!(
        listing.path_of(&listing.entries[1]),
        scratch.at("alpha.txt")
    );
}

#[tokio::test]
async fn the_places_view_offers_somewhere_to_start() {
    // A client cannot guess a host's drive letters or user name, so this is
    // the only way into the filesystem that does not require already knowing
    // a path.
    let linked = log_in_as("archivist", "keep-it-all").await;
    let listing = within(
        "the places",
        pravera_client::files::list(linked.client.session(), Location::Places),
    )
    .await
    .expect("listing places");

    assert!(listing.path.is_none());
    assert!(!listing.entries.is_empty(), "nowhere to start browsing");
    for entry in &listing.entries {
        assert!(entry.path.is_some(), "a place must carry its own path");
        assert!(entry.kind.is_directory());
    }
}

#[tokio::test]
async fn an_operator_is_refused_the_hosts_files_by_the_host() {
    // The client was not patched to be polite: it asks, and the host says no.
    // This is the check that a modified client cannot get past.
    let scratch = Scratch::new("refused");
    let secret = scratch.write("secret.txt", b"not for you");

    let linked = log_in_as("driver", "drive-it").await;
    assert!(
        !linked.client.permissions().contains(Permission::FILE_READ),
        "the test account was supposed to lack FILE_READ"
    );
    let session = linked.client.session();

    let listed = within(
        "the listing",
        pravera_client::files::list(session, Location::Path(scratch.path())),
    )
    .await;
    assert!(
        matches!(
            listed,
            Err(pravera_client::ClientError::File(FileError::NotPermitted))
        ),
        "an operator listed the host's files: {listed:?}"
    );

    let down = scratch.at("stolen.txt");
    let downloaded = within(
        "the download",
        pravera_client::files::download(session, &secret, &down, false, |_| {}),
    )
    .await;
    assert!(
        matches!(
            downloaded,
            Err(pravera_client::ClientError::File(FileError::NotPermitted))
        ),
        "an operator downloaded from the host: {downloaded:?}"
    );
    assert!(
        !PathBuf::from(&down).exists(),
        "a refused download still made a file"
    );

    let mine = scratch.write("mine.txt", b"hello");
    let uploaded = within(
        "the upload",
        pravera_client::files::upload(session, &mine, &scratch.at("dropped.txt"), false, |_| {}),
    )
    .await;
    assert!(
        matches!(
            uploaded,
            Err(pravera_client::ClientError::File(FileError::NotPermitted))
        ),
        "an operator wrote to the host: {uploaded:?}"
    );
}

#[tokio::test]
async fn a_path_that_walks_upwards_is_refused_over_the_wire() {
    // The client's own check is not the one that counts — a patched client
    // would simply not run it. This asks the host directly.
    let linked = log_in_as("archivist", "keep-it-all").await;
    let session = linked.client.session();

    let listed = within(
        "the listing",
        pravera_client::files::list(session, Location::Path("../../etc".into())),
    )
    .await;
    assert!(
        matches!(
            listed,
            Err(pravera_client::ClientError::File(FileError::Malformed))
        ),
        "a relative path was accepted: {listed:?}"
    );
}

#[tokio::test]
async fn a_file_already_there_is_not_replaced_unless_asked() {
    let scratch = Scratch::new("replace");
    let source = scratch.write("new.txt", b"the new one");
    let target = scratch.write("old.txt", b"the old one");

    let linked = log_in_as("archivist", "keep-it-all").await;
    let session = linked.client.session();

    let refused = within(
        "the upload",
        pravera_client::files::upload(session, &source, &target, false, |_| {}),
    )
    .await;
    assert!(
        matches!(
            refused,
            Err(pravera_client::ClientError::File(FileError::Exists))
        ),
        "an upload overwrote a file without being asked: {refused:?}"
    );
    assert_eq!(std::fs::read(&target).expect("still there"), b"the old one");

    within(
        "the upload",
        pravera_client::files::upload(session, &source, &target, true, |_| {}),
    )
    .await
    .expect("replacing when asked");
    assert_eq!(std::fs::read(&target).expect("replaced"), b"the new one");
}

#[tokio::test]
async fn a_missing_file_is_refused_without_naming_anything() {
    let scratch = Scratch::new("missing");
    let linked = log_in_as("archivist", "keep-it-all").await;

    let error = within(
        "the download",
        pravera_client::files::download(
            linked.client.session(),
            &scratch.at("nothing.bin"),
            &scratch.at("copy.bin"),
            false,
            |_| {},
        ),
    )
    .await
    .expect_err("no such file");

    assert!(matches!(
        error,
        pravera_client::ClientError::File(FileError::NotFound)
    ));
    let shown = error.to_string();
    assert!(!shown.contains("nothing.bin"), "{shown}");
    assert!(!shown.contains(&scratch.path()), "{shown}");
}

#[tokio::test]
async fn a_transfer_does_not_hold_up_the_control_stream() {
    // The whole reason files were kept off the control stream. A copy large
    // enough to take many round trips runs while pings keep being answered.
    let scratch = Scratch::new("concurrent");
    let big = vec![0x5au8; 4 * 1024 * 1024];
    let on_the_host = scratch.write("big.bin", &big);

    let mut linked = log_in_as("archivist", "keep-it-all").await;
    let session = linked.client.session().clone();
    let down = scratch.at("big-copy.bin");

    let transfer = tokio::spawn(async move {
        pravera_client::files::download(&session, &on_the_host, &down, false, |_| {}).await
    });

    // Answered while the transfer is in flight. If files rode the control
    // stream these would queue behind four megabytes and time out.
    let mut answered = 0;
    while !transfer.is_finished() {
        within("a ping", linked.client.ping())
            .await
            .expect("the control stream stayed usable during a transfer");
        answered += 1;
        if answered > 200 {
            break;
        }
    }

    let moved = within("the transfer", transfer)
        .await
        .expect("the transfer task")
        .expect("downloading");
    assert_eq!(moved, big.len() as u64);
    assert!(answered > 0, "the transfer finished before a single ping");
    assert_eq!(std::fs::read(scratch.at("big-copy.bin")).unwrap(), big);
}
