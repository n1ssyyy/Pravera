//! One connection to one peer.
//!
//! A session carries both channels described in `pravera-proto`: a reliable
//! bidirectional stream for control, and unreliable datagrams for media.

use std::time::Duration;

use bytes::Bytes;
use iroh::endpoint::{Connection, VarInt};
use pravera_core::DeviceId;

use crate::bulk::BulkStream;
use crate::control::{ClientControl, ControlStream, HostControl};
use crate::endpoint::ALPN;
use crate::error::{Result, TransportError};
use crate::peer::PeerKey;

/// How the bytes are actually getting there.
///
/// Both fields are `Option` on purpose. A path takes a moment to settle after
/// the handshake, and reporting "relayed, 0 ms" during that window would be a
/// measurement nobody took. Unknown says unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Route {
    pub kind: Option<RouteKind>,
    pub rtt: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteKind {
    /// Straight to the peer's address. A cable, a LAN, or a hole-punched path.
    Direct,
    /// Through a relay. Still end to end encrypted, measurably slower.
    Relay,
}

impl Route {
    pub fn is_direct(&self) -> bool {
        self.kind == Some(RouteKind::Direct)
    }
}

/// An established, mutually authenticated QUIC connection.
///
/// "Authenticated" here means the peer proved possession of the private key
/// behind [`Session::peer_key`]. It says nothing about *who is sitting at* that
/// machine. That is the username and password, and it happens on the control
/// stream.
///
/// Cheap to clone: every clone is a handle to the same connection, which is how
/// a host runs its control loop and its media pump in separate tasks.
///
/// Dropping the **last** handle closes the connection immediately and discards
/// anything the peer has not read yet, so a session must be held for as long as
/// it is meant to live. Sending a final message and then returning from the
/// task that owns the session is a race the peer usually loses.
#[derive(Debug, Clone)]
pub struct Session {
    connection: Connection,
    peer: PeerKey,
}

impl Session {
    pub(crate) fn from_connection(connection: Connection) -> Result<Self> {
        // iroh only completes a handshake for an ALPN the endpoint was
        // configured with, so this cannot currently fail. It is checked anyway:
        // the day a second ALPN is added, this is the line that stops a peer
        // speaking the other protocol from being handed to the session code.
        let negotiated = connection.alpn();
        if negotiated != ALPN {
            return Err(TransportError::WrongProtocol {
                negotiated: String::from_utf8_lossy(negotiated).into_owned(),
            });
        }

        let peer = PeerKey::from(connection.remote_id());
        Ok(Session { connection, peer })
    }

    /// The public key the peer proved during the handshake.
    pub fn peer_key(&self) -> PeerKey {
        self.peer
    }

    pub fn peer_device_id(&self) -> DeviceId {
        self.peer.device_id()
    }

    /// The path currently in use, measured rather than assumed.
    ///
    /// This is what feeds the route meter in the interface, and it is read
    /// fresh each time: QUIC migrates between paths mid-session, so a session
    /// that started on a relay may be direct a second later.
    pub fn route(&self) -> Route {
        let paths = self.connection.paths();
        let Some(selected) = paths.iter().find(|path| path.is_selected()) else {
            return Route::default();
        };

        let kind = if selected.is_relay() {
            Some(RouteKind::Relay)
        } else if selected.is_ip() {
            Some(RouteKind::Direct)
        } else {
            // A custom transport. Real, but not one of the two things the
            // interface knows how to draw, so it stays unnamed rather than
            // being rounded to the nearest lie.
            None
        };

        Route {
            kind,
            rtt: Some(selected.rtt()),
        }
    }

    // ------------------------------------------------------------- control

    /// Open the control stream. The **client** side.
    ///
    /// QUIC does not put a stream on the wire until something is written to it,
    /// so the host's [`Session::accept_control`] stays pending until the client
    /// sends its first message. That is the correct order, and it is worth
    /// knowing: a host awaiting a control stream from a silent client is
    /// working, not hung.
    pub async fn open_control(&self) -> Result<ClientControl> {
        let (send, recv) = self
            .connection
            .open_bi()
            .await
            .map_err(TransportError::stream)?;
        Ok(ClientControl::new(ControlStream::new(send, recv)))
    }

    /// Accept the control stream. The **host** side.
    pub async fn accept_control(&self) -> Result<HostControl> {
        let (send, recv) = self
            .connection
            .accept_bi()
            .await
            .map_err(TransportError::stream)?;
        Ok(HostControl::new(ControlStream::new(send, recv)))
    }

    // ---------------------------------------------------------------- bulk

