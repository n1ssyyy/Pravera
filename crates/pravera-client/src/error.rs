//! What can go wrong on the client side of a session.

use pravera_core::Codec;

pub type Result<T, E = ClientError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The host speaks a different protocol version.
    ///
    /// Carries both numbers because the only useful thing a person can do
    /// about this is update whichever end is older, and they need to know
    /// which that is.
    #[error("the host speaks protocol version {theirs}; this client speaks {ours}")]
    VersionMismatch { ours: u16, theirs: u16 },

    /// The login was refused.
    ///
    /// Reasonless by design — the host will not say whether the account exists,
    /// is disabled, or simply had the wrong password, because answering that
    /// for an unauthenticated caller turns the login into a way to enumerate
    /// accounts. There is nothing more to report here because there is nothing
    /// more the host said.
    #[error("the host refused those credentials")]
    Denied,

    /// The host and this client have no codec in common.
    #[error("no shared codec: the host offers {offered:?}")]
    NoSharedCodec { offered: Vec<Codec> },

    /// The host refused a request outright.
    #[error("the host refused: {0}")]
    Refused(pravera_proto::ProtocolError),

    /// The host ended the session.
    #[error("the host ended the session: {0}")]
    Ended(String),

    /// A reply arrived that does not belong to the request that was sent.
    ///
    /// The control stream is strictly request-response, so this means either a
    /// host bug or a peer that is not really Pravera. Either way the session
    /// cannot be trusted to continue.
    #[error("the host answered {got} where {expected} was expected")]
    Unexpected {
        expected: &'static str,
        got: &'static str,
    },

    /// An operation was attempted before the handshake reached the state that
    /// allows it — starting a stream before logging in, for instance.
    #[error("not possible yet: {0}")]
    TooSoon(&'static str),

    /// A file could not be browsed or moved.
    ///
    /// Kept as the host's own category rather than collapsed to a string: the
    /// interface uses it to decide between offering "try again" and asking the
    /// person to pick somewhere else, and that distinction is real.
    #[error("{0}")]
    File(pravera_proto::FileError),

    #[error(transparent)]
    Transport(#[from] pravera_transport::TransportError),

    #[error(transparent)]
    Codec(#[from] pravera_codec::CodecError),

    #[error("protocol: {0}")]
    Protocol(#[from] pravera_proto::ProtocolError),
}

impl ClientError {
    /// Whether reconnecting stands a chance of working.
    ///
    /// A refused password will be refused again; a lost connection may not be.
    /// The UI uses this to decide between offering "retry" and asking the
    /// person to change something.
    pub fn is_worth_retrying(&self) -> bool {
        use pravera_transport::TransportError as T;
        match self {
            // A wrong password stays wrong; a mismatched version and a missing
            // codec both need someone to change something first.
            ClientError::Denied
            | ClientError::VersionMismatch { .. }
            | ClientError::NoSharedCodec { .. }
            | ClientError::TooSoon(_) => false,

            // The host has already made this judgement about its own
            // filesystem, and it is better placed to: a missing file stays
            // missing, a locked one may not.
            ClientError::File(error) => error.is_worth_retrying(),

            // A key that is not on the curve is a typo, and a path that will
            // not carry datagrams needs a different network, not another go.
            ClientError::Transport(
                T::InvalidPeerKey | T::DatagramsUnsupported | T::WrongProtocol { .. } | T::Closed,
            ) => false,

            _ => true,
        }
    }
}

impl From<ClientError> for pravera_core::Error {
    fn from(error: ClientError) -> Self {
        match &error {
            ClientError::Transport(_) => pravera_core::Error::Transport(error.to_string()),
            ClientError::Codec(_) => pravera_core::Error::Codec(error.to_string()),
            ClientError::Denied => pravera_core::Error::AuthFailed,
            _ => pravera_core::Error::Protocol(error.to_string()),
        }
    }
}

/// A one-word name for a host message, for [`ClientError::Unexpected`].
pub(crate) fn describe(message: &pravera_proto::HostMessage) -> &'static str {
    use pravera_proto::HostMessage as M;
    match message {
        M::Welcome(_) => "Welcome",
        M::AuthResult(_) => "AuthResult",
        M::SessionStarted(_) => "SessionStarted",
        M::Monitors(_) => "Monitors",
        M::Pong { .. } => "Pong",
        M::Clipboard(_) => "Clipboard",
        M::ClipboardSet { .. } => "ClipboardSet",
        M::TerminalStarted => "TerminalStarted",
        M::SasSent => "SasSent",
        M::Failed(_) => "Failed",
        M::Goodbye { .. } => "Goodbye",
    }
}
