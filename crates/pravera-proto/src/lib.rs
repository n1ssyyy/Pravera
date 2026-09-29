//! The Pravera wire protocol.
//!
//! Two channels run over one QUIC connection, and the split is the whole design:
//!
//! - **Control** rides a reliable, ordered stream. Handshake, authentication,
//!   session negotiation, input, and clipboard. Everything here must arrive and
//!   must arrive in order.
//! - **Media** rides unreliable datagrams ([RFC 9221]). Video and audio. A lost
//!   packet must never stall the ones behind it, which is exactly what a
//!   reliable stream would do: one dropped frame would hold up every frame
//!   after it while the sender retransmits something already too late to show.
//!
//! [RFC 9221]: https://www.rfc-editor.org/rfc/rfc9221.html
//!
//! ## Wire format
//!
//! Control messages are [postcard], a compact non-self-describing encoding.
//! Non-self-describing is the important part: field names are not transmitted,
//! so the two ends must agree exactly on the shape of every type. That makes
//! the protocol cheap and makes version negotiation non-optional. See
//! [`VERSION`].
//!
//! Media chunks do **not** use postcard. They carry a hand-laid fixed header
//! (see [`frame::ChunkHeader`]) because the byte layout of the hot path should
//! be visible and pinned rather than left to a derive.
//!
//! ## Everything here is attacker-controlled
//!
//! Every byte this crate parses arrives from the network. A peer is
//! authenticated only *after* the handshake, so the handshake itself parses
//! bytes from a stranger, and even an authenticated peer may be running a
//! patched client. So:
//!
//! - Control messages are capped at [`MAX_CONTROL_MESSAGE`]; a length prefix is
//!   never trusted enough to allocate against.
//! - [`frame::Reassembler`] bounds how many partial frames it will hold, so a
//!   peer cannot exhaust memory by opening frames it never finishes.
//! - Chunk headers are validated for internal consistency before use.
//!
//! And the rule that outranks all of them: **permissions in this protocol are
//! advisory to the client and authoritative only on the host.** The permission
//! set in [`control::AuthResult`] is sent so a client can grey out controls it
//! does not have. The host re-checks every request against its own record at
//! dispatch time, so a client patched to believe it holds `ADMIN` gains nothing.

pub mod codec;
pub mod control;
pub mod error;
pub mod files;
pub mod frame;
pub mod terminal;

pub use codec::{
    body_length, decode, decode_framed, encode, split_frame, LENGTH_PREFIX, MAX_CONTROL_MESSAGE,
};
pub use control::{
    AuthResult, ClientMessage, ClipboardSeq, ClipboardUpdate, Credentials, Hello, HostMessage,
    InputEvent, KeyCode, Monitor, MonitorId, PointerButton, SessionConfig, SessionRequest, Welcome,
    MAX_CLIPBOARD_BYTES, MAX_TEXT_BYTES,
};
pub use error::{ProtocolError, Result};
pub use files::{
    is_safe_path, Entry, EntryKind, FileError, FileReply, FileRequest, Listing, Location,
    HASH_BYTES, MAX_ENTRIES, MAX_PATH_BYTES, TRANSFER_CHUNK,
};
pub use frame::{
    ChunkFlags, ChunkHeader, Frame, FrameMeta, Reassembler, MAX_CHUNK_PAYLOAD, MAX_FRAME_BYTES,
    SAFE_DATAGRAM,
};
pub use terminal::{
    TerminalIn, TerminalOut, MAX_COLUMNS, MAX_INPUT_CHUNK, MAX_OUTPUT_CHUNK, MAX_ROWS,
};

/// The protocol version.
///
/// Bumped on any change that alters the shape of a message. Because postcard
/// does not transmit field names, a peer speaking a different version does not
/// fail cleanly: it silently misreads one field as another. There is no partial
/// compatibility to be had, so [`Hello`] carries this and the host refuses
/// anything that does not match exactly.
pub const VERSION: u16 = 2;

/// ALPN identifier negotiated during the TLS handshake.
///
/// Carries the version too, so a mismatched peer is rejected by QUIC before a
/// single application byte is exchanged. [`Hello`] checks it a second time
/// because the ALPN only proves what the peer *claims* to speak.
pub const ALPN: &[u8] = b"pravera/2";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_alpn_names_the_version_it_carries() {
        // If these drift, two incompatible builds would complete a TLS
        // handshake and only fail later, deep in a misparsed message.
        let alpn = std::str::from_utf8(ALPN).expect("ALPN must be printable");
        let (name, version) = alpn.split_once('/').expect("ALPN must be name/version");
        assert_eq!(name, "pravera");
        assert_eq!(version.parse::<u16>().unwrap(), VERSION);
    }
}
