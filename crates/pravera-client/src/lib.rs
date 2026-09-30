//! The client side of a Pravera session.
//!
//! Dial a host, say hello, log in, ask for a stream, and from then on: send
//! input on the control stream, take decoded pictures out of [`VideoStream`].
//!
//! ## This crate enforces nothing
//!
//! [`AuthResult::Granted`] carries a permission set and [`Client::permissions`]
//! hands it back, but that set is a **display hint** — it exists so the UI can
//! grey out a button rather than offer something that will be refused. Every
//! request is checked again by `pravera-host` against its own record of the
//! role. Patching this crate to claim more permissions changes what the buttons
//! look like and nothing else.
//!
//! That is the whole reason the check lives on the host: the client is the part
//! an attacker controls.
//!
//! ## A connection is not a login
//!
//! The QUIC handshake proves which *machine* answered, because iroh uses the
//! device's ed25519 key as the TLS identity. It proves nothing about who is
//! sitting at it, and grants nothing. Username and password are a separate
//! conversation, and they happen the same way over a cable, a tailnet, or the
//! open internet.

pub mod audio;
pub mod cursor;
mod error;
pub mod files;
pub mod terminal;
mod video;

pub use audio::{AudioSink, AudioStats, AudioStream};
pub use cursor::{CursorImage, CursorState, CursorStream};
pub use error::{ClientError, Result};
pub use files::Progress;
pub use terminal::{Terminal, TerminalEvent};
pub use video::{VideoStats, VideoStream};

use std::time::Duration;

use pravera_core::{for_log, AudioFormat, Codec, Permission, QualityProfile, Resolution};
use pravera_proto::{
    AuthResult, ClientMessage, ClipboardSeq, ClipboardUpdate, Credentials, Hello, HostMessage,
    InputEvent, Monitor, MonitorId, SessionConfig, SessionRequest, Welcome, CURSOR_VERSION,
    MAX_CLIPBOARD_BYTES,
};
use pravera_transport::{ClientControl, PeerAddress, Session, Transport};
use tracing::{debug, info, warn};

/// How this client introduces itself.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Shown in the host's session list. A hint with no authority — it is
    /// whatever this end chose to type, and the host treats it as such.
    pub client_name: String,
    /// Decoders this client actually has, best first.
    ///
    /// Claiming one that is not really there produces a session that negotiates
    /// successfully and then shows nothing, so this is filled from
    /// [`pravera_codec::decodable`] rather than from a wish list.
    ///
    /// Decodable, not encodable. A client with no GPU encoder of its own can
    /// still decode a stream from a host that has one, and saying otherwise
    /// would drag that host down to software encoding to match a limitation
    /// this side does not have.
    pub codecs: Vec<Codec>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            client_name: hostname(),
            codecs: pravera_codec::decodable(),
        }
    }
}

fn hostname() -> String {
    // Best effort. A machine with no name in the environment is still perfectly
    // able to hold a session; it just shows up generically in the host's list.
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "Pravera client".into())
}

/// What the host granted at login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub username: String,
    pub role: String,
    /// A display hint. See the crate documentation.
    pub permissions: Permission,
}

/// A live connection to a host.
///
/// Holds the control stream, so it must be kept alive for as long as the
/// session is meant to last: dropping the last handle to the underlying QUIC
/// connection closes it and discards anything still in flight.
pub struct Client {
    pub(crate) session: Session,
    pub(crate) control: ClientControl,
    welcome: Welcome,
    grant: Option<Grant>,
    streaming: Option<SessionConfig>,
    /// Rises for every ping, so a reply can be matched to its request.
    nonce: u64,
}

impl Client {
    /// Dial a host and complete the version handshake.
    ///
    /// Stops short of logging in: the caller decides what to do with a
    /// [`Welcome`], and asking someone for a password before knowing the host
    /// is even speaking a compatible protocol wastes their time.
    pub async fn connect(
        transport: &Transport,
        peer: &PeerAddress,
        config: &ClientConfig,
    ) -> Result<Client> {
        let session = transport.connect(peer).await?;
        Client::greet(session, config).await
    }

