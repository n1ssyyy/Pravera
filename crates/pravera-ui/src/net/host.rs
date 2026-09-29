//! Answering sessions other machines start.
//!
//! Pravera is peer to peer: the same application both reaches out and accepts.
//! This is the accepting half — an endpoint bound to this machine's device key,
//! an accept loop, and one [`Agent`] per connection tying capture, encoding and
//! input injection together.
//!
//! ## Where this will not live forever
//!
//! Running the host inside the interface means it stops when the window
//! closes, cannot serve the lock screen, and cannot survive a reboot. That is
//! the honest limit of P1 and it is stated plainly in the interface rather than
//! implied away. P5 moves this into `pravera-service`, running as SYSTEM with a
//! session agent per desktop; the code here changes almost none of its shape
//! when it does, because everything below [`serve`] is already the real thing.
//!
//! ## Every connection is a stranger
//!
//! Accepting proves which *machine* dialled, and nothing more. The username,
//! the password and the permission check all happen afterwards inside
//! `pravera_host::serve`, and they happen identically whether the peer came
//! over a cable, a tailnet or the open internet. Nothing in this file decides
//! who may do what — it could not, and should not be able to.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use pravera_auth::Role;
use pravera_capture::CaptureSource;
use pravera_discovery::mdns;
use pravera_host::{monitors, Agent, FileStore, HostConfig, HostSession};
use pravera_transport::{PeerKey, Transport};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// What a machine with nothing plugged into its graphics output reports.
///
/// This is the thing that stops a headless box from working, and it is not a
/// Pravera limitation: with no monitor attached there is no desktop composited
/// anywhere for anything to capture, so Windows hands out an empty display
/// list. Every screen-sharing tool hits it, which is why some of them ship a
/// virtual display driver.
///
/// Written out in full because the person reading it is very likely sitting at
/// a different machine wondering why this one refuses.
pub const NO_DISPLAY: &str = "This machine has no screen to share. Hosting refuses \
     until one appears: plug in a monitor or an HDMI/DisplayPort dummy plug, or \
     let Pravera add its virtual display: install the Pravera service, or run \
     pravera.exe once as administrator.";

/// What the accepting half tells the interface.
#[derive(Debug, Clone)]
pub enum Event {
    /// A peer connected. Not yet authenticated — this is only "someone
    /// dialled", and it is shown as such.
    Arrived { device: String },
    /// A connection finished, however it finished.
    Left { device: String },
    /// The accept loop stopped. Terminal.
    Stopped(String),
}

/// The interface's handle on the accepting half.
///
/// Dropping it stops accepting. Existing sessions are dropped with it, which
/// is the correct behaviour for a host that lives in a window: closing the
/// window is a deliberate act, and a session that outlived its own interface
/// would be a session nobody could see or end.
pub struct Hosting {
    key: PeerKey,
    transport: Transport,
    connections: Arc<AtomicU64>,
    /// This machine, announced on the local subnet.
    ///
    /// Held here so it is withdrawn the moment hosting stops: a machine that
    /// has stopped accepting sessions must stop saying it accepts them.
    /// `None` when the subnet refused the advertisement, which is a reason to
    /// be harder to find and not a reason to refuse to host.
    advertisement: Option<mdns::Advertisement>,
    /// Ends the accept loop when dropped.
    _stop: mpsc::Sender<()>,
}

impl Hosting {
    /// This machine's connect code: what someone pastes in to reach it.
    pub fn code(&self) -> String {
        self.key.to_code()
    }

    pub fn device_id(&self) -> pravera_core::DeviceId {
        self.key.device_id()
    }

    /// How many peers are connected right now.
    pub fn connections(&self) -> u64 {
        self.connections.load(Ordering::Relaxed)
    }

    /// The addresses this endpoint is reachable at, for showing in Settings.
    pub fn addresses(&self) -> Vec<std::net::SocketAddr> {
        self.transport.bound_sockets()
    }

    /// Whether this machine is announcing itself on the local subnet.
    ///
    /// False means every other route still works but nobody will find this
    /// machine by looking — they will need its connect code. Worth saying,
    /// because "it isn't in the list" and "it isn't running" look identical
    /// from the other machine.
    pub fn listed_on_lan(&self) -> bool {
        self.advertisement.is_some()
    }
}

