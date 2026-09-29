//! Messages on the reliable control stream.
//!
//! The conversation has a fixed shape, and the host enforces it:
//!
//! ```text
//! client                                host
//!   |------------- Hello ---------------->|   version check
//!   |<------------ Welcome ---------------|   device id, codecs, monitors
//!   |--------- Authenticate ------------->|   argon2id verify
//!   |<----------- AuthResult -------------|   role and permission set
//!   |--------- StartSession ------------->|   monitor, profile
//!   |<-------- SessionStarted ------------|   negotiated frame format
//!   |                                     |
//!   |------------- Input ---------------->|   permission-checked per message
//!   |<============ media datagrams =======|
//! ```
//!
//! Anything arriving out of that order is [`crate::ProtocolError::OutOfOrder`].
//! The host never assumes a step happened because the next one arrived.

use pravera_core::{AudioFormat, Codec, FrameFormat, Permission, QualityProfile, Resolution};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::ProtocolError;
use crate::terminal::{is_valid_size, REQUIRED_PERMISSION};

// ---------------------------------------------------------------- handshake

/// Opening message. Establishes that both ends speak the same protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Must equal [`crate::VERSION`] exactly. See that constant for why there
    /// is no compatibility range.
    pub version: u16,
    /// What to show in the host's session list. A display hint with no
    /// authority: it is whatever the client chose to type.
    pub client_name: String,
    /// Decoders this client actually has, best first.
    pub codecs: Vec<Codec>,
}

/// The host's answer to [`Hello`].
///
/// Sent before authentication, so it must contain nothing an unauthenticated
/// stranger should not learn. The device ID is fine: it is derived from the
/// public key the peer already saw during the TLS handshake. The monitor list
/// is *not* fine, which is why it is absent here and arrives only after a role
/// holding `MULTI_MONITOR` has been granted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub version: u16,
    /// The host's own name. Again a display hint, not an identity claim: the
    /// identity is the ed25519 key proven by the TLS handshake.
    pub host_name: String,
    /// Encoders this host has, best first.
    pub codecs: Vec<Codec>,
}

/// Username and password, in the clear inside the QUIC connection.
///
/// "In the clear" is doing real work in that sentence. The bytes are protected
/// by TLS 1.3 in transit and by nothing at all in memory, so this type zeroes
/// itself on drop and refuses to print its password.
///
/// The host never stores what arrives here. It runs Argon2id against the stored
/// PHC string and drops it.
///
/// The derived `PartialEq` is **not** constant-time and must never be used to
/// check a password. Verification belongs to `pravera-auth`, which compares
/// through Argon2's own verifier; equality here exists for tests and for the
/// enclosing message enums.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    /// Prints the username and elides the password.
    ///
    /// The derived impl would put a plaintext password into any log line that
    /// formatted a message, and the messages that carry credentials are exactly
    /// the ones a developer reaches for `{:?}` on while debugging a handshake.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// The outcome of an authentication attempt.
///
/// ## Why refusal carries no reason
///
/// There is one failure variant, and it says nothing. No "no such user", no
/// "account disabled", no "wrong password". Each of those would confirm to an
/// unauthenticated caller whether a given username exists, turning the login
/// into an account-enumeration oracle. `pravera-auth` already defends the
/// timing side of this by verifying against a dummy hash when the user does not
/// exist; collapsing the reply preserves that on the semantic side.
///
/// The host logs which it was. The peer is told only that it failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthResult {
    Granted {
        /// Echoed back so a client that sent the wrong thing can see what stuck.
        username: String,
        role: String,
        /// What the host granted. A **display hint**: it lets the client grey
        /// out what it cannot do. The host re-checks every request against its
        /// own record, so editing this client-side buys nothing.
        permissions: Permission,
    },
    /// Refused. Deliberately silent about why.
    Denied,
}

impl AuthResult {
    /// The granted permissions, or an empty set when refused.
    pub fn permissions(&self) -> Permission {
        match self {
            AuthResult::Granted { permissions, .. } => *permissions,
            AuthResult::Denied => Permission::empty(),
        }
    }

