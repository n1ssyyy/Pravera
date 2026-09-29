//! How a peer is named and located.
//!
//! Two different things, and conflating them is the mistake this module exists
//! to prevent:
//!
//! - A [`PeerKey`] is *who*. It is the device's ed25519 public key, it is
//!   proven by the TLS handshake, and nothing else in Pravera counts as
//!   identity.
//! - A [`PeerAddress`] adds *where*: socket addresses learned from discovery.
//!   Addresses are hints. They can be stale, spoofed, or supplied by a hostile
//!   party, and none of that matters, because a connection to the wrong machine
//!   simply fails the handshake.

use std::fmt;
use std::net::SocketAddr;

use iroh::{EndpointAddr, EndpointId};
use pravera_core::DeviceId;

use crate::error::{Result, TransportError};

/// A device's ed25519 public key: the only thing Pravera treats as identity.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerKey([u8; 32]);

impl PeerKey {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        PeerKey(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The short human-readable name for this device.
    ///
    /// One-way: [`DeviceId`] is a 40-bit BLAKE3 truncation, so a key yields an
    /// ID but an ID never yields a key. Dialling therefore needs the full key,
    /// which discovery supplies. That asymmetry is deliberate, and it is why a
    /// device ID is safe to read aloud.
    pub fn device_id(&self) -> DeviceId {
        DeviceId::from_public_key(&self.0)
    }

    /// The key as a connect code: the string a person pastes in to reach this
    /// machine.
    ///
    /// Grouped for reading. See [`pravera_core::connect_code`] for why this
    /// exists alongside the much shorter device ID, and why one cannot replace
    /// the other.
    pub fn to_code(&self) -> String {
        pravera_core::connect_code::grouped(&self.0)
    }

    /// Read a key back out of a connect code.
    ///
    /// The error is a `pravera_core::Error` because it is shown to whoever
    /// typed the code, and it says which character or which length was wrong.
    pub fn from_code(text: &str) -> pravera_core::Result<PeerKey> {
        pravera_core::connect_code::from_code(text).map(PeerKey)
    }

    pub(crate) fn endpoint_id(&self) -> Result<EndpointId> {
        EndpointId::from_bytes(&self.0).map_err(|_| TransportError::InvalidPeerKey)
    }
}

impl From<EndpointId> for PeerKey {
    fn from(id: EndpointId) -> Self {
        PeerKey(*id.as_bytes())
    }
}

impl fmt::Display for PeerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.device_id())
    }
}

impl fmt::Debug for PeerKey {
    /// Prints the device ID rather than 64 hex characters.
    ///
    /// A public key is not secret, but it is unreadable, and a log full of raw
    /// keys is a log nobody checks. The device ID is what the interface shows,
    /// so it is what the log should show too.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PeerKey({})", self.device_id())
    }
}

/// Where to try reaching a peer, and who it must turn out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAddress {
    pub key: PeerKey,
    /// Socket addresses to try. Empty means "ask the address lookup service",
    /// which only works on a [`Reachability::Global`] endpoint.
    ///
    /// [`Reachability::Global`]: crate::Reachability::Global
    pub addrs: Vec<SocketAddr>,
}

impl PeerAddress {
    /// A peer to be found by key alone, through discovery.
    pub fn new(key: PeerKey) -> Self {
        PeerAddress {
            key,
            addrs: Vec::new(),
        }
    }

    /// A peer at known addresses: a direct link, a LAN neighbour, or a tailnet
    /// address that discovery already resolved.
    pub fn at(key: PeerKey, addrs: impl IntoIterator<Item = SocketAddr>) -> Self {
        PeerAddress {
            key,
            addrs: addrs.into_iter().collect(),
        }
    }

    pub fn device_id(&self) -> DeviceId {
        self.key.device_id()
    }

