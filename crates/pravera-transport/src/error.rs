//! Local failures in the transport layer.
//!
//! Nothing here is ever sent to a peer. These are diagnostics for the operator:
//! they name hosts, sockets and internal causes, and they carry the underlying
//! iroh or QUIC error as a source so a failure can be read back to its origin.
//!
//! When something has to be *told* to the other end, the host sends a
//! [`pravera_proto::ProtocolError`] instead, which is deliberately vague for
//! exactly the reason this type is not.

use pravera_proto::ProtocolError;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, TransportError>;

/// A boxed cause, so the transport does not re-export iroh's error taxonomy to
/// every crate that touches it.
type Source = Box<dyn std::error::Error + Send + Sync + 'static>;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("could not bind a local endpoint")]
    Bind(#[source] Source),

    #[error("could not reach the peer")]
    Unreachable(#[source] Source),

    #[error("the connection ended")]
    Lost(#[source] Source),

    #[error("the control stream failed")]
    Stream(#[source] Source),

    /// The peer finished the control stream cleanly. Ordinary at the end of a
    /// session; a problem only when it happens mid-handshake.
    #[error("the peer closed the control stream")]
    StreamClosed,

    #[error("a media datagram could not be sent")]
    Datagram(#[source] Source),

    /// The negotiated path will not carry datagrams at all, so media has
    /// nowhere to go. Worth its own variant because the fix is a network
    /// change, not a retry.
    #[error("this path will not carry datagrams, so media cannot flow over it")]
    DatagramsUnsupported,

    #[error("a {size} byte datagram exceeds the {limit} byte limit for this path")]
    DatagramTooLarge { size: usize, limit: usize },

    /// The 32 bytes offered as a peer key are not a point on the curve, so no
    /// such device can exist. A typo, not a network failure.
    #[error("that is not a valid ed25519 public key")]
    InvalidPeerKey,

    /// The peer completed a TLS handshake under a different application
    /// protocol. iroh only accepts ALPNs the endpoint was configured with, so
    /// this should be unreachable; it is checked because "should be
    /// unreachable" is a poor foundation for a security boundary.
    #[error("the peer negotiated {negotiated:?}, not the Pravera protocol")]
    WrongProtocol { negotiated: String },

    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    #[error("the endpoint is closed")]
    Closed,
}

impl TransportError {
    pub(crate) fn bind(source: impl Into<Source>) -> Self {
        TransportError::Bind(source.into())
    }

    pub(crate) fn unreachable(source: impl Into<Source>) -> Self {
        TransportError::Unreachable(source.into())
    }

    pub(crate) fn lost(source: impl Into<Source>) -> Self {
        TransportError::Lost(source.into())
    }

    pub(crate) fn stream(source: impl Into<Source>) -> Self {
        TransportError::Stream(source.into())
    }

    pub(crate) fn datagram(source: impl Into<Source>) -> Self {
        TransportError::Datagram(source.into())
    }
}