/// Where the accounts for this machine are kept.
pub fn accounts_path() -> Result<std::path::PathBuf, String> {
    pravera_core::paths::accounts_file()
        .map_err(|error| format!("Pravera has nowhere to keep its accounts: {error}"))
}

/// The accounts this machine already knows.
pub fn accounts() -> Result<FileStore, String> {
    FileStore::load(accounts_path()?).map_err(|error| error.to_string())
}

/// Save the account other machines will sign in with.
///
/// Written to disk rather than held in memory, because a machine that reboots
/// at three in the morning has nobody to type a password into it. The password
/// itself is not stored; its Argon2id hash is, which is what a login is checked
/// against and is not a credential anyone can replay.
pub fn save_account(username: &str, password: &str, role: &str) -> Result<FileStore, String> {
    if username.trim().is_empty() {
        return Err("Choose a username for people connecting to this machine.".into());
    }
    if password.is_empty() {
        return Err("Set a password. A host with no password is open to anyone who knows its                     connect code."
            .into());
    }

    let mut store = accounts()?;
    store
        .set(username.trim(), password, role)
        .map_err(|error| error.to_string())?;
    Ok(store)
}

/// How many displays this machine could share.
///
/// Asked once at startup rather than when hosting is switched on, because the
/// answer decides whether this machine can host at all. A person setting up a
/// machine with no monitor should find that out before they choose a password,
/// not after — and they are very likely reading the answer from somewhere else.
pub async fn displays() -> Result<usize, String> {
    let source = pravera_capture::source()
        .map_err(|error| format!("This machine's screen cannot be captured: {error}"))?;

    monitors(source.as_ref())
        .map(|screens| screens.len())
        .map_err(|error| format!("This machine's displays could not be read: {error}"))
}

/// The roles a person can be given when hosting is switched on.
pub fn roles() -> Vec<Role> {
    Role::builtins()
}

/// What this machine calls itself, for the other end to display.
///
/// A display hint and nothing more: the identity is the ed25519 key the TLS
/// handshake proves, and the far end treats this string as untrusted text. It
/// is capped here anyway, because a name long enough to fill a panel is a name
/// that breaks the layout it lands in.
pub fn machine_name() -> String {
    let raw = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|name| name.trim().to_string())
        })
        .unwrap_or_default();

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "Pravera host".to_string();
    }
    trimmed.chars().take(48).collect()
}

