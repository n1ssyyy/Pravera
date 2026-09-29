use std::fmt;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The one error type crossing crate boundaries. Leaf crates keep their own
/// richer errors and convert at the edge, so callers never match on a
/// platform-specific variant they cannot handle.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration: {0}")]
    Config(String),

    #[error("identity: {0}")]
    Identity(String),

    #[error("authentication failed")]
    AuthFailed,

    #[error("permission denied: {action} requires {required}")]
    PermissionDenied { action: String, required: String },

    #[error("transport: {0}")]
    Transport(String),

    #[error("capture: {0}")]
    Capture(String),

    #[error("codec: {0}")]
    Codec(String),

    #[error("input injection: {0}")]
    Input(String),

    #[error("protocol: {0}")]
    Protocol(String),

    /// A cable or link that physically cannot carry a session. Carries a
    /// human-readable explanation because this is surfaced directly in the UI —
    /// e.g. plain USB-C between two hosts.
    #[error("unusable link: {0}")]
    UnusableLink(String),

    #[error("peer is offline")]
    PeerOffline,

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn other(msg: impl fmt::Display) -> Self {
        Error::Other(msg.to_string())
    }

    /// True when retrying the same operation could plausibly succeed later.
    /// Used by the reconnect loop to decide between backoff and giving up.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Error::Transport(_) | Error::PeerOffline | Error::Io(_) | Error::Capture(_)
        )
    }
}