    pub fn is_granted(&self) -> bool {
        matches!(self, AuthResult::Granted { .. })
    }
}

// ------------------------------------------------------------------ session

/// Identifies one of the host's displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MonitorId(pub u8);

impl MonitorId {
    /// The display a session lands on when the client did not choose.
    pub const PRIMARY: MonitorId = MonitorId(0);
}

/// One of the host's displays.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Monitor {
    pub id: MonitorId,
    pub name: String,
    pub resolution: Resolution,
    /// Position in the host's virtual desktop, which may be negative when a
    /// display sits left of or above the primary.
    pub position: (i32, i32),
    /// The host's DPI scaling, so the client can size its window sensibly.
    pub scale: f32,
    pub primary: bool,
}

/// What the client wants to watch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRequest {
    pub monitor: MonitorId,
    pub profile: QualityProfile,
    /// Cap the stream below the host's native resolution. `None` means native.
    pub max_resolution: Option<Resolution>,
    /// Whether the client would also like to hear the host.
    ///
    /// A request, not a claim: the host grants it only if the account holds
    /// [`Permission::AUDIO`] and this machine can actually tap its own mixer.
    /// A client that sets this and is given [`SessionConfig::audio`] of
    /// `None` has been refused, and shows no audio controls.
    pub audio: bool,
}

/// What the host actually agreed to.
///
/// Never assume this matches the request. The host may downgrade the codec, the
/// resolution or the profile depending on what its encoder can do, and the
/// client renders what it is told rather than what it asked for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionConfig {
    pub monitor: MonitorId,
    pub format: FrameFormat,
    pub profile: QualityProfile,
    /// The audio stream the host agreed to send, if it agreed to any.
    ///
    /// `None` covers every reason there might be no sound — not asked for,
    /// not permitted, no audio device — deliberately, because the client has
    /// nothing to do differently in any of those cases and the host should not
    /// describe its hardware to a peer that was refused.
    pub audio: Option<AudioFormat>,
}

// -------------------------------------------------------------------- input

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PointerButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

/// A physical key, identified by its USB HID usage code (usage page 0x07).
///
/// A HID usage rather than a named enum, because that is the layer both target
/// platforms already speak: Windows scancodes and Linux evdev codes each map to
/// and from HID usages by a fixed table. It also means a key Pravera has never
/// heard of still travels correctly instead of falling off the end of an enum.
///
/// This is a *physical* key, not a character. `Text` carries what was actually
/// typed, which is what IMEs and dead keys need.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeyCode(pub u16);

/// Something the user did, to be replayed on the host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    /// Pointer position as a fraction of the captured surface, x and y in
    /// `0.0..=1.0`.
    ///
    /// Normalised rather than in pixels so a client rendering in a resized
    /// window does not have to know the host's resolution, and so the mapping
    /// stays correct if the host's resolution changes mid-session.
    PointerMoveAbsolute {
        x: f32,
        y: f32,
    },

    /// Raw pointer motion in host pixels.
    ///
    /// The one games need. Windows `SendInput` cannot emit true relative motion
    /// (it always routes through the cursor pipeline), which is why the plan
    /// keeps a kernel driver behind the `InputSink` trait. Linux `uinput`
    /// delivers this correctly today.
    PointerMoveRelative {
        dx: i32,
        dy: i32,
    },

    PointerButton {
        button: PointerButton,
        pressed: bool,
    },

    /// Scroll in wheel detents. Fractional for trackpads.
    Scroll {
        dx: f32,
        dy: f32,
    },

    Key {
        code: KeyCode,
        pressed: bool,
    },

    /// Text the client's own IME already composed. Sent instead of keystrokes
    /// when a dead key or input method produced a character no single physical
    /// key stands for.
    Text(String),
}

impl InputEvent {
    /// What a peer must hold to be allowed to send this.
    ///
    /// Advisory on the client and authoritative on the host: `pravera-host`
    /// calls this at dispatch and refuses anything the session's role does not
    /// cover. Every variant needs `CONTROL`; the method exists so that a future
    /// event needing something stronger cannot be added without deciding what.
    pub fn required_permission(&self) -> Permission {
        match self {
            InputEvent::PointerMoveAbsolute { .. }
            | InputEvent::PointerMoveRelative { .. }
            | InputEvent::PointerButton { .. }
            | InputEvent::Scroll { .. }
            | InputEvent::Key { .. }
            | InputEvent::Text(_) => Permission::CONTROL,
        }
    }