/// Start accepting sessions.
///
/// Takes the endpoint this machine already owns rather than binding one, so
/// that the connect code shown here is the same key a session dialled from
/// this window presents at the far end. One machine, one identity.
pub async fn start(
    transport: Transport,
    store: FileStore,
    host_name: String,
) -> Result<(Hosting, mpsc::UnboundedReceiver<Event>), String> {
    if store.is_empty() {
        return Err(
            "Set up an account first. Nobody can sign in to a machine with no \
                    accounts on it."
                .into(),
        );
    }

    let source: Arc<dyn CaptureSource> = pravera_capture::source()
        .map_err(|error| format!("This machine's screen cannot be captured: {error}"))?
        .into();

    // A machine with no real screen gets its virtual display *before* the
    // monitor list is taken, so the list offered to every client is the one
    // that will actually be streamed. Blocking work — a first install creates a
    // device and waits for its monitor — so it runs off the async threads.
    let virtual_display = tokio::task::spawn_blocking(|| {
        pravera_capture::needs_virtual_display()
            .then(|| pravera_capture::ensure_virtual_display(1920, 1080, 60))
    })
    .await
    .map_err(|error| format!("Checking this machine's displays failed: {error}"))?;
    if let Some(Err(error)) = virtual_display {
        // Only placeholders (or nothing) are left, and they stream black.
        warn!(%error, state = %pravera_capture::virtual_display_diagnostics(), "no virtual display");
        return Err(format!("{NO_DISPLAY}\n\n{error}"));
    }

    let screens = monitors(source.as_ref())
        .map_err(|error| format!("This machine's displays could not be read: {error}"))?;
    if screens.is_empty() {
        return Err(NO_DISPLAY.into());
    }

    let key = transport.peer_key();

    // Kept before the config takes ownership: the advertisement shows the same
    // name a connected client sees, because two names for one machine is two
    // machines as far as anybody reading the screen is concerned.
    let name_for_advert = host_name.clone();

    let config = Arc::new(HostConfig {
        host_name,
        // Only what there is really an encoder for. A host that advertises a
        // codec it cannot produce negotiates a session that then shows
        // nothing, and the failure surfaces at the far end of the pipeline.
        codecs: pravera_codec::encodable(),
        // Empty on a platform with no loopback tap, so a client is never
        // promised sound this machine has no way to produce.
        audio_codecs: pravera_audio::capturable(),
        monitors: screens,
        ..HostConfig::default()
    });

    let (events, inbox) = mpsc::unbounded_channel();
    let (stop, stopped) = mpsc::channel(1);
    let connections = Arc::new(AtomicU64::new(0));

    info!(
        device = %key.device_id(),
        displays = config.monitors.len(),
        codecs = ?config.codecs,
        "accepting sessions"
    );

    tokio::spawn(accept(
        transport.clone(),
        config,
        Arc::new(store),
        source,
        events,
        stopped,
        connections.clone(),
    ));

    // Announced only now, at the end, once there is genuinely something to
    // announce. Publishing before the capture source and the accounts were
    // checked would put this machine in other people's device lists and then
    // refuse everyone who clicked it.
    let advertisement = advertise(&transport, key, &name_for_advert);

    Ok((
        Hosting {
            key,
            transport,
            connections,
            advertisement,
            _stop: stop,
        },
        inbox,
    ))
}

/// Put this machine on the local subnet, if the subnet will have it.
///
/// The port is the transport's own UDP port, so a client that finds this
/// advertisement dials the socket the endpoint is already listening on rather
/// than going out to a relay to come back to the same building.
fn advertise(transport: &Transport, key: PeerKey, name: &str) -> Option<mdns::Advertisement> {
    let sockets = transport.bound_sockets();
    let port = sockets
        .iter()
        .find(|addr| addr.is_ipv4())
        .or_else(|| sockets.first())
        .map(|addr| addr.port())?;

    match mdns::Advertisement::publish(name, &key.to_code(), port, pravera_proto::VERSION) {
        Ok(advert) => {
            info!(port, "listed on the local subnet");
            Some(advert)
        }
        // Not fatal, and not the person's problem to solve right now: every
        // other way of reaching this machine still works, and the connect code
        // is on screen.
        Err(error) => {
            warn!(%error, "this machine will not be listed on the local subnet");
            None
        }
    }
}

/// The accept loop. Ends when `stopped` closes or the endpoint does.
#[allow(clippy::too_many_arguments)]
async fn accept(
    transport: Transport,
    config: Arc<HostConfig>,
    store: Arc<FileStore>,
    source: Arc<dyn CaptureSource>,
    events: mpsc::UnboundedSender<Event>,
    mut stopped: mpsc::Receiver<()>,
    connections: Arc<AtomicU64>,
) {
    loop {
        let incoming = tokio::select! {
            // `recv` resolves with `None` the moment the sender is dropped,
            // which is what closing the window does.
            _ = stopped.recv() => {
                debug!("hosting was switched off");
                return;
            }
            incoming = transport.accept() => incoming,
        };

        let session = match incoming {
            Some(Ok(session)) => session,
            // One failed handshake is one peer with a dropped packet or a
            // different protocol. Ending the loop over it would take the
            // machine offline for everyone else.
            Some(Err(error)) => {
                debug!(%error, "a connection failed before it began");
                continue;
            }
            None => {
                let _ = events.send(Event::Stopped(
                    "The network endpoint closed. Switch hosting off and on to try again.".into(),
                ));
                return;
            }
        };

        let device = session.peer_device_id().to_string();
        info!(peer = %device, "a peer connected");
        connections.fetch_add(1, Ordering::Relaxed);
        if events
            .send(Event::Arrived {
                device: device.clone(),
            })
            .is_err()
        {
            // The interface is gone, so there is nobody to serve for.
            return;
        }

        let (config, store, source, events, connections) = (
            config.clone(),
            store.clone(),
            source.clone(),
            events.clone(),
            connections.clone(),
        );
        tokio::spawn(async move {
            serve(session, config, store, source).await;
            connections.fetch_sub(1, Ordering::Relaxed);
            let _ = events.send(Event::Left { device });
        });
    }
}

