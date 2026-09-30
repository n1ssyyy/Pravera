//! The local QUIC endpoint.
//!
//! One [`Transport`] per process. It owns the device key, listens for incoming
//! sessions, and dials outgoing ones.

use std::net::SocketAddr;

use iroh::endpoint::{presets, ConnectOptions};
use iroh::{Endpoint, SecretKey};
use pravera_core::DeviceId;
use pravera_crypto::Identity;
use tracing::debug;

use crate::error::{Result, TransportError};
use crate::peer::{PeerAddress, PeerKey};
use crate::session::Session;

/// The newest application protocol name, negotiated during the TLS handshake.
///
/// Carries the protocol version, so two incompatible builds are separated by
/// QUIC before either sends an application byte. An endpoint accepts every
/// version in [`pravera_proto::ALPNS`], and a dialler offers all of them in the
/// one handshake: the host picks the newest both ends know, so no retry is ever
/// needed and an old host is not dialled twice.
pub const ALPN: &[u8] = pravera_proto::ALPN;

/// How far this endpoint can be reached from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reachability {
    /// Reachable from anywhere. Uses n0's relay servers and publishes to their
    /// address-lookup service, so a peer holding only a device key can find
    /// this machine across the internet.
    ///
    /// Publishing is the cost: the endpoint announces its key and addresses to
    /// a public DNS/pkarr service. Everything Pravera then carries is still end
    /// to end encrypted and a relay only ever sees ciphertext, but the fact
    /// that this device exists and roughly where it is becomes public.
    Global,

    /// No relays and no address publication. Only peers whose address is
    /// already known are reachable: a direct cable, a LAN neighbour found over
    /// mDNS, or a tailnet address.
    ///
    /// The private choice, and the one a direct-link session wants: nothing
    /// leaves the local network and nothing is announced anywhere.
    LocalOnly,
}

/// The local endpoint: one device's presence on the network.
#[derive(Debug, Clone)]
pub struct Transport {
    endpoint: Endpoint,
}

impl Transport {
    /// Bind an endpoint using this device's identity.
    ///
    /// The device key is the TLS identity, so the machine that answers a dial
    /// is provably the machine that key names. There is no separate
    /// certificate, no CA, and nothing to configure.
    pub async fn bind(identity: &Identity, reachability: Reachability) -> Result<Self> {
        Self::bind_up_to(identity, reachability, pravera_proto::VERSION).await
    }

    /// Like [`Transport::bind`], but speaking no protocol newer than `newest`.
    ///
    /// Exists so a test can stand up a host that behaves like an older build and
    /// prove a newer viewer still connects to it. Nothing in the product calls
    /// it with anything but the current version.
    pub async fn bind_up_to(
        identity: &Identity,
        reachability: Reachability,
        newest: u16,
    ) -> Result<Self> {
        let alpns: Vec<Vec<u8>> = pravera_proto::ALPNS
            .iter()
            .filter(|alpn| pravera_proto::version_of_alpn(alpn).is_some_and(|v| v <= newest))
            .map(|alpn| alpn.to_vec())
            .collect();
        let secret = SecretKey::from_bytes(&identity.secret_bytes());

        let builder = match reachability {
            Reachability::Global => Endpoint::builder(presets::N0),
            Reachability::LocalOnly => Endpoint::builder(presets::Minimal),
        };

        let endpoint = builder
            .secret_key(secret)
            .alpns(alpns)
            .bind()
            .await
            .map_err(TransportError::bind)?;

        debug!(
            device = %identity.device_id(),
            ?reachability,
            sockets = ?endpoint.bound_sockets(),
            "endpoint bound"
        );
        Ok(Transport { endpoint })
    }

    /// This device's public key.
    pub fn peer_key(&self) -> PeerKey {
        PeerKey::from(self.endpoint.id())
    }

    pub fn device_id(&self) -> DeviceId {
        self.peer_key().device_id()
    }

    /// What to hand a peer so it can dial back.
    ///
    /// On a [`Reachability::Global`] endpoint the addresses are a shortcut: a
    /// peer with only the key can still find this device. On a
    /// [`Reachability::LocalOnly`] endpoint they are the only way in.
    pub fn local_address(&self) -> PeerAddress {
        let addr = self.endpoint.addr();
        PeerAddress::at(self.peer_key(), addr.ip_addrs().copied())
    }

    /// The sockets actually bound, which may be unspecified addresses such as
    /// `0.0.0.0:41641`. Use [`Transport::local_address`] for something dialable.
    pub fn bound_sockets(&self) -> Vec<SocketAddr> {
        self.endpoint.bound_sockets()
    }

    /// Dial a peer and complete the QUIC handshake.
    ///
    /// Returning `Ok` means the far end proved possession of the private key
    /// for [`PeerAddress::key`]. It does **not** mean anyone has logged in:
    /// that is the next conversation, over the control stream.
    pub async fn connect(&self, peer: &PeerAddress) -> Result<Session> {
        let addr = peer.to_endpoint_addr()?;
        let older = pravera_proto::ALPNS[1..]
            .iter()
            .map(|alpn| alpn.to_vec())
            .collect();
        let connection = self
            .endpoint
            .connect_with_opts(
                addr,
                ALPN,
                ConnectOptions::new().with_additional_alpns(older),
            )
            .await
            .map_err(TransportError::unreachable)?
            .await
            .map_err(TransportError::unreachable)?;
        Session::from_connection(connection)
    }

    /// Wait for an incoming session.
    ///
    /// Yields `None` once the endpoint is closed, which is the signal to stop
    /// the accept loop. A single failed handshake is `Some(Err(..))` and must
    /// not end the loop: one peer with a bad clock or a dropped packet cannot
    /// be allowed to take the host offline.
    pub async fn accept(&self) -> Option<Result<Session>> {
        let incoming = self.endpoint.accept().await?;
        Some(match incoming.await {
            Ok(connection) => Session::from_connection(connection),
            Err(e) => Err(TransportError::lost(e)),
        })
    }

    /// Close the endpoint and every session on it.
    pub async fn close(&self) {
        self.endpoint.close().await;
    }

    pub fn is_closed(&self) -> bool {
        self.endpoint.is_closed()
    }
}