    /// Whether an absolute pointer position is inside the surface.
    ///
    /// Coordinates arrive from the network and land in a scaling calculation on
    /// the host, so a NaN or an out-of-range value must be rejected rather than
    /// clamped: clamping a NaN silently parks the cursor in a corner.
    pub fn is_well_formed(&self) -> bool {
        match self {
            InputEvent::PointerMoveAbsolute { x, y } => {
                x.is_finite() && y.is_finite() && (0.0..=1.0).contains(x) && (0.0..=1.0).contains(y)
            }
            InputEvent::Scroll { dx, dy } => dx.is_finite() && dy.is_finite(),
            InputEvent::Text(text) => !text.is_empty() && text.len() <= MAX_TEXT_BYTES,
            _ => true,
        }
    }
}

/// Ceiling on a single [`InputEvent::Text`].
///
/// A paste is a clipboard operation, gated by `CLIPBOARD_WRITE`. Text input is
/// for composed characters, so a few hundred bytes is generous; without a cap,
/// `CONTROL` alone would let a peer push unbounded strings through the
/// keyboard path and bypass the clipboard permission entirely.
pub const MAX_TEXT_BYTES: usize = 512;

// ---------------------------------------------------------------- clipboard

/// Where the host's clipboard has got to.
///
/// Opaque, and only ever compared for equality. On Windows it is the value of
/// `GetClipboardSequenceNumber`, which the OS moves on every change by anyone;
/// elsewhere it is derived from the content. Either way it is a cache key the
/// host mints and the client hands back, not a count of anything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClipboardSeq(pub u64);

/// The host's answer to [`ClientMessage::GetClipboard`].
///
/// Every variant except `Unchanged` carries the sequence the client should ask
/// with next time. That includes the ones that produce no clipboard write:
/// without a sequence to move on to, a client would ask about the same image
/// forever.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardUpdate {
    /// Nothing has happened since the sequence the client asked with.
    Unchanged,
    Text {
        seq: ClipboardSeq,
        text: String,
    },
    /// The clipboard changed to something this version does not carry — an
    /// image, a file list, or nothing at all.
    ///
    /// Not an error. The client records the sequence and stops asking about
    /// this particular change, and the person sees their own clipboard rather
    /// than a stale copy of the host's.
    Uncarried {
        seq: ClipboardSeq,
    },
}

impl ClipboardUpdate {
    /// The sequence to ask with next, if this update moved it.
    pub fn seq(&self) -> Option<ClipboardSeq> {
        match self {
            ClipboardUpdate::Unchanged => None,
            ClipboardUpdate::Text { seq, .. } | ClipboardUpdate::Uncarried { seq } => Some(*seq),
        }
    }
}

/// Ceiling on one clipboard transfer.
///
/// Well under [`crate::MAX_CONTROL_MESSAGE`], because the control stream is
/// also carrying input: a clipboard payload large enough to take milliseconds
/// on the wire would sit in front of every keystroke behind it. Text longer
/// than this is refused rather than truncated — a silently shortened paste is
/// worse than one that did not happen.
pub const MAX_CLIPBOARD_BYTES: usize = 256 * 1024;

// ----------------------------------------------------------------- messages