/// Run one connection to completion.
///
/// Everything a peer is allowed to do is decided inside `pravera_host::serve`,
/// against the role the login granted. Nothing here decides who may do what.
async fn serve(
    session: pravera_transport::Session,
    config: Arc<HostConfig>,
    store: Arc<FileStore>,
    source: Arc<dyn CaptureSource>,
) {
    let mut agent = match Agent::new(session.clone(), source) {
        Ok(agent) => agent,
        Err(error) => {
            // Serving the picture anyway and dropping every input event would
            // be the friendlier-looking choice and the worse one: the client
            // would show a granted `CONTROL`, and clicks would vanish with no
            // explanation. Saying so up front is the honest failure.
            //
            // Running view-only properly means the login's granted permissions
            // reflecting what this host can actually do, which is a change to
            // the grant in `pravera-host` and lands with the Linux input
            // backend in P3. Until then, this.
            warn!(%error, "no input backend on this machine; refusing the session");
            session.close("this host cannot accept input");
            return;
        }
    };

    let mut host = HostSession::new(config, store, session.peer_key());
    if let Err(error) = pravera_host::serve(&session, &mut host, &mut agent).await {
        debug!(%error, peer = %session.peer_device_id(), "a session ended badly");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosting_without_a_password_is_refused_and_says_why() {
        // A machine whose screen and keyboard are reachable by anyone holding
        // a public string is not a state to slip into by leaving a field
        // blank. Checked before anything touches the disk.
        let error = save_account("me", "", "operator").expect_err("an empty password");
        assert!(error.to_lowercase().contains("password"), "{error}");
        assert!(error.to_lowercase().contains("open to anyone"), "{error}");
    }

    #[test]
    fn hosting_without_a_username_is_refused() {
        assert!(save_account("", "hunter2", "operator").is_err());
        assert!(save_account("   ", "hunter2", "operator").is_err());
    }

    #[test]
    fn the_message_for_a_machine_with_no_screen_says_what_to_do_about_it() {
        // The failure a headless box actually hits. "No displays" alone reads
        // as a bug in Pravera, and the person reading it is at another
        // machine with no way to investigate this one. The message must say
        // hosting refuses and what brings a real display.
        assert!(NO_DISPLAY.contains("dummy plug"), "{NO_DISPLAY}");
        assert!(NO_DISPLAY.contains("virtual display"), "{NO_DISPLAY}");
        assert!(
            !NO_DISPLAY.contains("test pattern"),
            "hosting must refuse, never stream a test pattern: {NO_DISPLAY}"
        );
    }

    #[test]
    fn the_offered_roles_are_the_built_in_ones() {
        let offered = roles();
        assert!(!offered.is_empty());

        let names: Vec<&str> = offered.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"viewer"), "{names:?}");
        assert!(names.contains(&"operator"), "{names:?}");
        assert!(names.contains(&"admin"), "{names:?}");
    }

    #[test]
    fn this_machine_always_has_a_name_to_show() {
        // An empty name would leave the other end's toolbar with a blank where
        // the machine should be.
        let name = machine_name();
        assert!(!name.trim().is_empty());
        assert!(name.chars().count() <= 48, "{name}");
    }

    #[test]
    fn a_host_only_advertises_codecs_it_can_encode() {
        // Claiming one it cannot produce negotiates a session that shows
        // nothing at all, which is far harder to diagnose than a refusal.
        // Encodable rather than decodable: this list is the host's half of the
        // handshake, and every machine can *decode* hardware H.264 whether or
        // not it owns an encoder.
        let encodable = pravera_codec::encodable();
        assert!(!encodable.is_empty());
        assert!(
            encodable.contains(&pravera_core::Codec::OpenH264),
            "the software floor must always be there to fall back to"
        );
    }
}