    /// Open a stream for one transfer or one listing.
    ///
    /// Ordinary bidirectional streams, the same kind the control stream uses.
    /// The **control stream must be established first**, on both ends: QUIC
    /// delivers accepted streams in the order the peer opened them, and that
    /// ordering is the only thing separating the control stream from the first
    /// bulk one. A host that started accepting bulk streams before its control
    /// stream would take the control stream for a transfer.
    pub async fn open_bulk(&self) -> Result<BulkStream> {
        let (send, recv) = self
            .connection
            .open_bi()
            .await
            .map_err(TransportError::stream)?;
        Ok(BulkStream::new(send, recv))
    }

    /// Accept the next bulk stream the peer opens.
    ///
    /// Pending until the peer starts one, which for a session where nobody
    /// touches the file browser is the whole session. That is the intended
    /// shape: this is awaited in a loop alongside everything else.
    pub async fn accept_bulk(&self) -> Result<BulkStream> {
        let (send, recv) = self
            .connection
            .accept_bi()
            .await
            .map_err(TransportError::stream)?;
        Ok(BulkStream::new(send, recv))
    }

    // --------------------------------------------------------------- media

    /// Largest datagram this path will carry, if it carries datagrams at all.
    ///
    /// Smaller than the UDP payload: a QUIC DATAGRAM frame pays for the packet
    /// header, the packet number and the AEAD tag before the application gets
    /// any of it. Measured at 1162 on loopback against an assumed 1200 UDP
    /// payload, which is why `pravera_proto::SAFE_DATAGRAM` reserves for it.
    pub fn max_datagram_size(&self) -> Option<usize> {
        self.connection.max_datagram_size()
    }

    /// Chunk payload this path can actually carry, header included.
    ///
    /// `pravera_proto::MAX_CHUNK_PAYLOAD` is the conservative constant the
    /// splitter uses and the reassembler enforces. This is what the path in
    /// front of us reports right now, which may be larger on a direct link and
    /// smaller behind a tunnel. `None` means datagrams will not flow at all.
    pub fn max_chunk_payload(&self) -> Option<usize> {
        let limit = self.max_datagram_size()?;
        limit.checked_sub(pravera_proto::ChunkHeader::SIZE)
    }

    /// Send one media chunk.
    ///
    /// Unreliable and unordered by design. A dropped datagram is a dropped
    /// chunk, and the reassembler on the far end handles it; retransmitting
    /// would deliver a frame that is already too old to show while blocking
    /// every frame behind it.
    ///
    /// Never blocks. If the send buffer is full this returns an error rather
    /// than waiting, because a video pipeline that stalls on backpressure
    /// converts a bandwidth problem into a latency problem.
    pub fn send_media(&self, datagram: Bytes) -> Result<()> {
        let Some(limit) = self.max_datagram_size() else {
            return Err(TransportError::DatagramsUnsupported);
        };
        if datagram.len() > limit {
            return Err(TransportError::DatagramTooLarge {
                size: datagram.len(),
                limit,
            });
        }
        self.connection
            .send_datagram(datagram)
            .map_err(TransportError::datagram)
    }

    /// Receive one media chunk. Feed it to a `pravera_proto::Reassembler`.
    pub async fn recv_media(&self) -> Result<Bytes> {
        self.connection
            .read_datagram()
            .await
            .map_err(TransportError::lost)
    }

    // --------------------------------------------------------------- close

    /// Close this session, telling the peer why.
    ///
    /// The reason is read by a human at the other end, so it must not carry
    /// anything an unauthenticated peer should not see. It travels in a QUIC
    /// CONNECTION_CLOSE frame, which is encrypted but arrives before any
    /// application-level authorisation has happened.
    pub fn close(&self, reason: &str) {
        self.connection
            .close(VarInt::from_u32(0), reason.as_bytes());
    }

    /// Resolves when the session ends, however it ends.
    pub async fn closed(&self) {
        self.connection.closed().await;
    }

    /// The underlying iroh connection, for anything this wrapper does not cover.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }
}

/// A session's role in the protocol conversation.
///
/// Which side dialled decides who speaks first and which message enum each end
/// sends. It is not a trust distinction: both ends authenticated the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Dialled out. Sends [`pravera_proto::ClientMessage`], receives
    /// [`pravera_proto::HostMessage`].
    Client,
    /// Accepted in. Sends [`pravera_proto::HostMessage`], receives
    /// [`pravera_proto::ClientMessage`].
    Host,
}

impl Role {
    pub const fn name(self) -> &'static str {
        match self {
            Role::Client => "client",
            Role::Host => "host",
        }
    }
}