/// Client to host, on the control stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMessage {
    Hello(Hello),
    Authenticate(Credentials),
    StartSession(SessionRequest),
    /// Ask for the display list. Requires `MULTI_MONITOR`.
    ListMonitors,
    SelectMonitor(MonitorId),
    SetProfile(QualityProfile),
    Input(InputEvent),
    /// The decoder lost sync and needs a fresh keyframe.
    RequestKeyframe,

    /// Has the host's clipboard changed since `since`? Requires
    /// `CLIPBOARD_READ`.
    ///
    /// Asked rather than pushed. The control stream is strictly
    /// request-response — see [`ClientMessage::expects_reply`] — and an
    /// unsolicited update from the host would land between some other request
    /// and its answer, pairing every reply after it with the wrong question.
    GetClipboard {
        since: ClipboardSeq,
    },
    /// Put this on the host's clipboard. Requires `CLIPBOARD_WRITE`.
    SetClipboard {
        text: String,
    },
    /// Round-trip probe. The host echoes the nonce back unchanged.
    Ping {
        nonce: u64,
    },
    /// Start a shell on the host and open its terminal stream. Requires
    /// [`crate::terminal::REQUIRED_PERMISSION`].
    ///
    /// The request carries only a size; everything else about the shell —
    /// which one, which user, which working directory — belongs to the host.
    /// The answer is [`HostMessage::TerminalStarted`] or
    /// [`HostMessage::Failed`], after which the conversation moves to the
    /// terminal stream itself (see `crate::terminal`).
    OpenTerminal {
        cols: u16,
        rows: u16,
    },
    /// Ask the host to generate Ctrl+Alt+Del on the console (Secure Attention
    /// Sequence) so the lock screen can be dismissed without a physical
    /// keyboard. Requires `CONTROL` and, on the host, `SYSTEM` plus the
    /// `SoftwareSASGeneration` policy — see `pravera-service::secure`.
    SendSas,
    Goodbye {
        reason: String,
    },
}

impl ClientMessage {
    /// What the sender must hold for this to be honoured.
    ///
    /// The handshake messages return an empty set because they run before a
    /// permission set exists; the host gates those on connection state instead.
    pub fn required_permission(&self) -> Permission {
        match self {
            ClientMessage::Hello(_)
            | ClientMessage::Authenticate(_)
            | ClientMessage::Ping { .. }
            | ClientMessage::Goodbye { .. } => Permission::empty(),

            ClientMessage::StartSession(_)
            | ClientMessage::SetProfile(_)
            | ClientMessage::RequestKeyframe => Permission::VIEW,

            ClientMessage::ListMonitors | ClientMessage::SelectMonitor(_) => {
                Permission::MULTI_MONITOR
            }

            // Read and write are separate grants, and the direction is named
            // from the host's clipboard: `GetClipboard` reads it, `SetClipboard`
            // writes it. A role may hold either without the other — letting
            // somebody paste into a machine is not the same as letting them
            // take whatever is on it.
            ClientMessage::GetClipboard { .. } => Permission::CLIPBOARD_READ,
            ClientMessage::SetClipboard { .. } => Permission::CLIPBOARD_WRITE,

            ClientMessage::Input(event) => event.required_permission(),

            // See `crate::terminal::REQUIRED_PERMISSION` for why this is
            // CONTROL rather than a flag of its own.
            ClientMessage::OpenTerminal { .. } => REQUIRED_PERMISSION,

            ClientMessage::SendSas => Permission::CONTROL,
        }
    }

    /// Whether this message is internally consistent enough to act on.
    ///
    /// Checked on the host before anything is done with it. A peer may be
    /// running a patched client, so a length this end depends on is a length
    /// this end has to verify.
    pub fn is_well_formed(&self) -> bool {
        match self {
            ClientMessage::Input(event) => event.is_well_formed(),
            ClientMessage::SetClipboard { text } => text.len() <= MAX_CLIPBOARD_BYTES,
            // The size is the only field, and it becomes a grid allocation on
            // the client and a console resize on the host, so an absurd or
            // zero value has to stop here rather than at either consumer.
            ClientMessage::OpenTerminal { cols, rows } => is_valid_size(*cols, *rows),
            _ => true,
        }
    }