    /// Complete the handshake on a session that is already established.
    ///
    /// Separate from [`Client::connect`] so a session obtained some other way —
    /// a direct-link dial, a test harness — takes the same path.
    pub async fn greet(session: Session, config: &ClientConfig) -> Result<Client> {
        // The version the handshake settled on, which may be older than this
        // build's newest: a host that has not been updated only speaks 2.
        let version = session.protocol_version();
        let mut control = session.open_control().await?;

        let reply = control
            .request(&ClientMessage::Hello(Hello {
                version,
                client_name: config.client_name.clone(),
                codecs: config.codecs.clone(),
            }))
            .await?;

        let welcome = match reply {
            HostMessage::Welcome(welcome) => welcome,
            other => return Err(refusal(other, "Welcome")),
        };

        if welcome.version != version {
            // The host should already have refused this, and says so through
            // `Failed` rather than `Welcome`. Checked anyway, because a client
            // that trusts a peer's framing to be well-behaved is a client that
            // does whatever a hostile peer wants.
            return Err(ClientError::VersionMismatch {
                ours: version,
                theirs: welcome.version,
            });
        }

        // Better to find out now than after someone has typed a password.
        if !config.codecs.iter().any(|c| welcome.codecs.contains(c)) {
            return Err(ClientError::NoSharedCodec {
                offered: welcome.codecs,
            });
        }

        info!(
            host = %for_log(&welcome.host_name),
            device = %session.peer_device_id(),
            route = ?session.route().kind,
            "connected"
        );

        Ok(Client {
            session,
            control,
            welcome,
            grant: None,
            streaming: None,
            nonce: 0,
        })
    }

    /// What the host said about itself. A display hint; the identity is the
    /// device key, which the TLS handshake already proved.
    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    /// The protocol version this connection speaks. Features newer than version
    /// 2 exist only when it says so.
    pub fn protocol_version(&self) -> u16 {
        self.session.protocol_version()
    }

    /// The host's cursor, if this connection carries one (protocol version 3).
    ///
    /// `None` means the host draws its cursor into the picture, as it always
    /// did, and the interface should show that and nothing more. Must be called
    /// inside a tokio runtime, once per connection.
    pub fn cursor(&self) -> Option<CursorStream> {
        (self.protocol_version() >= CURSOR_VERSION)
            .then(|| CursorStream::start(self.session.clone()))
    }

    /// The host's device ID, derived from the key it authenticated with.
    pub fn device_id(&self) -> pravera_core::DeviceId {
        self.session.peer_device_id()
    }

    /// How the packets are getting there.
    pub fn route(&self) -> pravera_transport::Route {
        self.session.route()
    }

    pub fn grant(&self) -> Option<&Grant> {
        self.grant.as_ref()
    }

    /// What the host said this login may do.
    ///
    /// Empty before logging in. A **hint**: see the crate documentation for why
    /// nothing may be decided from it.
    pub fn permissions(&self) -> Permission {
        self.grant
            .as_ref()
            .map_or(Permission::empty(), |grant| grant.permissions)
    }

    /// The configuration the host is currently streaming, if any.
    pub fn streaming(&self) -> Option<&SessionConfig> {
        self.streaming.as_ref()
    }

    /// Log in.
    ///
    /// A refusal is [`ClientError::Denied`] and says nothing further, because
    /// the host says nothing further — deliberately, so that a failed login
    /// cannot be used to work out which usernames exist.
    ///
    /// The host allows a small number of attempts and then drops the
    /// connection, so a caller must be prepared for the next call to fail at
    /// the transport instead.
    pub async fn authenticate(&mut self, username: &str, password: &str) -> Result<Grant> {
        let reply = self
            .control
            .request(&ClientMessage::Authenticate(Credentials {
                username: username.to_owned(),
                password: password.to_owned(),
            }))
            .await?;

        match reply {
            HostMessage::AuthResult(AuthResult::Granted {
                username,
                role,
                permissions,
            }) => {
                info!(%username, %role, ?permissions, "authenticated");
                let grant = Grant {
                    username,
                    role,
                    permissions,
                };
                self.grant = Some(grant.clone());
                Ok(grant)
            }
            HostMessage::AuthResult(AuthResult::Denied) => Err(ClientError::Denied),
            other => Err(refusal(other, "AuthResult")),
        }
    }

