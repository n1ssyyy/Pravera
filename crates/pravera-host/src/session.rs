//! The host side of a session, as a state machine.
//!
//! This is where permissions are enforced, and it is the only place they are.
//! The permission set sent to a client in `AuthResult` is a display hint; every
//! request that arrives is checked here against the host's own record of what
//! the logged-in account may do. A client patched to believe it holds `ADMIN`
//! gets exactly as far as one that has not been patched.
//!
//! ## No I/O
//!
//! [`HostSession::handle`] takes a message and returns a [`Response`]. It never
//! reads, writes, captures or injects. That keeps the security-critical logic
//! synchronous, deterministic, and testable without a network: every rule below
//! has a test that calls `handle` directly.
//!
//! The driver in [`crate::serve`] performs whatever the response asks for.

use std::sync::Arc;

use pravera_auth::{authenticate, AuthOutcome, Permission, Role, User};
use pravera_core::{
    for_log, AudioCodec, AudioFormat, Codec, FrameFormat, PixelFormat, QualityProfile, Resolution,
};
use pravera_proto::{
    AuthResult, ClientMessage, ClipboardSeq, ClipboardUpdate, Credentials, Hello, HostMessage,
    InputEvent, Monitor, MonitorId, ProtocolError, SessionConfig, SessionRequest, Welcome, VERSION,
};
use pravera_transport::PeerKey;
use tracing::{debug, info, warn};

use crate::store::UserStore;

/// What this host offers and how strictly it behaves.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Shown in the client's device list. A label, not an identity: the
    /// identity is the device key.
    pub host_name: String,
    /// Encoders available here, best first.
    pub codecs: Vec<Codec>,
    /// Chroma layouts this host's encoders can actually produce, best first.
    ///
    /// A profile asks for a layout — Quality wants 4:4:4 so text edges stay
    /// sharp — and gets it only if an encoder here can deliver it. Software
    /// H.264 codes 4:2:0 and nothing else, so a host running only that
    /// advertises 4:2:0 no matter what the profile would prefer.
    ///
    /// It matters that this is not simply copied from the profile: the client
    /// picks its colour-conversion shader from the format it is told, and one
    /// that is told 4:4:4 while receiving 4:2:0 renders a broken picture.
    pub chroma_formats: Vec<PixelFormat>,
    /// Displays that can be streamed.
    pub monitors: Vec<Monitor>,
    /// Audio codecs this host can produce, best first.
    ///
    /// Empty when this machine cannot tap its own mixer at all, which is the
    /// case on every platform but Windows today. A host with an empty list
    /// never offers audio, so a client is never told to expect a stream that
    /// cannot exist.
    pub audio_codecs: Vec<AudioCodec>,
    /// Roles this host recognises, used when the store does not override them.
    pub roles: Vec<Role>,
    /// Password attempts allowed on one connection before it is dropped.
    ///
    /// Argon2id at 19 MiB takes tens of milliseconds, which already limits
    /// guessing to a few dozen per second per connection. This limits the total
    /// so an attacker has to pay for a fresh QUIC handshake every few guesses
    /// rather than grinding forever down one open stream.
    pub max_auth_attempts: u32,
}

impl Default for HostConfig {
    fn default() -> Self {
        HostConfig {
            host_name: "pravera".into(),
            codecs: vec![Codec::H264],
            chroma_formats: vec![PixelFormat::Nv12],
            monitors: Vec::new(),
            audio_codecs: Vec::new(),
            roles: Role::builtins(),
            max_auth_attempts: 3,
        }
    }
}

/// What the driver should do after a message.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Response {
    /// Send this back on the control stream, before applying `effect`.
    pub reply: Option<HostMessage>,
    /// Do this locally.
    pub effect: Option<Effect>,
}

/// A local action the state machine cannot perform itself.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Authorised input. Hand it to the platform input sink.
    ///
    /// Nothing goes back: acknowledging each event would double the round trips
    /// on the most latency-sensitive path in the product.
    Inject(InputEvent),
    /// Start or reconfigure the capture and encode pipeline.
    Stream(SessionConfig),
    /// The client's decoder lost sync. Emit a keyframe on the next capture.
    Keyframe,
    /// Read or write this machine's clipboard, **and answer on the control
    /// stream**.
    ///
    /// The only effect that owes a reply. The state machine cannot produce one
    /// because the answer is not in its state — it is in the clipboard — so the
    /// driver sends it. [`Response::reply`] is always `None` alongside this, and
    /// the driver must send exactly one message for it, or the client waits
    /// forever for an answer to a request it is blocked on.
    Clipboard(ClipboardAsk),
    /// End the session.
    Disconnect,
    /// Start a shell on this machine, in a console of the given size.
    ///
    /// Already permission-checked (`CONTROL` — a shell is keyboard control
    /// taken to its conclusion) and size-checked. The `TerminalStarted` reply
    /// is part of the [`Response`] that carries this effect, so the driver
    /// spawns the terminal knowing the client has already been told to accept
    /// its stream.
    OpenTerminal {
        cols: u16,
        rows: u16,
    },
    /// Ask Windows to generate Ctrl+Alt+Del. Permission-checked (`CONTROL`)
    /// and answered with `SasSent` before the effect runs, so the client knows
    /// the request was heard even if `sas.dll` is not available.
    SendSas,
}

/// What to do to this machine's clipboard.
///
/// Already permission-checked: `Read` means the peer holds `CLIPBOARD_READ`,
/// `Write` means it holds `CLIPBOARD_WRITE`. The driver does not re-check —
/// see the crate documentation on why there is exactly one authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardAsk {
    /// Has it changed since `since`? If so, what does it say now?
    Read { since: ClipboardSeq },
    /// Put this on it.
    Write { text: String },
}

/// What happened when the driver did it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardAnswer {
    Update(ClipboardUpdate),
    /// Written, and the sequence that produced. The client records it so its
    /// next poll is not told its own paste is a change to copy back.
    Written(ClipboardSeq),
    /// This machine has no clipboard Pravera can reach, or reaching it failed.
    ///
    /// Deliberately carries no detail. The reason is in the host's log; the
    /// peer is told only that it did not work.
    Unavailable,
}

impl Response {
    fn reply(message: HostMessage) -> Self {
        Response {
            reply: Some(message),
            effect: None,
        }
    }

    fn effect(effect: Effect) -> Self {
        Response {
            reply: None,
            effect: Some(effect),
        }
    }

    /// Refuse this request but keep the session.
    fn refuse(error: ProtocolError) -> Self {
        Response::reply(HostMessage::Failed(error))
    }

    /// Refuse a message the client is not waiting on an answer for.
    ///
    /// Nothing goes back. See [`ClientMessage::expects_reply`]: answering a
    /// refused input event would put a reply on the control stream for every
    /// mouse movement, and let a peer with no permissions make the host
    /// generate more traffic than the peer sent.
    fn refuse_silently(message: &ClientMessage, error: ProtocolError) -> Self {
        if message.expects_reply() {
            Response::refuse(error)
        } else {
            Response::default()
        }
    }

    /// Refuse and hang up. For failures there is no recovering from, such as a
    /// peer speaking a different protocol version.
    fn fatal(error: ProtocolError) -> Self {
        Response {
            reply: Some(HostMessage::Failed(error)),
            effect: Some(Effect::Disconnect),
        }
    }
}