    pub(crate) fn to_endpoint_addr(&self) -> Result<EndpointAddr> {
        let mut addr = EndpointAddr::new(self.key.endpoint_id()?);
        for socket in &self.addrs {
            addr = addr.with_ip_addr(*socket);
        }
        Ok(addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_real_key() -> [u8; 32] {
        // Not random: an arbitrary 32 bytes is usually not a valid curve point,
        // and half these tests would then be testing the rejection path.
        *iroh::SecretKey::from_bytes(&[7u8; 32]).public().as_bytes()
    }

    #[test]
    fn a_key_yields_the_device_id_the_interface_shows() {
        let key = PeerKey::from_bytes(a_real_key());
        assert_eq!(key.device_id(), DeviceId::from_public_key(&a_real_key()));
        assert_eq!(key.to_string(), key.device_id().to_string());
    }

    #[test]
    fn debug_prints_the_device_id_not_sixty_four_hex_characters() {
        let key = PeerKey::from_bytes(a_real_key());
        let shown = format!("{key:?}");
        assert!(shown.contains(&key.device_id().to_string()), "{shown}");
        assert!(!shown.contains("[7, 7, 7"), "{shown}");
    }

    #[test]
    fn a_key_survives_being_pasted_between_two_machines() {
        // The connect code is the only way to reach a machine before discovery
        // lands, so a key that does not survive the round trip means no
        // session at all.
        let key = PeerKey::from_bytes(a_real_key());
        let code = key.to_code();

        assert_eq!(PeerKey::from_code(&code).unwrap(), key);
        assert_eq!(PeerKey::from_code(&code.to_lowercase()).unwrap(), key);
        assert_eq!(PeerKey::from_code(&code.replace('-', " ")).unwrap(), key);
    }

    #[test]
    fn a_device_id_is_not_accepted_where_a_connect_code_belongs() {
        // They look similar enough to confuse, and the failure mode of
        // accepting one for the other would be dialling a machine nobody
        // named. A device ID is five bytes; a key is thirty-two.
        let key = PeerKey::from_bytes(a_real_key());
        assert!(PeerKey::from_code(&key.device_id().to_string()).is_err());
    }

    #[test]
    fn a_connect_code_and_the_device_id_beside_it_agree() {
        let key = PeerKey::from_bytes(a_real_key());
        let recovered = PeerKey::from_code(&key.to_code()).unwrap();
        assert_eq!(recovered.device_id(), key.device_id());
    }

    #[test]
    fn a_key_round_trips_through_iroh() {
        let key = PeerKey::from_bytes(a_real_key());
        let id = key.endpoint_id().unwrap();
        assert_eq!(PeerKey::from(id), key);
    }

    #[test]
    fn thirty_two_bytes_that_are_not_a_curve_point_are_refused() {
        // A mistyped or corrupted key must fail here, with a message about the
        // key, rather than surfacing later as an unexplained dial failure.
        //
        // The invalid pattern is searched for rather than hardcoded: roughly
        // half of all 32-byte strings do decompress to a valid Edwards point,
        // and picking one by eye gets it wrong. `[0xff; 32]` is valid.
        let bad = (0u8..=255)
            .map(|n| [n; 32])
            .find(|bytes| EndpointId::from_bytes(bytes).is_err())
            .expect("some uniform byte pattern must fail to decompress");

        let key = PeerKey::from_bytes(bad);
        assert!(matches!(
            key.endpoint_id(),
            Err(TransportError::InvalidPeerKey)
        ));
        assert!(matches!(
            PeerAddress::new(key).to_endpoint_addr(),
            Err(TransportError::InvalidPeerKey)
        ));
    }

    #[test]
    fn address_hints_are_carried_through_but_identity_is_not_taken_from_them() {
        let key = PeerKey::from_bytes(a_real_key());
        let hint: SocketAddr = "192.0.2.10:41641".parse().unwrap();
        let address = PeerAddress::at(key, [hint]);

        let endpoint_addr = address.to_endpoint_addr().unwrap();
        assert_eq!(endpoint_addr.id, key.endpoint_id().unwrap());
        assert_eq!(
            endpoint_addr.ip_addrs().copied().collect::<Vec<_>>(),
            vec![hint]
        );
    }

    #[test]
    fn a_peer_with_no_hints_is_left_for_discovery_to_locate() {
        let address = PeerAddress::new(PeerKey::from_bytes(a_real_key()));
        assert!(address.addrs.is_empty());
        assert!(address
            .to_endpoint_addr()
            .unwrap()
            .ip_addrs()
            .next()
            .is_none());
    }
}