    /// Ask the host to start sending a display.
    ///
    /// Returns what the host actually agreed to, which may differ from the
    /// request in codec, resolution or profile. Render what this says, not what
    /// was asked for.
    pub async fn start_session(
        &mut self,
        monitor: MonitorId,
        profile: QualityProfile,
        max_resolution: Option<Resolution>,
        audio: bool,
    ) -> Result<SessionConfig> {
        self.require_login("starting a stream")?;
        let config = self
            .expect_session(ClientMessage::StartSession(SessionRequest {
                monitor,
                profile,
                max_resolution,
                audio,
            }))
            .await?;
        Ok(config)
    }

    /// Begin decoding what the host agreed to send.
    ///
    /// Separate from [`Client::start_session`] because the two have different
    /// lifetimes: a stream is torn down and rebuilt on every reconfiguration,
    /// while the client outlives all of them.
    pub fn video(&self, audio: Option<AudioSink>) -> Result<VideoStream> {
        let config = self
            .streaming
            .as_ref()
            .ok_or(ClientError::TooSoon("no stream has been started"))?;
        VideoStream::start(self.session.clone(), config.format, audio)
    }

    /// The audio stream the host agreed to send, if it agreed to any.
    ///
    /// `None` covers every reason there might be no sound; the host does not
    /// say which, and there is nothing different for the client to do in any
    /// of the cases.
    pub fn audio_format(&self) -> Option<AudioFormat> {
        self.streaming.as_ref().and_then(|config| config.audio)
    }

    /// Switch to another display. Needs `MULTI_MONITOR`.
    pub async fn select_monitor(&mut self, monitor: MonitorId) -> Result<SessionConfig> {
        self.require_login("switching displays")?;
        self.expect_session(ClientMessage::SelectMonitor(monitor))
            .await
    }

    /// Change the quality profile.
    pub async fn set_profile(&mut self, profile: QualityProfile) -> Result<SessionConfig> {
        self.require_login("changing the profile")?;
        self.expect_session(ClientMessage::SetProfile(profile))
            .await
    }

    /// Ask for the host's display list. Needs `MULTI_MONITOR`.
    pub async fn monitors(&mut self) -> Result<Vec<Monitor>> {
        self.require_login("listing displays")?;
        match self.control.request(&ClientMessage::ListMonitors).await? {
            HostMessage::Monitors(monitors) => Ok(monitors),
            other => Err(refusal(other, "Monitors")),
        }
    }

    /// Send one input event.
    ///
    /// Fire and forget: the host does not acknowledge input, because a
    /// round-trip per keystroke would put the network's latency between a key
    /// and the letter appearing.
    ///
    /// Malformed events are dropped here rather than sent. The host validates
    /// too — it has to, since it cannot trust this end — but there is no reason
    /// to spend a packet on something that will certainly be refused.
    pub async fn send_input(&mut self, event: InputEvent) -> Result<()> {
        self.require_login("sending input")?;
        if !event.is_well_formed() {
            warn!(?event, "dropped a malformed input event");
            return Ok(());
        }
        self.control.send(&ClientMessage::Input(event)).await?;
        Ok(())
    }

    /// Ask what the host's clipboard has done since `since`. Needs
    /// `CLIPBOARD_READ`.
    ///
    /// Polled rather than pushed. The control stream is strictly
    /// request-response, so a host that announced a clipboard change on its own
    /// initiative would drop that announcement between some other request and
    /// its answer, and every reply after it would be paired with the wrong
    /// question. Asking costs one small round trip on a timer and keeps the
    /// stream's one invariant intact.
    ///
    /// Pass back the sequence from the previous answer. A host whose clipboard
    /// has not moved replies [`ClipboardUpdate::Unchanged`], which is cheap
    /// enough that polling twice a second is unremarkable.
    pub async fn clipboard_since(&mut self, since: ClipboardSeq) -> Result<ClipboardUpdate> {
        self.require_login("reading the clipboard")?;
        match self
            .control
            .request(&ClientMessage::GetClipboard { since })
            .await?
        {
            HostMessage::Clipboard(update) => Ok(update),
            other => Err(refusal(other, "Clipboard")),
        }
    }