/// Where a connection has got to.
///
/// The stage is half the enforcement: a permission check answers "may this
/// account do that", and the stage answers "is now a sensible time to ask".
/// Skipping a step is [`ProtocolError::OutOfOrder`], never an implicit success.
#[derive(Debug)]
enum Stage {
    /// Connected. Nothing said yet.
    Greeting,
    /// Versions agreed. Waiting for credentials.
    Login,
    /// Logged in. No stream yet.
    Ready { user: User },
    /// Streaming.
    Streaming {
        user: User,
        config: SessionConfig,
        /// Carried so a monitor switch keeps honouring the cap the client
        /// asked for at the start.
        max_resolution: Option<Resolution>,
    },
    /// Finished. Everything further is ignored.
    Ended,
}

/// One client's conversation with this host.
pub struct HostSession<S: UserStore> {
    config: Arc<HostConfig>,
    store: S,
    peer: PeerKey,
    stage: Stage,
    /// Codecs both ends have, best first. Empty until `Hello`.
    shared_codecs: Vec<Codec>,
    attempts: u32,
    /// The protocol version this connection was negotiated at. What `Hello`
    /// must carry and what `Welcome` answers with.
    version: u16,
}

impl<S: UserStore> HostSession<S> {
    pub fn new(config: Arc<HostConfig>, store: S, peer: PeerKey) -> Self {
        HostSession {
            config,
            store,
            peer,
            stage: Stage::Greeting,
            shared_codecs: Vec::new(),
            attempts: 0,
            version: VERSION,
        }
    }

    /// Pin the protocol version this connection speaks, as negotiated by the
    /// transport. Defaults to the newest, which is right for a session with no
    /// transport under it; `serve` sets it from the connection's ALPN.
    pub fn set_version(&mut self, version: u16) {
        self.version = version;
    }

    /// The protocol version this connection speaks.
    pub fn version(&self) -> u16 {
        self.version
    }

    /// The device on the other end, proven by the TLS handshake.
    pub fn peer(&self) -> PeerKey {
        self.peer
    }

    /// The logged-in account, once there is one.
    pub fn user(&self) -> Option<&User> {
        match &self.stage {
            Stage::Ready { user } | Stage::Streaming { user, .. } => Some(user),
            _ => None,
        }
    }

    /// What the logged-in account may do. Empty before authentication.
    pub fn permissions(&self) -> Permission {
        self.user()
            .map(|user| user.permissions)
            .unwrap_or_else(Permission::empty)
    }

    pub fn is_authenticated(&self) -> bool {
        self.user().is_some()
    }

    pub fn is_streaming(&self) -> bool {
        matches!(self.stage, Stage::Streaming { .. })
    }

    pub fn is_ended(&self) -> bool {
        matches!(self.stage, Stage::Ended)
    }

    /// The stream currently negotiated, if any.
    pub fn session_config(&self) -> Option<&SessionConfig> {
        match &self.stage {
            Stage::Streaming { config, .. } => Some(config),
            _ => None,
        }
    }

    /// Handle one message from the client.
    pub fn handle(&mut self, message: ClientMessage) -> Response {
        if self.is_ended() {
            return Response::default();
        }

        match message {
            // Allowed at any stage. A goodbye is information, not a request.
            ClientMessage::Goodbye { reason } => {
                debug!(peer = %self.peer, reason = %for_log(&reason), "client said goodbye");
                self.stage = Stage::Ended;
                Response::effect(Effect::Disconnect)
            }

            // Allowed before authentication so a client can measure the route
            // before deciding to log in. It echoes a number the caller chose
            // and reveals nothing.
            ClientMessage::Ping { nonce } => Response::reply(HostMessage::Pong { nonce }),

            ClientMessage::Hello(hello) => self.on_hello(hello),
            ClientMessage::Authenticate(credentials) => self.on_authenticate(credentials),

            // Everything else goes through the gate.
            other => self.on_privileged(other),
        }
    }

    // ------------------------------------------------------------ handshake

    fn on_hello(&mut self, hello: Hello) -> Response {
        if !matches!(self.stage, Stage::Greeting) {
            return Response::refuse(ProtocolError::OutOfOrder);
        }

        if hello.version != self.version {
            // Both numbers are safe to disclose; the ALPN already carried ours
            // in cleartext during the TLS handshake.
            warn!(peer = %self.peer, theirs = hello.version, "protocol version mismatch");
            self.stage = Stage::Ended;
            return Response::fatal(ProtocolError::VersionMismatch {
                ours: self.version,
                theirs: hello.version,
            });
        }

        self.shared_codecs = negotiate(&self.config.codecs, &hello.codecs);
        if self.shared_codecs.is_empty() {
            warn!(peer = %self.peer, "no codec in common");
            self.stage = Stage::Ended;
            return Response::fatal(ProtocolError::Unsupported);
        }

        debug!(
            peer = %self.peer,
            client = %for_log(&hello.client_name),
            codecs = ?self.shared_codecs,
            "hello accepted"
        );
        self.stage = Stage::Login;

        Response::reply(HostMessage::Welcome(Welcome {
            version: self.version,
            host_name: self.config.host_name.clone(),
            // The intersection rather than the full list. It is a subset of
            // what this host has, so it is not a lie, and it saves the client
            // from picking something the other end cannot decode.
            codecs: self.shared_codecs.clone(),
        }))
    }

    fn on_authenticate(&mut self, credentials: Credentials) -> Response {
        if !matches!(self.stage, Stage::Login) {
            // Re-authenticating mid-session would be a way to change privileges
            // without reconnecting, and there is no reason to want one.
            return Response::refuse(ProtocolError::OutOfOrder);
        }

        self.attempts += 1;

        // No early exit for a malformed or absent username. The lookup misses,
        // `authenticate` verifies against its dummy hash anyway, and the reply
        // takes the same time it would for a real account with a wrong
        // password. That equal timing is the whole defence.
        let stored = self.store.credential(&credentials.username);
        let roles = self.store.roles();
        let outcome = authenticate(
            &credentials.username,
            &credentials.password,
            stored.as_ref(),
            &roles,
        );

        match outcome {
            AuthOutcome::Granted(user) => {
                info!(
                    peer = %self.peer,
                    user = %for_log(&user.username),
                    role = %for_log(&user.role),
                    "authenticated"
                );
                let reply = HostMessage::AuthResult(AuthResult::Granted {
                    username: user.username.clone(),
                    role: user.role.clone(),
                    permissions: user.permissions,
                });
                self.stage = Stage::Ready { user };
                Response::reply(reply)
            }

            refused => {
                // The reason is logged here and nowhere else. On the wire this
                // is one reasonless refusal, so a stranger cannot use the login
                // prompt to find out which usernames exist.
                warn!(
                    peer = %self.peer,
                    user = %for_log(&credentials.username),
                    attempt = self.attempts,
                    reason = ?refused,
                    "authentication refused"
                );

                let reply = HostMessage::AuthResult(AuthResult::Denied);
                if self.attempts >= self.config.max_auth_attempts {
                    self.stage = Stage::Ended;
                    Response {
                        reply: Some(reply),
                        effect: Some(Effect::Disconnect),
                    }
                } else {
                    Response::reply(reply)
                }
            }
        }
    }

    // ------------------------------------------------------- the permission gate

