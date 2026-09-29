//! Errors that can be named on the wire.
//!
//! Every variant here may be sent to a peer, which constrains what they are
//! allowed to say. An error message is an oracle: it tells whoever is on the
//! other end something about the host's internal state, and before
//! authentication that "whoever" is a stranger. So these are deliberately
//! coarse. The detailed reason is logged host-side, where only the operator
//! sees it.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, ProtocolError>;

#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
pub enum ProtocolError {
    /// The peer speaks a different version. Both numbers are safe to disclose:
    /// the ALPN already carried ours in cleartext.
    #[error("protocol version mismatch: we speak {ours}, peer speaks {theirs}")]
    VersionMismatch { ours: u16, theirs: u16 },

    /// A message arrived that does not belong in the current state, such as
    /// input before authentication.
    #[error("unexpected message for the current state")]
    OutOfOrder,

    /// The peer must authenticate before doing this.
    ///
    /// Note this is *not* returned for a failed password: that is an
    /// [`crate::control::AuthResult`], and it deliberately says less.
    #[error("not authenticated")]
    Unauthenticated,

    /// Authenticated, but the granted role does not permit the request.
    ///
    /// Safe to be specific here: the peer has already proven who it is, and
    /// telling a viewer that it may not type is useful rather than leaky.
    #[error("permission denied")]
    PermissionDenied,

    /// A length prefix or chunk header was self-inconsistent, oversized, or
    /// otherwise unparseable.
    #[error("malformed message")]
    Malformed,

    /// The requested monitor, codec or resolution is not on offer.
    #[error("unsupported request")]
    Unsupported,

    /// The host cannot start another session right now.
    #[error("host is busy")]
    Busy,

    /// Something failed on the host that the peer is not entitled to know
    /// about. The real cause is in the host's log.
    #[error("internal host error")]
    Internal,
}

impl From<postcard::Error> for ProtocolError {
    /// Any decode failure collapses to [`ProtocolError::Malformed`].
    ///
    /// postcard's own errors describe exactly where parsing gave up, which is a
    /// gift to anyone probing the protocol. It is worth logging and not worth
    /// transmitting.
    fn from(_: postcard::Error) -> Self {
        ProtocolError::Malformed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decode_failure_never_describes_itself_to_the_peer() {
        let detailed = postcard::from_bytes::<u32>(&[]).unwrap_err();
        let on_the_wire = ProtocolError::from(detailed);
        assert_eq!(on_the_wire, ProtocolError::Malformed);
        assert_eq!(on_the_wire.to_string(), "malformed message");
    }

    #[test]
    fn no_error_message_leaks_a_path_or_an_identifier() {
        // These strings go to an unauthenticated stranger. A file path, user
        // name or address in one would be a free reconnaissance signal.
        let all = [
            ProtocolError::VersionMismatch { ours: 1, theirs: 2 },
            ProtocolError::OutOfOrder,
            ProtocolError::Unauthenticated,
            ProtocolError::PermissionDenied,
            ProtocolError::Malformed,
            ProtocolError::Unsupported,
            ProtocolError::Busy,
            ProtocolError::Internal,
        ];
        for error in all {
            let text = error.to_string();
            assert!(!text.contains('/'), "{text:?} looks like it carries a path");
            assert!(
                !text.contains('\\'),
                "{text:?} looks like it carries a path"
            );
            assert!(
                !text.contains('@'),
                "{text:?} looks like it carries an identifier"
            );
        }
    }

    #[test]
    fn errors_survive_a_round_trip() {
        let original = ProtocolError::VersionMismatch { ours: 1, theirs: 9 };
        let bytes = postcard::to_allocvec(&original).unwrap();
        assert_eq!(
            postcard::from_bytes::<ProtocolError>(&bytes).unwrap(),
            original
        );
    }
}