    /// Put `text` on the host's clipboard. Needs `CLIPBOARD_WRITE`.
    ///
    /// The returned sequence is what the host's clipboard reads *after* the
    /// write. Handing it to the next [`Client::clipboard_since`] is what stops
    /// the two machines copying the same text back and forth forever: without
    /// it the next poll reports this very paste as a change worth carrying
    /// home.
    ///
    /// Text longer than [`MAX_CLIPBOARD_BYTES`] is refused here rather than
    /// sent. The host refuses it too — it has to, since it cannot trust this
    /// end — but there is no reason to spend the bandwidth first.
    pub async fn set_clipboard(&mut self, text: &str) -> Result<ClipboardSeq> {
        self.require_login("writing the clipboard")?;
        if text.len() > MAX_CLIPBOARD_BYTES {
            warn!(
                bytes = text.len(),
                limit = MAX_CLIPBOARD_BYTES,
                "did not send a clipboard larger than the protocol carries"
            );
            return Err(ClientError::Refused(
                pravera_proto::ProtocolError::Malformed,
            ));
        }

        match self
            .control
            .request(&ClientMessage::SetClipboard {
                text: text.to_owned(),
            })
            .await?
        {
            HostMessage::ClipboardSet { seq } => Ok(seq),
            other => Err(refusal(other, "ClipboardSet")),
        }
    }

    /// Ask for a fresh keyframe, after loss has corrupted the picture.
    pub async fn request_keyframe(&mut self) -> Result<()> {
        self.require_login("requesting a keyframe")?;
        self.control.send(&ClientMessage::RequestKeyframe).await?;
        Ok(())
    }

    /// Ask the host to generate Ctrl+Alt+Del (Secure Attention Sequence).
    pub async fn send_sas(&mut self) -> Result<()> {
        self.require_login("sending SAS")?;
        match self
            .control
            .request(&ClientMessage::SendSas)
            .await?
        {
            HostMessage::SasSent => Ok(()),
            other => Err(refusal(other, "SasSent")),
        }
    }

    /// Measure the control round-trip.
    ///
    /// This is a real measurement of the reliable stream, which is not the same
    /// thing as media latency — media rides datagrams and is not retransmitted.
    /// It is reported as what it is, because a number in a UI that was not
    /// actually measured is worse than no number.
    pub async fn ping(&mut self) -> Result<Duration> {
        self.nonce = self.nonce.wrapping_add(1);
        let sent = self.nonce;
        let at = std::time::Instant::now();

        match self
            .control
            .request(&ClientMessage::Ping { nonce: sent })
            .await?
        {
            HostMessage::Pong { nonce } if nonce == sent => Ok(at.elapsed()),
            HostMessage::Pong { nonce } => {
                // The control stream is strictly ordered request-response, so
                // this cannot happen against a correct host. Refusing to
                // report a figure derived from the wrong reply is the point.
                debug!(sent, got = nonce, "a pong did not match its ping");
                Err(ClientError::Unexpected {
                    expected: "Pong",
                    got: "Pong",
                })
            }
            other => Err(refusal(other, "Pong")),
        }
    }

    /// Say goodbye and close.
    ///
    /// Consumes the client, because everything after this would fail. The
    /// goodbye is flushed before closing: a QUIC connection that closes with
    /// data still in flight discards it, and the last message is the one that
    /// explains why the session ended.
    pub async fn disconnect(mut self, reason: &str) -> Result<()> {
        let said = self
            .control
            .send(&ClientMessage::Goodbye {
                reason: reason.to_owned(),
            })
            .await;
        if let Err(error) = &said {
            debug!(%error, "could not say goodbye; closing anyway");
        }
        let _ = self.control.flush().await;
        self.session.close("client disconnected");
        Ok(())
    }

    /// The underlying session, for a caller that needs the connection itself.
    pub fn session(&self) -> &Session {
        &self.session
    }