    /// Every request that is not part of the handshake passes through here.
    fn on_privileged(&mut self, message: ClientMessage) -> Response {
        let Some(user) = self.user() else {
            // Before login the answer is always the same, whatever was asked.
            // Anything more specific would describe the host to a stranger.
            return Response::refuse_silently(&message, ProtocolError::Unauthenticated);
        };

        let required = message.required_permission();
        if !user.permissions.allows(required) {
            // Logged whether or not anything goes back, because the refusal is
            // exactly what an operator investigating a session needs to see.
            warn!(
                peer = %self.peer,
                user = %for_log(&user.username),
                ?required,
                held = ?user.permissions,
                answered = message.expects_reply(),
                "request refused"
            );
            return Response::refuse_silently(&message, ProtocolError::PermissionDenied);
        }

        // Checked here, with the whole message in hand, rather than inside each
        // handler. A peer may be running a patched client, so any length or
        // coordinate this host is about to depend on is one this host has to
        // verify — and doing it in one place means a message added later cannot
        // quietly skip it.
        if !message.is_well_formed() {
            warn!(
                peer = %self.peer,
                user = %for_log(&user.username),
                "refused a malformed message"
            );
            return Response::refuse_silently(&message, ProtocolError::Malformed);
        }

        match message {
            ClientMessage::StartSession(request) => self.on_start_session(request),
            ClientMessage::ListMonitors => {
                Response::reply(HostMessage::Monitors(self.config.monitors.clone()))
            }
            ClientMessage::SelectMonitor(id) => self.on_select_monitor(id),
            ClientMessage::SetProfile(profile) => self.on_set_profile(profile),
            ClientMessage::Input(event) => self.on_input(event),
            ClientMessage::RequestKeyframe => self.on_request_keyframe(),

            // A terminal is a promise with an order to it: the reply goes out
            // first, then the effect opens the stream the client is waiting to
            // accept. If the shell fails to start, the stream opens and
            // immediately closes — see `pravera_proto::terminal`.
            ClientMessage::OpenTerminal { cols, rows } => Response {
                reply: Some(HostMessage::TerminalStarted),
                effect: Some(Effect::OpenTerminal { cols, rows }),
            },

            ClientMessage::SendSas => Response {
                reply: Some(HostMessage::SasSent),
                effect: Some(Effect::SendSas),
            },

            // Not gated on a running stream. Someone with a clipboard grant and
            // no picture yet is a legitimate case — a script pushing text to a
            // machine has no reason to start an encoder first.
            ClientMessage::GetClipboard { since } => {
                Response::effect(Effect::Clipboard(ClipboardAsk::Read { since }))
            }
            ClientMessage::SetClipboard { text } => {
                Response::effect(Effect::Clipboard(ClipboardAsk::Write { text }))
            }

            // Handled before the gate. Refusing rather than panicking, because
            // a panic reachable from a client message is a denial of service.
            ClientMessage::Hello(_)
            | ClientMessage::Authenticate(_)
            | ClientMessage::Ping { .. }
            | ClientMessage::Goodbye { .. } => {
                debug_assert!(false, "handshake message reached the permission gate");
                Response::refuse(ProtocolError::OutOfOrder)
            }
        }
    }

    // -------------------------------------------------------------- session

    fn on_start_session(&mut self, request: SessionRequest) -> Response {
        let Stage::Ready { user } = &self.stage else {
            // Already streaming. Changing an active stream is SelectMonitor or
            // SetProfile, so that the two ends cannot disagree about which
            // negotiation is current.
            return Response::refuse(ProtocolError::OutOfOrder);
        };
        let user = user.clone();

        // StartSession itself needs only VIEW, but choosing a display is
        // choosing a display. Without this, a viewer could pick any monitor by
        // naming it here and never calling ListMonitors.
        if request.monitor != MonitorId::PRIMARY
            && !user.permissions.allows(Permission::MULTI_MONITOR)
        {
            warn!(
                peer = %self.peer,
                user = %for_log(&user.username),
                "refused a non-primary monitor without MULTI_MONITOR"
            );
            return Response::refuse(ProtocolError::PermissionDenied);
        }

        let Some(monitor) = self.monitor(request.monitor) else {
            return Response::refuse(ProtocolError::Unsupported);
        };
        let Some(resolution) = clamp(monitor.resolution, request.max_resolution) else {
            return Response::refuse(ProtocolError::Unsupported);
        };
        let Some(&codec) = self.shared_codecs.first() else {
            debug_assert!(
                false,
                "a session cannot start before Hello negotiated a codec"
            );
            return Response::refuse(ProtocolError::OutOfOrder);
        };

        let Some(pixel_format) = self.chroma_for(request.profile) else {
            debug_assert!(false, "a host with no chroma format can encode nothing");
            return Response::refuse(ProtocolError::Unsupported);
        };

        let audio = self.audio_for(request.audio, request.profile, &user);

        let config = SessionConfig {
            monitor: request.monitor,
            format: FrameFormat {
                resolution,
                pixel_format,
                codec,
            },
            profile: request.profile,
            audio,
        };

        info!(
            peer = %self.peer,
            user = %for_log(&user.username),
            monitor = request.monitor.0,
            %resolution,
            codec = codec.name(),
            profile = request.profile.name(),
            audio = audio.map(|a| a.codec.name()).unwrap_or("none"),
            "session started"
        );

        self.stage = Stage::Streaming {
            user,
            config: config.clone(),
            max_resolution: request.max_resolution,
        };
        Response {
            reply: Some(HostMessage::SessionStarted(config.clone())),
            effect: Some(Effect::Stream(config)),
        }
    }

    fn on_select_monitor(&mut self, id: MonitorId) -> Response {
        let Stage::Streaming {
            config,
            max_resolution,
            ..
        } = &self.stage
        else {
            return Response::refuse(ProtocolError::OutOfOrder);
        };
        let (profile, max_resolution) = (config.profile, *max_resolution);

        let Some(monitor) = self.monitor(id) else {
            return Response::refuse(ProtocolError::Unsupported);
        };
        // The new display has its own native size, but the cap the client asked
        // for at the start still applies.
        let Some(resolution) = clamp(monitor.resolution, max_resolution) else {
            return Response::refuse(ProtocolError::Unsupported);
        };

        self.reconfigure(|config| {
            config.monitor = id;
            config.format.resolution = resolution;
            config.format.pixel_format = profile.preferred_pixel_format();
        })
    }

    fn on_set_profile(&mut self, profile: QualityProfile) -> Response {
        if !self.is_streaming() {
            return Response::refuse(ProtocolError::OutOfOrder);
        }
        let Some(pixel_format) = self.chroma_for(profile) else {
            return Response::refuse(ProtocolError::Unsupported);
        };
        self.reconfigure(|config| {
            config.profile = profile;
            // The profile asks for a chroma layout, so a switch to Quality
            // carries 4:4:4 with it when this host can produce it. When it
            // cannot, the layout stays as it was — the profile still changes
            // rate control and cadence, which is most of what it is for.
            config.format.pixel_format = pixel_format;
        })
    }

    fn on_input(&mut self, event: InputEvent) -> Response {
        if !self.is_streaming() {
            // Input before a session has started has nothing to aim at: there
            // is no negotiated resolution to scale a pointer against.
            return Response::refuse(ProtocolError::OutOfOrder);
        }
        // Well-formedness — a NaN coordinate that would be scaled against the
        // host resolution and land somewhere arbitrary — is checked by
        // `on_privileged` before this is reached, silently, because input
        // expects no reply and answering one would put a message on the stream
        // for every mouse movement.
        debug_assert!(event.is_well_formed());
        Response::effect(Effect::Inject(event))
    }