    /// Whether the host answers this on the control stream.
    ///
    /// The stream is strictly request-response for everything that returns
    /// `true`, and that property is what lets a client pair a reply with the
    /// request that caused it. Without it a refusal arriving between a request
    /// and its answer is read as the answer, and every later reply is off by
    /// one for the rest of the session.
    ///
    /// `Input` and `RequestKeyframe` return `false`. They are sent at input
    /// rates — hundreds a second while someone moves a mouse — so answering
    /// each one, *even to refuse it*, would put a reply on the stream for every
    /// movement. A client with no permission to send input could then make the
    /// host generate more traffic than the client sent, which is an
    /// amplification the host controls and should not offer.
    ///
    /// A refused fire-and-forget message is logged host-side and dropped. The
    /// client is not left guessing: it already holds the permission set from
    /// [`AuthResult::Granted`] and knows what its role allows.
    ///
    /// `Goodbye` is also unanswered, for the plainer reason that the sender has
    /// already stopped listening.
    pub fn expects_reply(&self) -> bool {
        !matches!(
            self,
            ClientMessage::Input(_)
                | ClientMessage::RequestKeyframe
                | ClientMessage::Goodbye { .. }
        )
    }

    /// Whether this may be sent before authenticating.
    pub fn is_pre_auth(&self) -> bool {
        matches!(
            self,
            ClientMessage::Hello(_)
                | ClientMessage::Authenticate(_)
                | ClientMessage::Goodbye { .. }
        )
    }
}