    fn require_login(&self, what: &'static str) -> Result<()> {
        if self.grant.is_none() {
            return Err(ClientError::TooSoon(what));
        }
        Ok(())
    }

    /// Send something that should produce a `SessionStarted`, and remember it.
    async fn expect_session(&mut self, message: ClientMessage) -> Result<SessionConfig> {
        match self.control.request(&message).await? {
            HostMessage::SessionStarted(config) => {
                // `info`: "connected" and "authenticated" are `info`, and a
                // session starting is the key operator event joining them.
                info!(
                    monitor = config.monitor.0,
                    codec = config.format.codec.name(),
                    resolution = %config.format.resolution,
                    chroma = ?config.format.pixel_format,
                    profile = config.profile.name(),
                    "the host is streaming"
                );
                self.streaming = Some(config.clone());
                Ok(config)
            }
            other => Err(refusal(other, "SessionStarted")),
        }
    }
}

/// Turn an unexpected host message into the error that describes it.
///
/// `Failed` and `Goodbye` are answers in their own right, not surprises, so
/// they keep what the host actually said.
fn refusal(message: HostMessage, expected: &'static str) -> ClientError {
    match message {
        HostMessage::Failed(error) => ClientError::Refused(error),
        HostMessage::Goodbye { reason } => ClientError::Ended(reason),
        other => ClientError::Unexpected {
            expected,
            got: error::describe(&other),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_claims_only_the_codecs_it_can_actually_decode() {
        // Advertising a decoder that is not linked in negotiates a session
        // that then shows nothing at all, and the failure appears at the far
        // end of the pipeline where it is hardest to read.
        let config = ClientConfig::default();
        assert_eq!(config.codecs, pravera_codec::decodable());
        assert!(!config.codecs.is_empty());
    }

    #[test]
    fn a_client_has_no_permissions_before_logging_in() {
        // The set is a display hint, but "no session yet" must still read as
        // "nothing allowed" rather than as an empty-therefore-unrestricted.
        let grant: Option<Grant> = None;
        let permissions = grant.map_or(Permission::empty(), |g| g.permissions);
        assert!(permissions.is_empty());
    }

    #[test]
    fn a_host_that_says_the_wrong_thing_is_named_in_the_error() {
        let error = refusal(HostMessage::Pong { nonce: 1 }, "Welcome");
        assert!(matches!(
            error,
            ClientError::Unexpected {
                expected: "Welcome",
                got: "Pong"
            }
        ));
    }

    #[test]
    fn a_refusal_keeps_what_the_host_said() {
        let error = refusal(
            HostMessage::Failed(pravera_proto::ProtocolError::PermissionDenied),
            "SessionStarted",
        );
        assert!(matches!(
            error,
            ClientError::Refused(pravera_proto::ProtocolError::PermissionDenied)
        ));

        let error = refusal(
            HostMessage::Goodbye {
                reason: "the host is shutting down".into(),
            },
            "SessionStarted",
        );
        match error {
            ClientError::Ended(reason) => assert_eq!(reason, "the host is shutting down"),
            other => panic!("the reason was lost: {other:?}"),
        }
    }

    #[test]
    fn a_refused_password_is_not_worth_retrying_but_a_lost_connection_is() {
        assert!(!ClientError::Denied.is_worth_retrying());
        assert!(!ClientError::VersionMismatch { ours: 1, theirs: 2 }.is_worth_retrying());
        assert!(!ClientError::NoSharedCodec { offered: vec![] }.is_worth_retrying());

        assert!(
            ClientError::Transport(pravera_transport::TransportError::StreamClosed)
                .is_worth_retrying()
        );
    }

    #[test]
    fn a_denied_login_becomes_the_shared_auth_failure_and_says_nothing_more() {
        // The whole point of the reasonless refusal survives the conversion
        // into `pravera_core::Error`: nothing downstream may learn whether the
        // account existed.
        let shared: pravera_core::Error = ClientError::Denied.into();
        assert!(matches!(shared, pravera_core::Error::AuthFailed));

        let shown = shared.to_string();
        assert!(!shown.contains("user"), "{shown}");
        assert!(!shown.contains("password"), "{shown}");
    }
}