    fn on_request_keyframe(&mut self) -> Response {
        if !self.is_streaming() {
            return Response::refuse(ProtocolError::OutOfOrder);
        }
        Response::effect(Effect::Keyframe)
    }

    // --------------------------------------------------------------- helpers

    fn monitor(&self, id: MonitorId) -> Option<&Monitor> {
        self.config.monitors.iter().find(|monitor| monitor.id == id)
    }

    /// The chroma layout to promise for a profile.
    ///
    /// The profile's preference when an encoder here can produce it, otherwise
    /// this host's best available. Never the preference alone: the client
    /// selects its colour-conversion path from what it is told, so promising
    /// 4:4:4 and sending 4:2:0 produces a picture with the colour planes half
    /// the size the client is reading them at.
    fn chroma_for(&self, profile: QualityProfile) -> Option<PixelFormat> {
        let preferred = profile.preferred_pixel_format();
        if self.config.chroma_formats.contains(&preferred) {
            return Some(preferred);
        }
        self.config.chroma_formats.first().copied()
    }

    /// Apply a change to the live stream config and tell both sides about it.
    /// The audio stream this session gets, if any.
    ///
    /// Three things have to line up: the client asked, the account is allowed
    /// to listen, and this machine has something to listen with. Any one of
    /// them missing gives `None`, and the client is not told which — a peer
    /// that was refused does not learn whether the refusal was about its
    /// permissions or about the host's hardware.
    fn audio_for(&self, wanted: bool, profile: QualityProfile, user: &User) -> Option<AudioFormat> {
        if !wanted || !user.permissions.allows(Permission::AUDIO) {
            return None;
        }
        let preferred = profile.preferred_audio_codec();
        let codec = if self.config.audio_codecs.contains(&preferred) {
            preferred
        } else {
            // The profile only expresses a preference. A host that can produce
            // audio at all should be heard, even if not in the shape asked for.
            *self.config.audio_codecs.first()?
        };
        Some(AudioFormat::new(codec))
    }

    fn reconfigure(&mut self, change: impl FnOnce(&mut SessionConfig)) -> Response {
        let Stage::Streaming { config, .. } = &mut self.stage else {
            debug_assert!(false, "reconfigure called outside a stream");
            return Response::refuse(ProtocolError::OutOfOrder);
        };
        change(config);
        let config = config.clone();
        Response {
            reply: Some(HostMessage::SessionStarted(config.clone())),
            effect: Some(Effect::Stream(config)),
        }
    }
}

/// Codecs both ends have, best first.
///
/// Filtered from the host's own list, so a client advertising ten thousand
/// codecs produces at most as many entries as this host actually supports.
fn negotiate(ours: &[Codec], theirs: &[Codec]) -> Vec<Codec> {
    let mut shared: Vec<Codec> = ours
        .iter()
        .copied()
        .filter(|codec| theirs.contains(codec))
        .collect();
    shared.sort_by_key(|codec| std::cmp::Reverse(codec.preference()));
    shared.dedup();
    shared
}

