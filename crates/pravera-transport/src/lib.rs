//! QUIC transport for Pravera, over [iroh].
//!
//! One connection carries both channels the protocol needs:
//!
//! - a reliable bidirectional stream for control ([`ClientControl`] /
//!   [`HostControl`]),
//! - unreliable datagrams for video and audio ([`Session::send_media`]),
//! - a fresh reliable stream per file transfer or directory listing
//!   ([`BulkStream`]), so a large copy never sits in front of a keystroke.
//!
//! ## The device key is the TLS identity
//!
//! iroh authenticates a QUIC connection with the endpoint's ed25519 key, so
//! dialling a device and proving that it is that device are the same operation.
//! There is no certificate authority, nothing to enrol, and nothing to expire.
//! A [`Session`] that exists at all is a session to the holder of that private
//! key.
//!
//! ## What a session is not
//!
//! An established session proves *which machine*. It proves nothing about *who
//! is at it*, and it grants nothing. Username, password and permissions are a
//! separate conversation on the control stream, enforced by `pravera-host`.
//!
//! The same rule covers every route. Being on the same tailnet, or plugged into
//! the same cable, changes how the packets travel and changes nothing about
//! authorisation.
//!
//! ## Everything this crate reads is attacker-controlled
//!
//! The peer is unauthenticated until the handshake completes, and even after it
//! completes the peer may be running a patched client. `pravera-proto` does the
//! bounds checking; this crate's job is to never hand it an unbounded read. In
//! particular, a control message body is only allocated after its length prefix
//! has been checked against `pravera_proto::MAX_CONTROL_MESSAGE`.
//!
//! [iroh]: https://iroh.computer

pub mod bulk;
pub mod control;
pub mod cursor;
pub mod endpoint;
pub mod error;
pub mod peer;
pub mod session;

pub use bulk::BulkStream;
pub use control::{ClientControl, HostControl};
pub use cursor::{CursorReceiver, CursorSender};
pub use endpoint::{Reachability, Transport, ALPN};
pub use error::{Result, TransportError};
pub use peer::{PeerAddress, PeerKey};
pub use session::{Role, Route, RouteKind, Session};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transport_alpn_is_the_protocol_alpn() {
        // Two constants for one string would eventually drift, and the failure
        // would be an unexplained handshake rejection.
        assert_eq!(ALPN, pravera_proto::ALPN);
    }
}