/// Host to client, on the control stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HostMessage {
    Welcome(Welcome),
    AuthResult(AuthResult),
    SessionStarted(SessionConfig),
    Monitors(Vec<Monitor>),
    Clipboard(ClipboardUpdate),
    /// The host's clipboard now holds what the client sent.
    ///
    /// Carries the sequence the write produced, and that is the whole point of
    /// answering at all: without it the client's next poll would be told the
    /// clipboard had changed, copy its own text back over itself, change its
    /// own clipboard, and push again — a loop between two machines that each
    /// think the other typed something.
    ClipboardSet {
        seq: ClipboardSeq,
    },
    Pong {
        nonce: u64,
    },
    /// A shell was started for [`ClientMessage::OpenTerminal`], and its
    /// terminal stream is about to be opened by the host.
    ///
    /// Arrives before the stream opens, so a client that receives this can go
    /// straight to accepting rather than polling. If the shell then fails to
    /// start, the host opens and immediately closes the stream — an empty
    /// terminal that ends at once, never one that silently never appears.
    TerminalStarted,
    /// The Secure Attention Sequence was generated. The console should now be
    /// on the sign-in screen.
    SasSent,
    Failed(ProtocolError),
    Goodbye {
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(value: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let bytes = postcard::to_allocvec(value).expect("encode");
        postcard::from_bytes(&bytes).expect("decode")
    }

    #[test]
    fn only_the_high_rate_messages_go_unanswered() {
        // Every message a client blocks on must be answered, or the client
        // waits forever. Every message it does not block on must not be, or a
        // refusal lands between a request and its reply and every answer after
        // it is paired with the wrong question.
        for message in [
            ClientMessage::Hello(Hello {
                version: crate::VERSION,
                client_name: "laptop".into(),
                codecs: vec![Codec::H264],
            }),
            ClientMessage::Authenticate(Credentials {
                username: "a".into(),
                password: "b".into(),
            }),
            ClientMessage::StartSession(SessionRequest {
                monitor: MonitorId::PRIMARY,
                profile: QualityProfile::Adaptive,
                max_resolution: None,
                audio: true,
            }),
            ClientMessage::ListMonitors,
            ClientMessage::SelectMonitor(MonitorId(1)),
            ClientMessage::SetProfile(QualityProfile::Quality),
            ClientMessage::GetClipboard {
                since: ClipboardSeq(0),
            },
            ClientMessage::SetClipboard { text: "hi".into() },
            ClientMessage::Ping { nonce: 1 },
            ClientMessage::OpenTerminal {
                cols: 120,
                rows: 40,
            },
        ] {
            assert!(message.expects_reply(), "{message:?}");
        }

        for message in [
            ClientMessage::Input(InputEvent::Scroll { dx: 0.0, dy: 1.0 }),
            ClientMessage::RequestKeyframe,
            ClientMessage::Goodbye {
                reason: "done".into(),
            },
        ] {
            assert!(!message.expects_reply(), "{message:?}");
        }
    }

    #[test]
    fn a_credential_never_prints_its_password() {
        let creds = Credentials {
            username: "operator".into(),
            password: "correct horse battery staple".into(),
        };
        let shown = format!("{creds:?}");
        assert!(
            shown.contains("operator"),
            "the username is useful in a log"
        );
        assert!(
            !shown.contains("correct horse battery staple"),
            "the password reached a log line: {shown}"
        );
        assert!(shown.contains("redacted"));
    }

    #[test]
    fn a_refusal_says_nothing_about_why() {
        // One failure variant, carrying no payload. If a `reason` field or a
        // second variant ever appears here, it becomes possible to ask this
        // host whether a username exists.
        let bytes = postcard::to_allocvec(&AuthResult::Denied).unwrap();
        assert_eq!(bytes.len(), 1, "a refusal grew a payload: {bytes:?}");
        assert!(!AuthResult::Denied.is_granted());
        assert_eq!(AuthResult::Denied.permissions(), Permission::empty());
    }

    #[test]
    fn a_grant_carries_the_permission_set_it_claims() {
        let granted = AuthResult::Granted {
            username: "ops".into(),
            role: "operator".into(),
            permissions: Permission::VIEW | Permission::CONTROL,
        };
        assert!(granted.is_granted());
        assert_eq!(
            granted.permissions(),
            Permission::VIEW | Permission::CONTROL
        );
        assert_eq!(round_trip(&granted), granted);
    }

    #[test]
    fn the_welcome_sent_before_auth_carries_only_what_a_stranger_may_see() {
        // Welcome is answered to anyone who completes a TLS handshake, before
        // a password has been offered. Monitor names leak hardware models and
        // frequently a person's name, so they live behind MULTI_MONITOR and
        // arrive in HostMessage::Monitors instead.
        //
        // Destructuring is the guard: adding a field to Welcome stops
        // compiling here, which forces whoever added it to decide whether a
        // stranger may see it.
        let Welcome {
            version,
            host_name,
            codecs,
        } = Welcome {
            version: crate::VERSION,
            host_name: "workshop".into(),
            codecs: vec![Codec::H264],
        };
        assert_eq!(version, crate::VERSION);
        assert_eq!(host_name, "workshop");
        assert_eq!(codecs, vec![Codec::H264]);
    }

    #[test]
    fn listing_monitors_needs_the_multi_monitor_grant() {
        assert_eq!(
            ClientMessage::ListMonitors.required_permission(),
            Permission::MULTI_MONITOR
        );
        assert_eq!(
            ClientMessage::SelectMonitor(MonitorId(1)).required_permission(),
            Permission::MULTI_MONITOR
        );
    }

    #[test]
    fn every_input_event_needs_control() {
        let events = [
            InputEvent::PointerMoveAbsolute { x: 0.5, y: 0.5 },
            InputEvent::PointerMoveRelative { dx: 3, dy: -2 },
            InputEvent::PointerButton {
                button: PointerButton::Left,
                pressed: true,
            },
            InputEvent::Scroll { dx: 0.0, dy: 1.0 },
            InputEvent::Key {
                code: KeyCode(0x04),
                pressed: true,
            },
            InputEvent::Text("e".into()),
        ];
        for event in events {
            assert_eq!(
                event.required_permission(),
                Permission::CONTROL,
                "{event:?} must not be sendable without CONTROL"
            );
            assert_eq!(
                ClientMessage::Input(event.clone()).required_permission(),
                Permission::CONTROL
            );
        }
    }

    #[test]
    fn a_viewer_can_watch_and_nothing_more() {
        let viewer = Permission::VIEW;
        assert!(viewer.allows(ClientMessage::RequestKeyframe.required_permission()));
        assert!(viewer.allows(
            ClientMessage::StartSession(SessionRequest {
                monitor: MonitorId::PRIMARY,
                profile: QualityProfile::Adaptive,
                max_resolution: None,
                audio: false,
            })
            .required_permission()
        ));
        assert!(!viewer.allows(
            ClientMessage::Input(InputEvent::PointerButton {
                button: PointerButton::Left,
                pressed: true
            })
            .required_permission()
        ));
        assert!(!viewer.allows(ClientMessage::ListMonitors.required_permission()));
    }

    #[test]
    fn only_the_handshake_may_precede_authentication() {
        assert!(ClientMessage::Hello(Hello {
            version: crate::VERSION,
            client_name: "laptop".into(),
            codecs: vec![],
        })
        .is_pre_auth());
        assert!(ClientMessage::Authenticate(Credentials {
            username: "a".into(),
            password: "b".into(),
        })
        .is_pre_auth());

        // Everything that does real work must not be reachable before login.
        assert!(!ClientMessage::RequestKeyframe.is_pre_auth());
        assert!(!ClientMessage::ListMonitors.is_pre_auth());
        assert!(!ClientMessage::Input(InputEvent::Scroll { dx: 0.0, dy: 1.0 }).is_pre_auth());
        assert!(!ClientMessage::SetProfile(QualityProfile::Latency).is_pre_auth());
        // A stranger must not be able to make the host spawn a shell.
        assert!(
            !ClientMessage::OpenTerminal {
                cols: 80,
                rows: 24
            }
            .is_pre_auth()
        );
    }

    #[test]
    fn a_pointer_position_outside_the_surface_is_refused() {
        assert!(InputEvent::PointerMoveAbsolute { x: 0.0, y: 1.0 }.is_well_formed());
        assert!(!InputEvent::PointerMoveAbsolute { x: 1.5, y: 0.5 }.is_well_formed());
        assert!(!InputEvent::PointerMoveAbsolute { x: -0.1, y: 0.5 }.is_well_formed());
    }

    #[test]
    fn a_non_finite_coordinate_is_refused_rather_than_clamped() {
        // These land in a multiply against the host's resolution. NaN clamps to
        // a corner silently; infinity produces a nonsense pixel. Both are
        // reachable by anyone who can send input, so both are rejected here.
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(!InputEvent::PointerMoveAbsolute { x: bad, y: 0.5 }.is_well_formed());
            assert!(!InputEvent::PointerMoveAbsolute { x: 0.5, y: bad }.is_well_formed());
            assert!(!InputEvent::Scroll { dx: bad, dy: 0.0 }.is_well_formed());
        }
    }

    #[test]
    fn text_input_cannot_be_used_as_an_ungated_paste() {
        // CONTROL lets you type. Pushing a megabyte through the keyboard path
        // would be a paste without CLIPBOARD_WRITE.
        assert!(InputEvent::Text("ä".into()).is_well_formed());
        assert!(!InputEvent::Text(String::new()).is_well_formed());
        assert!(!InputEvent::Text("x".repeat(MAX_TEXT_BYTES + 1)).is_well_formed());
        assert!(InputEvent::Text("x".repeat(MAX_TEXT_BYTES)).is_well_formed());
    }

    #[test]
    fn every_control_message_survives_a_round_trip() {
        let messages = [
            ClientMessage::Hello(Hello {
                version: crate::VERSION,
                client_name: "laptop".into(),
                codecs: vec![Codec::H265, Codec::H264],
            }),
            ClientMessage::StartSession(SessionRequest {
                monitor: MonitorId(2),
                profile: QualityProfile::Latency,
                max_resolution: Some(Resolution::new(1920, 1080)),
                audio: true,
            }),
            ClientMessage::Input(InputEvent::PointerMoveRelative { dx: -4, dy: 7 }),
            ClientMessage::GetClipboard {
                since: ClipboardSeq(9_001),
            },
            ClientMessage::SetClipboard {
                text: "ünïcödé and \u{1f600}".into(),
            },
            ClientMessage::Ping { nonce: u64::MAX },
            ClientMessage::OpenTerminal {
                cols: 80,
                rows: 24,
            },
            ClientMessage::Goodbye {
                reason: "window closed".into(),
            },
        ];
        for message in messages {
            assert_eq!(round_trip(&message), message);
        }
    }

    #[test]
    fn every_host_message_survives_a_round_trip() {
        let messages = [
            HostMessage::Welcome(Welcome {
                version: crate::VERSION,
                host_name: "workshop".into(),
                codecs: vec![Codec::Av1],
            }),
            HostMessage::AuthResult(AuthResult::Denied),
            HostMessage::Monitors(vec![Monitor {
                id: MonitorId::PRIMARY,
                name: "DELL U2720Q".into(),
                resolution: Resolution::new(3840, 2160),
                position: (-1920, 0),
                scale: 1.5,
                primary: true,
            }]),
            HostMessage::Clipboard(ClipboardUpdate::Text {
                seq: ClipboardSeq(4),
                text: "copied".into(),
            }),
            HostMessage::Clipboard(ClipboardUpdate::Unchanged),
            HostMessage::Clipboard(ClipboardUpdate::Uncarried {
                seq: ClipboardSeq(5),
            }),
            HostMessage::ClipboardSet {
                seq: ClipboardSeq(6),
            },
            HostMessage::Pong { nonce: 7 },
            HostMessage::TerminalStarted,
            HostMessage::Failed(ProtocolError::PermissionDenied),
            HostMessage::Pong { nonce: 7 },
        ];
        for message in messages {
            assert_eq!(round_trip(&message), message);
        }
    }

    #[test]
    fn each_direction_of_the_clipboard_is_a_separate_grant() {
        // Letting somebody paste into a machine is not the same as letting them
        // take whatever happens to be on it, and a role may hold either alone.
        assert_eq!(
            ClientMessage::GetClipboard {
                since: ClipboardSeq(0)
            }
            .required_permission(),
            Permission::CLIPBOARD_READ
        );
        assert_eq!(
            ClientMessage::SetClipboard { text: "x".into() }.required_permission(),
            Permission::CLIPBOARD_WRITE
        );
    }

    #[test]
    fn an_oversized_paste_is_refused_rather_than_truncated() {
        // A silently shortened paste is worse than one that did not happen: the
        // person believes they moved the whole thing.
        let fits = ClientMessage::SetClipboard {
            text: "x".repeat(MAX_CLIPBOARD_BYTES),
        };
        let does_not = ClientMessage::SetClipboard {
            text: "x".repeat(MAX_CLIPBOARD_BYTES + 1),
        };
        assert!(fits.is_well_formed());
        assert!(!does_not.is_well_formed());
    }

    #[test]
    fn every_update_that_changed_something_says_what_to_ask_with_next() {
        // Including the ones that produce no write. Without a sequence to move
        // on to, a client would ask about the same image forever.
        assert_eq!(ClipboardUpdate::Unchanged.seq(), None);
        assert_eq!(
            ClipboardUpdate::Text {
                seq: ClipboardSeq(3),
                text: String::new()
            }
            .seq(),
            Some(ClipboardSeq(3))
        );
        assert_eq!(
            ClipboardUpdate::Uncarried {
                seq: ClipboardSeq(4)
            }
            .seq(),
            Some(ClipboardSeq(4))
        );
    }

    #[test]
    fn opening_a_terminal_needs_the_control_grant() {
        // A shell is keyboard control taken to its conclusion, so it rides the
        // same grant rather than introducing a flag that every role holding
        // CONTROL would hold anyway.
        let open = ClientMessage::OpenTerminal {
            cols: 80,
            rows: 24,
        };
        assert_eq!(open.required_permission(), Permission::CONTROL);
        assert!(!Permission::VIEW.allows(open.required_permission()));
    }

    #[test]
    fn a_terminal_of_nonsense_size_is_refused_before_it_becomes_a_grid() {
        // The size becomes an allocation on both ends. Refusing it at the
        // message level means neither end ever has to defend against it.
        assert!(
            !ClientMessage::OpenTerminal { cols: 0, rows: 24 }.is_well_formed()
        );
        assert!(
            !ClientMessage::OpenTerminal {
                cols: 80,
                rows: 0
            }
            .is_well_formed()
        );
        assert!(ClientMessage::OpenTerminal {
            cols: 80,
            rows: 24
        }
        .is_well_formed());
    }

    #[test]
    fn a_credential_round_trips_intact() {
        // Zeroize on drop must not disturb serialisation of a live value.
        let creds = Credentials {
            username: "operator".into(),
            password: "hunter2-but-longer".into(),
        };
        assert_eq!(round_trip(&creds), creds);
    }
}