/// The resolution to encode at, or `None` if the request makes no sense.
///
/// A client may ask for less than native but never more: the extra pixels do
/// not exist, and upscaling on the host would spend bandwidth inventing them.
fn clamp(native: Resolution, requested: Option<Resolution>) -> Option<Resolution> {
    let Some(requested) = requested else {
        return Some(native);
    };
    if requested.width == 0 || requested.height == 0 {
        // A zero-sized encoder surface. Refused rather than silently corrected,
        // because there is no sensible correction.
        return None;
    }
    Some(Resolution::new(
        requested.width.min(native.width),
        requested.height.min(native.height),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;
    use pravera_proto::PointerButton;

    fn monitors() -> Vec<Monitor> {
        vec![
            Monitor {
                id: MonitorId::PRIMARY,
                name: "primary".into(),
                resolution: Resolution::new(3840, 2160),
                position: (0, 0),
                scale: 1.5,
                primary: true,
            },
            Monitor {
                id: MonitorId(1),
                name: "secondary".into(),
                resolution: Resolution::new(1920, 1080),
                position: (3840, 0),
                scale: 1.0,
                primary: false,
            },
        ]
    }

    fn config() -> Arc<HostConfig> {
        Arc::new(HostConfig {
            host_name: "workshop".into(),
            codecs: vec![Codec::H265, Codec::H264, Codec::OpenH264],
            // A host with hardware good enough for 4:4:4, so the tests about
            // honouring a profile's preference have something to honour.
            chroma_formats: vec![PixelFormat::Yuv444, PixelFormat::Nv12],
            audio_codecs: vec![AudioCodec::Pcm16, AudioCodec::Adpcm4],
            monitors: monitors(),
            ..HostConfig::default()
        })
    }

    /// A host that can be heard, driven up to a stream as the named account.
    ///
    /// Returns what it agreed to, which is the thing every audio test is about.
    fn agreed_audio(
        username: &str,
        password: &str,
        asked: bool,
        profile: QualityProfile,
        codecs: Vec<AudioCodec>,
    ) -> Option<AudioFormat> {
        let config = Arc::new(HostConfig {
            audio_codecs: codecs,
            ..(*config()).clone()
        });
        let mut host = HostSession::new(config, store(), peer());
        host.handle(hello());
        assert!(
            host.handle(login(username, password))
                .reply
                .as_ref()
                .map(|reply| matches!(reply, HostMessage::AuthResult(r) if r.is_granted()))
                .unwrap_or(false),
            "{username} should have been able to log in"
        );

        let response = host.handle(ClientMessage::StartSession(SessionRequest {
            monitor: MonitorId::PRIMARY,
            profile,
            max_resolution: None,
            audio: asked,
        }));
        match response.reply {
            Some(HostMessage::SessionStarted(config)) => config.audio,
            other => panic!("the session did not start: {other:?}"),
        }
    }

    fn both() -> Vec<AudioCodec> {
        vec![AudioCodec::Pcm16, AudioCodec::Adpcm4]
    }

    /// A host whose only encoder codes 4:2:0, which is every machine running
    /// nothing but the software fallback.
    fn subsampled_only() -> HostSession<MemoryStore> {
        let config = Arc::new(HostConfig {
            chroma_formats: vec![PixelFormat::Nv12],
            ..(*config()).clone()
        });
        HostSession::new(config, store(), peer())
    }

    fn store() -> MemoryStore {
        let mut store = MemoryStore::new();
        store.add("looker", "look-only", "viewer").unwrap();
        store.add("driver", "drive-it", "operator").unwrap();
        store.add("boss", "run-it-all", "admin").unwrap();
        store.add("gone", "still-valid", "operator").unwrap();
        store.set_enabled("gone", false);
        store
    }

    fn peer() -> PeerKey {
        // Only ever displayed in these tests, never dialled, so it does not
        // need to be a real curve point.
        PeerKey::from_bytes([3u8; 32])
    }

    fn session() -> HostSession<MemoryStore> {
        HostSession::new(config(), store(), peer())
    }

    fn hello() -> ClientMessage {
        ClientMessage::Hello(Hello {
            version: VERSION,
            client_name: "laptop".into(),
            codecs: vec![Codec::H264, Codec::OpenH264],
        })
    }

    fn login(username: &str, password: &str) -> ClientMessage {
        ClientMessage::Authenticate(Credentials {
            username: username.into(),
            password: password.into(),
        })
    }

    fn start() -> ClientMessage {
        ClientMessage::StartSession(SessionRequest {
            monitor: MonitorId::PRIMARY,
            profile: QualityProfile::Adaptive,
            max_resolution: None,
            audio: false,
        })
    }

    fn click() -> ClientMessage {
        ClientMessage::Input(InputEvent::PointerButton {
            button: PointerButton::Left,
            pressed: true,
        })
    }

    /// Drive a session up to a live stream as the named account.
    fn streaming_as(username: &str, password: &str) -> HostSession<MemoryStore> {
        let mut host = session();
        assert!(matches!(
            host.handle(hello()).reply,
            Some(HostMessage::Welcome(_))
        ));
        assert!(
            host.handle(login(username, password))
                .reply
                .as_ref()
                .map(|reply| matches!(reply, HostMessage::AuthResult(r) if r.is_granted()))
                .unwrap_or(false),
            "{username} should have been able to log in"
        );
        assert!(matches!(
            host.handle(start()).reply,
            Some(HostMessage::SessionStarted(_))
        ));
        host
    }

    #[test]
    fn a_session_that_did_not_ask_for_sound_is_not_given_any() {
        assert_eq!(
            agreed_audio(
                "boss",
                "run-it-all",
                false,
                QualityProfile::Adaptive,
                both()
            ),
            None
        );
    }

    #[test]
    fn a_role_without_the_permission_is_refused_the_hosts_audio() {
        // The whole point of `Permission::AUDIO`. A viewer that asks is
        // refused here, host-side, rather than by a client choosing not to
        // play what it was sent.
        assert_eq!(
            agreed_audio(
                "looker",
                "look-only",
                true,
                QualityProfile::Adaptive,
                both()
            ),
            None
        );
    }

    #[test]
    fn a_machine_that_cannot_hear_itself_offers_nothing() {
        // Every platform but Windows today. The client is told there is no
        // audio rather than being left waiting for a stream that cannot exist.
        assert_eq!(
            agreed_audio(
                "boss",
                "run-it-all",
                true,
                QualityProfile::Adaptive,
                Vec::new()
            ),
            None
        );
    }

    #[test]
    fn a_refusal_does_not_say_which_refusal_it_was() {
        // Not asked, not permitted, and no device all give exactly `None`. A
        // peer that was refused must not learn whether the reason was its own
        // permissions or the host's hardware.
        let not_asked = agreed_audio(
            "boss",
            "run-it-all",
            false,
            QualityProfile::Adaptive,
            both(),
        );
        let not_allowed = agreed_audio(
            "looker",
            "look-only",
            true,
            QualityProfile::Adaptive,
            both(),
        );
        let no_device = agreed_audio(
            "boss",
            "run-it-all",
            true,
            QualityProfile::Adaptive,
            Vec::new(),
        );
        assert_eq!(not_asked, not_allowed);
        assert_eq!(not_allowed, no_device);
    }

    #[test]
    fn the_picture_profile_chooses_the_codec_the_sound_uses() {
        // Quality spends the bandwidth on both; the other two ration both.
        let quality = agreed_audio("boss", "run-it-all", true, QualityProfile::Quality, both());
        assert_eq!(quality.map(|f| f.codec), Some(AudioCodec::Pcm16));

        let latency = agreed_audio("boss", "run-it-all", true, QualityProfile::Latency, both());
        assert_eq!(latency.map(|f| f.codec), Some(AudioCodec::Adpcm4));
    }

    #[test]
    fn a_host_without_the_preferred_codec_is_still_heard() {
        // The profile expresses a preference, not a requirement. A host that
        // can produce sound should be heard even in a shape nobody asked for.
        let agreed = agreed_audio(
            "boss",
            "run-it-all",
            true,
            QualityProfile::Quality,
            vec![AudioCodec::Adpcm4],
        );
        assert_eq!(agreed.map(|f| f.codec), Some(AudioCodec::Adpcm4));
    }

    #[test]
    fn an_operator_is_allowed_to_listen() {
        // `operator` is the everyday role, and a remote desktop with no sound
        // for the everyday role is a remote desktop with no sound.
        let agreed = agreed_audio("driver", "drive-it", true, QualityProfile::Adaptive, both());
        assert_eq!(agreed.map(|f| f.codec), Some(AudioCodec::Adpcm4));
        assert!(agreed.is_some_and(|f| f.is_playable()));
    }

    fn failure(response: &Response) -> Option<ProtocolError> {
        match &response.reply {
            Some(HostMessage::Failed(error)) => Some(error.clone()),
            _ => None,
        }
    }

    // ------------------------------------------------------------ the gate

    #[test]
    fn a_viewer_is_refused_control_by_the_host() {
        // The rule the whole permission model exists for. The client was told
        // it holds VIEW only, but the host does not rely on the client having
        // believed that.
        let mut host = streaming_as("looker", "look-only");
        assert!(host.permissions().allows(Permission::VIEW));
        assert!(!host.permissions().allows(Permission::CONTROL));

        let response = host.handle(click());
        assert_eq!(
            response.effect, None,
            "a refused input must not be injected"
        );
        // Nothing goes back. The refusal is silent on the wire because input
        // is fire-and-forget; answering it would put a reply on the control
        // stream for every event a peer chose to send. What matters is that
        // the click did not happen, and it did not.
        assert_eq!(response.reply, None);
    }

    #[test]
    fn an_operator_is_allowed_control() {
        let mut host = streaming_as("driver", "drive-it");
        let response = host.handle(click());
        assert_eq!(response.reply, None, "input is not acknowledged");
        assert!(matches!(response.effect, Some(Effect::Inject(_))));
    }

    #[test]
    fn every_input_event_a_viewer_can_send_is_refused() {
        let mut host = streaming_as("looker", "look-only");
        let events = [
            InputEvent::PointerMoveAbsolute { x: 0.5, y: 0.5 },
            InputEvent::PointerMoveRelative { dx: 1, dy: 1 },
            InputEvent::Scroll { dx: 0.0, dy: 1.0 },
            InputEvent::Key {
                code: pravera_proto::KeyCode(0x04),
                pressed: true,
            },
            InputEvent::Text("a".into()),
        ];
        for event in events {
            let response = host.handle(ClientMessage::Input(event.clone()));
            assert_eq!(response.effect, None, "{event:?} reached the platform");
            assert_eq!(response.reply, None, "{event:?} was answered");
        }
    }

    #[test]
    fn a_viewer_cannot_enumerate_displays() {
        let mut host = streaming_as("looker", "look-only");
        let response = host.handle(ClientMessage::ListMonitors);
        assert_eq!(failure(&response), Some(ProtocolError::PermissionDenied));
        assert!(
            !matches!(response.reply, Some(HostMessage::Monitors(_))),
            "the monitor list leaked to a viewer"
        );
    }

    #[test]
    fn a_viewer_cannot_reach_a_second_display_by_naming_it_in_start_session() {
        // StartSession needs only VIEW, so without an explicit check here a
        // viewer could skip ListMonitors and simply guess monitor ids.
        let mut host = session();
        host.handle(hello());
        host.handle(login("looker", "look-only"));

        let response = host.handle(ClientMessage::StartSession(SessionRequest {
            monitor: MonitorId(1),
            profile: QualityProfile::Adaptive,
            max_resolution: None,
            audio: false,
        }));
        assert_eq!(failure(&response), Some(ProtocolError::PermissionDenied));
        assert!(!host.is_streaming());
    }

    #[test]
    fn an_operator_may_reach_a_second_display() {
        let mut host = session();
        host.handle(hello());
        host.handle(login("driver", "drive-it"));

        let response = host.handle(ClientMessage::StartSession(SessionRequest {
            monitor: MonitorId(1),
            profile: QualityProfile::Adaptive,
            max_resolution: None,
            audio: false,
        }));
        match response.reply {
            Some(HostMessage::SessionStarted(config)) => {
                assert_eq!(config.monitor, MonitorId(1));
                assert_eq!(config.format.resolution, Resolution::new(1920, 1080));
            }
            other => panic!("expected a session, got {other:?}"),
        }
    }

    #[test]
    fn an_admin_may_do_everything_a_client_can_ask_for() {
        let mut host = streaming_as("boss", "run-it-all");
        assert!(matches!(
            host.handle(ClientMessage::ListMonitors).reply,
            Some(HostMessage::Monitors(_))
        ));
        assert!(matches!(
            host.handle(click()).effect,
            Some(Effect::Inject(_))
        ));
        assert!(matches!(
            host.handle(ClientMessage::RequestKeyframe).effect,
            Some(Effect::Keyframe)
        ));
    }

    // ------------------------------------------------------- before login

    #[test]
    fn nothing_privileged_works_before_authentication() {
        let mut host = session();
        host.handle(hello());

        for message in [
            start(),
            click(),
            ClientMessage::ListMonitors,
            ClientMessage::SelectMonitor(MonitorId(1)),
            ClientMessage::SetProfile(QualityProfile::Latency),
            ClientMessage::RequestKeyframe,
        ] {
            let response = host.handle(message.clone());
            assert_eq!(
                response.effect, None,
                "{message:?} took effect before login"
            );
            // Everything a client waits on is refused out loud; the
            // fire-and-forget messages are refused in silence, because
            // answering them lets a stranger make the host talk.
            let expected = message
                .expects_reply()
                .then_some(ProtocolError::Unauthenticated);
            assert_eq!(failure(&response), expected, "{message:?}");
        }
    }

    #[test]
    fn an_unauthenticated_peer_gets_the_same_answer_whatever_it_asks() {
        // Varying the refusal by request would describe the host's capabilities
        // and configuration to a stranger.
        let mut host = session();
        host.handle(hello());

        let answers: Vec<_> = [
            ClientMessage::ListMonitors,
            ClientMessage::SelectMonitor(MonitorId(200)),
            ClientMessage::SetProfile(QualityProfile::Quality),
            start(),
        ]
        .into_iter()
        .map(|message| host.handle(message).reply)
        .collect();

        assert!(
            answers.windows(2).all(|pair| pair[0] == pair[1]),
            "refusals differed by request: {answers:?}"
        );

        // And the ones that go unanswered say nothing at all, which is the
        // same for every one of them.
        for quiet in [click(), ClientMessage::RequestKeyframe] {
            assert_eq!(host.handle(quiet).reply, None);
        }
    }

    #[test]
    fn the_monitor_list_is_never_part_of_the_welcome() {
        let mut host = session();
        let Some(HostMessage::Welcome(welcome)) = host.handle(hello()).reply else {
            panic!("expected a welcome");
        };
        let encoded = postcard_bytes(&welcome);
        for monitor in monitors() {
            assert!(
                !contains(&encoded, monitor.name.as_bytes()),
                "a monitor name reached an unauthenticated peer"
            );
        }
    }

    fn postcard_bytes<T: serde::Serialize>(value: &T) -> Vec<u8> {
        pravera_proto::codec::encode(value).unwrap()
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    // -------------------------------------------------------------- login

    #[test]
    fn every_kind_of_failed_login_is_answered_identically() {
        // Three genuinely different situations. If any of them produced a
        // different reply, the login prompt would be a way to find out which
        // usernames exist on this host, and which of them are switched off.
        let refuse = |username: &str, password: &str| {
            let mut host = session();
            host.handle(hello());
            host.handle(login(username, password)).reply
        };

        let wrong_password = refuse("driver", "not-it");
        let no_such_user = refuse("nobody", "not-it");
        let disabled_account = refuse("gone", "still-valid");

        assert_eq!(
            wrong_password,
            Some(HostMessage::AuthResult(AuthResult::Denied))
        );
        assert_eq!(
            wrong_password, no_such_user,
            "an unknown username was distinguishable from a wrong password"
        );
        assert_eq!(
            wrong_password, disabled_account,
            "a disabled account was distinguishable from a wrong password"
        );
    }

    #[test]
    fn a_login_attempt_before_the_handshake_is_out_of_order() {
        // Credentials must not be checked at all until versions have been
        // agreed, or a peer could skip Hello and start guessing immediately.
        let mut host = session();
        assert_eq!(
            failure(&host.handle(login("driver", "drive-it"))),
            Some(ProtocolError::OutOfOrder)
        );
        assert!(!host.is_authenticated());
    }

    #[test]
    fn repeated_wrong_passwords_end_the_connection() {
        let mut host = session();
        host.handle(hello());

        for attempt in 1..3 {
            let response = host.handle(login("driver", "wrong"));
            assert_eq!(response.effect, None, "hung up early on attempt {attempt}");
            assert!(!host.is_ended());
        }

        // Third strike. The refusal still goes out, then the connection ends.
        let response = host.handle(login("driver", "wrong"));
        assert_eq!(
            response.reply,
            Some(HostMessage::AuthResult(AuthResult::Denied))
        );
        assert_eq!(response.effect, Some(Effect::Disconnect));
        assert!(host.is_ended());
    }

    #[test]
    fn a_correct_password_after_a_wrong_one_still_works() {
        let mut host = session();
        host.handle(hello());
        host.handle(login("driver", "typo"));

        let response = host.handle(login("driver", "drive-it"));
        match response.reply {
            Some(HostMessage::AuthResult(AuthResult::Granted { username, role, .. })) => {
                assert_eq!(username, "driver");
                assert_eq!(role, "operator");
            }
            other => panic!("expected a grant, got {other:?}"),
        }
    }

    #[test]
    fn a_grant_reports_the_permissions_the_host_will_actually_enforce() {
        let mut host = session();
        host.handle(hello());
        let Some(HostMessage::AuthResult(AuthResult::Granted { permissions, .. })) =
            host.handle(login("looker", "look-only")).reply
        else {
            panic!("expected a grant");
        };
        assert_eq!(
            permissions,
            host.permissions(),
            "the client was told something the host does not believe"
        );
    }

    #[test]
    fn authenticating_twice_is_refused() {
        let mut host = session();
        host.handle(hello());
        host.handle(login("looker", "look-only"));

        // Otherwise this would be a way to swap privileges without dialling
        // again, and any state built under the first role would carry over.
        let response = host.handle(login("boss", "run-it-all"));
        assert_eq!(failure(&response), Some(ProtocolError::OutOfOrder));
        assert_eq!(host.user().unwrap().username, "looker");
    }

    // ----------------------------------------------------------- handshake

    #[test]
    fn a_peer_speaking_a_different_version_is_told_so_and_dropped() {
        let mut host = session();
        let response = host.handle(ClientMessage::Hello(Hello {
            version: VERSION + 1,
            client_name: "future".into(),
            codecs: vec![Codec::H264],
        }));
        assert_eq!(
            failure(&response),
            Some(ProtocolError::VersionMismatch {
                ours: VERSION,
                theirs: VERSION + 1
            })
        );
        assert_eq!(response.effect, Some(Effect::Disconnect));
        assert!(host.is_ended());
    }

    #[test]
    fn a_peer_with_no_codec_in_common_is_dropped() {
        let mut host = session();
        let response = host.handle(ClientMessage::Hello(Hello {
            version: VERSION,
            client_name: "exotic".into(),
            codecs: vec![Codec::Av1],
        }));
        assert_eq!(failure(&response), Some(ProtocolError::Unsupported));
        assert_eq!(response.effect, Some(Effect::Disconnect));
    }

    #[test]
    fn the_welcome_offers_the_best_codec_both_ends_have() {
        let mut host = session();
        let Some(HostMessage::Welcome(welcome)) = host
            .handle(ClientMessage::Hello(Hello {
                version: VERSION,
                client_name: "laptop".into(),
                codecs: vec![Codec::OpenH264, Codec::H265],
            }))
            .reply
        else {
            panic!("expected a welcome");
        };
        // Host has H265, H264, OpenH264; client has OpenH264, H265.
        assert_eq!(welcome.codecs, vec![Codec::H265, Codec::OpenH264]);
    }

    #[test]
    fn a_second_hello_is_refused() {
        let mut host = session();
        host.handle(hello());
        assert_eq!(
            failure(&host.handle(hello())),
            Some(ProtocolError::OutOfOrder)
        );
    }

    #[test]
    fn a_ping_is_answered_at_any_stage_and_reveals_nothing() {
        let mut host = session();
        assert_eq!(
            host.handle(ClientMessage::Ping { nonce: 7 }).reply,
            Some(HostMessage::Pong { nonce: 7 })
        );
        host.handle(hello());
        assert_eq!(
            host.handle(ClientMessage::Ping { nonce: 8 }).reply,
            Some(HostMessage::Pong { nonce: 8 })
        );
    }

    #[test]
    fn a_goodbye_ends_the_session_from_any_stage() {
        let mut host = session();
        let response = host.handle(ClientMessage::Goodbye {
            reason: "window closed".into(),
        });
        assert_eq!(response.effect, Some(Effect::Disconnect));
        assert!(host.is_ended());
    }

    #[test]
    fn nothing_is_answered_after_the_session_has_ended() {
        let mut host = streaming_as("boss", "run-it-all");
        host.handle(ClientMessage::Goodbye {
            reason: "done".into(),
        });
        assert_eq!(host.handle(click()), Response::default());
        assert_eq!(
            host.handle(ClientMessage::Ping { nonce: 1 }),
            Response::default()
        );
    }

    // ------------------------------------------------------------- streaming

    #[test]
    fn a_session_negotiates_the_best_shared_codec_and_the_profile_chroma() {
        let mut host = session();
        host.handle(hello()); // client offers H264 and OpenH264
        host.handle(login("driver", "drive-it"));

        let Some(HostMessage::SessionStarted(config)) = host
            .handle(ClientMessage::StartSession(SessionRequest {
                monitor: MonitorId::PRIMARY,
                profile: QualityProfile::Quality,
                max_resolution: None,
                audio: false,
            }))
            .reply
        else {
            panic!("expected a session");
        };

        assert_eq!(config.format.codec, Codec::H264, "the best both ends have");
        assert_eq!(config.format.resolution, Resolution::new(3840, 2160));
        assert_eq!(
            config.format.pixel_format,
            PixelFormat::Yuv444,
            "the Quality profile has to bring 4:4:4 with it"
        );
    }

    #[test]
    fn a_host_that_cannot_produce_full_chroma_does_not_claim_to() {
        // The client picks its colour-conversion path from this field. A host
        // that echoed the profile's wish while sending 4:2:0 would have the
        // client reading the chroma planes at twice their real size, which
        // renders as a smeared, mis-coloured picture and looks like a bug
        // anywhere but here.
        let mut host = subsampled_only();
        host.handle(hello());
        host.handle(login("driver", "drive-it"));

        let Some(HostMessage::SessionStarted(config)) = host
            .handle(ClientMessage::StartSession(SessionRequest {
                monitor: MonitorId::PRIMARY,
                profile: QualityProfile::Quality,
                max_resolution: None,
                audio: false,
            }))
            .reply
        else {
            panic!("expected a session");
        };

        assert_eq!(
            config.profile,
            QualityProfile::Quality,
            "the profile still applies"
        );
        assert_eq!(
            config.format.pixel_format,
            PixelFormat::Nv12,
            "promised a chroma layout this host cannot encode"
        );
    }

    #[test]
    fn switching_to_quality_on_a_subsampled_host_changes_the_profile_but_not_the_chroma() {
        let mut host = subsampled_only();
        host.handle(hello());
        host.handle(login("driver", "drive-it"));
        host.handle(start());

        let Some(HostMessage::SessionStarted(config)) = host
            .handle(ClientMessage::SetProfile(QualityProfile::Quality))
            .reply
        else {
            panic!("expected a reconfiguration");
        };

        assert_eq!(config.profile, QualityProfile::Quality);
        assert_eq!(config.format.pixel_format, PixelFormat::Nv12);
    }

    #[test]
    fn a_client_may_ask_for_less_than_native_but_never_more() {
        let mut host = session();
        host.handle(hello());
        host.handle(login("driver", "drive-it"));

        let Some(HostMessage::SessionStarted(config)) = host
            .handle(ClientMessage::StartSession(SessionRequest {
                monitor: MonitorId::PRIMARY,
                profile: QualityProfile::Latency,
                max_resolution: Some(Resolution::new(7680, 720)),
                audio: false,
            }))
            .reply
        else {
            panic!("expected a session");
        };
        // Width clamped to native, height honoured.
        assert_eq!(config.format.resolution, Resolution::new(3840, 720));
    }

    #[test]
    fn a_zero_sized_resolution_is_refused_rather_than_corrected() {
        let mut host = session();
        host.handle(hello());
        host.handle(login("driver", "drive-it"));

        let response = host.handle(ClientMessage::StartSession(SessionRequest {
            monitor: MonitorId::PRIMARY,
            profile: QualityProfile::Adaptive,
            max_resolution: Some(Resolution::new(1920, 0)),
            audio: false,
        }));
        assert_eq!(failure(&response), Some(ProtocolError::Unsupported));
        assert!(!host.is_streaming());
    }

    #[test]
    fn a_monitor_that_does_not_exist_is_refused() {
        let mut host = session();
        host.handle(hello());
        host.handle(login("boss", "run-it-all"));

        let response = host.handle(ClientMessage::StartSession(SessionRequest {
            monitor: MonitorId(200),
            profile: QualityProfile::Adaptive,
            max_resolution: None,
            audio: false,
        }));
        assert_eq!(failure(&response), Some(ProtocolError::Unsupported));
    }

    #[test]
    fn switching_monitors_keeps_the_resolution_cap_the_client_asked_for() {
        let mut host = session();
        host.handle(hello());
        host.handle(login("boss", "run-it-all"));
        host.handle(ClientMessage::StartSession(SessionRequest {
            monitor: MonitorId::PRIMARY,
            profile: QualityProfile::Adaptive,
            max_resolution: Some(Resolution::new(1280, 720)),
            audio: false,
        }));

        let Some(HostMessage::SessionStarted(config)) = host
            .handle(ClientMessage::SelectMonitor(MonitorId(1)))
            .reply
        else {
            panic!("expected a reconfiguration");
        };
        assert_eq!(config.monitor, MonitorId(1));
        assert_eq!(
            config.format.resolution,
            Resolution::new(1280, 720),
            "the cap was dropped on the monitor switch"
        );
    }

    #[test]
    fn changing_the_profile_restarts_the_pipeline_with_the_new_chroma() {
        let mut host = streaming_as("driver", "drive-it");
        let response = host.handle(ClientMessage::SetProfile(QualityProfile::Quality));

        let Some(HostMessage::SessionStarted(config)) = response.reply else {
            panic!("expected a reconfiguration");
        };
        assert_eq!(config.profile, QualityProfile::Quality);
        assert_eq!(config.format.pixel_format, PixelFormat::Yuv444);
        assert_eq!(response.effect, Some(Effect::Stream(config)));
    }

    #[test]
    fn a_second_start_session_is_refused_while_one_is_running() {
        let mut host = streaming_as("driver", "drive-it");
        assert_eq!(
            failure(&host.handle(start())),
            Some(ProtocolError::OutOfOrder)
        );
    }

    #[test]
    fn input_before_a_stream_exists_is_refused() {
        // There is no negotiated resolution yet, so a normalised pointer
        // position has nothing to scale against.
        let mut host = session();
        host.handle(hello());
        host.handle(login("driver", "drive-it"));

        let response = host.handle(click());
        assert_eq!(failure(&response), Some(ProtocolError::OutOfOrder));
        assert_eq!(response.effect, None);
    }

    #[test]
    fn a_malformed_input_event_is_dropped_even_from_an_operator() {
        // Dropped in silence, not answered. `Input` is fire-and-forget, so a
        // `Failed` here would land between some other request and its reply
        // and pair every answer after it with the wrong question — see
        // `ClientMessage::expects_reply`. The refusal is in the host's log,
        // where the operator can read it and the client cannot trip over it.
        let mut host = streaming_as("driver", "drive-it");
        for bad in [
            InputEvent::PointerMoveAbsolute {
                x: f32::NAN,
                y: 0.5,
            },
            InputEvent::PointerMoveAbsolute { x: 2.0, y: 0.5 },
            InputEvent::Scroll {
                dx: f32::INFINITY,
                dy: 0.0,
            },
            InputEvent::Text("x".repeat(pravera_proto::MAX_TEXT_BYTES + 1)),
        ] {
            let response = host.handle(ClientMessage::Input(bad.clone()));
            assert_eq!(response.effect, None, "{bad:?} was injected");
            assert_eq!(response.reply, None, "{bad:?} drew an unsolicited reply");
        }
    }

    // ------------------------------------------------------------ terminals

    #[test]
    fn an_operator_asking_for_a_terminal_is_told_to_accept_one() {
        // The promise has an order to it: the reply says the stream is coming
        // before the effect opens it, so the driver can spawn the terminal
        // knowing the client is already listening.
        let mut host = streaming_as("driver", "drive-it");
        let response = host.handle(ClientMessage::OpenTerminal {
            cols: 120,
            rows: 30,
        });
        assert!(matches!(response.reply, Some(HostMessage::TerminalStarted)));
        assert_eq!(
            response.effect,
            Some(Effect::OpenTerminal {
                cols: 120,
                rows: 30
            })
        );
    }

    #[test]
    fn a_viewer_is_refused_a_terminal_in_silence() {
        // A shell is keyboard control taken to its conclusion. The VIEW role
        // is refused it exactly as it is refused a keystroke, and for the same
        // reason: no second authority, no second answer.
        let mut host = streaming_as("looker", "look-only");
        let response = host.handle(ClientMessage::OpenTerminal {
            cols: 120,
            rows: 30,
        });
        assert_eq!(response.effect, None, "a shell was started for a viewer");
        assert!(matches!(
            response.reply,
            Some(HostMessage::Failed(ProtocolError::PermissionDenied))
        ));
    }

    #[test]
    fn a_terminal_of_nonsense_size_is_refused_before_anything_is_spawned() {
        // The size becomes a console allocation and a grid; a zero cell count
        // must stop at the gate rather than at either consumer.
        let mut host = streaming_as("driver", "drive-it");
        let response = host.handle(ClientMessage::OpenTerminal { cols: 0, rows: 30 });
        assert_eq!(response.effect, None);
        assert!(matches!(
            response.reply,
            Some(HostMessage::Failed(ProtocolError::Malformed))
        ));
    }

    #[test]
    fn a_malformed_message_that_is_waited_on_is_answered_rather_than_dropped() {
        // The other half of the rule. A client blocked on a reply must get one
        // even when its request was nonsense, or it waits forever.
        let mut host = streaming_as("driver", "drive-it");
        let response = host.handle(ClientMessage::SetClipboard {
            text: "x".repeat(pravera_proto::MAX_CLIPBOARD_BYTES + 1),
        });
        assert_eq!(failure(&response), Some(ProtocolError::Malformed));
        assert_eq!(response.effect, None);
    }

    // -------------------------------------------------------- negotiation

    #[test]
    fn codec_negotiation_prefers_the_best_and_drops_the_rest() {
        assert_eq!(
            negotiate(
                &[Codec::H265, Codec::H264, Codec::OpenH264],
                &[Codec::OpenH264, Codec::H264]
            ),
            vec![Codec::H264, Codec::OpenH264]
        );
        assert!(negotiate(&[Codec::H264], &[Codec::Av1]).is_empty());
    }

    #[test]
    fn this_machine_negotiates_the_fastest_encoder_it_owns() {
        // The whole point of the hardware encoder is that a real session picks
        // it. Both halves are the real lists this build produces, so if the
        // capability probe, the preference order, or the client's accept list
        // ever disagree, this fails rather than quietly costing six times the
        // encode time per frame.
        let host = pravera_codec::encodable();
        let client = pravera_codec::decodable();
        let shared = negotiate(&host, &client);

        assert!(
            !shared.is_empty(),
            "host {host:?} and client {client:?} share no codec at all"
        );
        let chosen = shared[0];
        if host.contains(&Codec::H264) {
            assert_eq!(
                chosen,
                Codec::H264,
                "a machine with a hardware encoder negotiated {} instead",
                chosen.name()
            );
        } else {
            assert_eq!(chosen, Codec::OpenH264);
        }
    }

    #[test]
    fn a_client_advertising_the_same_codec_endlessly_produces_a_short_list() {
        let spam = vec![Codec::H264; 10_000];
        let shared = negotiate(&[Codec::H265, Codec::H264], &spam);
        assert_eq!(shared, vec![Codec::H264]);
    }

    #[test]
    fn a_resolution_request_is_clamped_to_what_the_display_has() {
        let native = Resolution::new(2560, 1440);
        assert_eq!(clamp(native, None), Some(native));
        assert_eq!(
            clamp(native, Some(Resolution::new(1280, 720))),
            Some(Resolution::new(1280, 720))
        );
        assert_eq!(
            clamp(native, Some(Resolution::new(9999, 9999))),
            Some(native)
        );
        assert_eq!(clamp(native, Some(Resolution::new(0, 720))), None);
        assert_eq!(clamp(native, Some(Resolution::new(720, 0))), None);
    }
}
