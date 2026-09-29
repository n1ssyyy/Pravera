use std::net::IpAddr;

use pravera_core::DeviceId;
use serde::{Deserialize, Serialize};

/// The physical class of a point-to-point cable link.
///
/// All three arrive through the same observable event, a new interface holding
/// a link-local address, so they share one code path and differ only in how we
/// describe them to the user and what throughput we expect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LinkKind {
    /// A plain Ethernet cable between two machines.
    Ethernet,
    /// USB4 or Thunderbolt 3/4 host-to-host. Windows enumerates a USB4 P2P
    /// adapter (compatible ID `USB4\USB4NET`); Linux uses `thunderbolt-net`.
    Usb4Net,
    /// A USB bridge cable whose bridge IC speaks CDC-NCM, bound to the in-box
    /// NCM host driver.
    Ncm,
}

impl LinkKind {
    pub const fn label(self) -> &'static str {
        match self {
            LinkKind::Ethernet => "Ethernet",
            LinkKind::Usb4Net => "Thunderbolt / USB4",
            LinkKind::Ncm => "USB bridge",
        }
    }

    /// Rough expected throughput, used only to set expectations in the UI.
    pub const fn expected_gbps(self) -> u32 {
        match self {
            LinkKind::Ethernet => 1,
            LinkKind::Usb4Net => 20,
            LinkKind::Ncm => 1,
        }
    }
}

/// How a Tailscale peer is currently reachable.
///
/// Taken from the `CurAddr` and `Relay` fields of the daemon status: a peer
/// with a current address has been hole-punched and is talking directly, while
/// one with only a relay is riding a DERP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TsRoute {
    /// Hole-punched WireGuard, peer to peer.
    Direct,
    /// Relayed through a DERP server in the named region.
    Derp { region: String },
    /// Tailscale knows the peer but has not established a path yet.
    Unknown,
}

impl TsRoute {
    pub fn label(&self) -> String {
        match self {
            TsRoute::Direct => "direct".to_string(),
            TsRoute::Derp { region } => format!("relay ({region})"),
            TsRoute::Unknown => "connecting".to_string(),
        }
    }

    pub const fn is_direct(&self) -> bool {
        matches!(self, TsRoute::Direct)
    }
}

/// Where a peer was found. Everything downstream consumes a single unified
/// peer stream and never branches on transport, so adding a source later does
/// not ripple through the app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerSource {
    /// A cable. Sub-millisecond and uncontended, so always ranked first.
    DirectLink { kind: LinkKind },
    /// Found by mDNS on the local subnet.
    Lan {
        /// UDP port from the advertisement's SRV record.
        port: u16,
        /// The connect code the advertisement carried: the peer's full public
        /// key, base32.
        ///
        /// Named `claims` rather than `key` on purpose. Anybody on the subnet
        /// can advertise anything, so this is a string to dial *at*, not a
        /// statement of who will answer. Who answers is settled by the TLS
        /// handshake, and a substituted key is caught by the pinned entry in
        /// the known-machines file exactly as it would be on any other route.
        claims: String,
        /// The wire-protocol version the peer said it speaks, when it said.
        /// `None` means the advertisement omitted it, which an older build
        /// does.
        version: Option<u16>,
    },
    /// A node on the same tailnet.
    Tailscale { magic_dns: String, route: TsRoute },
    /// Dialed by device ID through iroh.
    Iroh,
}

impl PeerSource {
    /// Lower ranks are tried first. Background probing may later migrate a live
    /// session to a better-ranked route.
    pub fn rank(&self) -> u8 {
        match self {
            PeerSource::DirectLink { .. } => 1,
            PeerSource::Lan { .. } => 2,
            PeerSource::Tailscale { route, .. } if route.is_direct() => 3,
            PeerSource::Iroh => 4,
            PeerSource::Tailscale { .. } => 5,
        }
    }

    pub fn label(&self) -> String {
        match self {
            PeerSource::DirectLink { kind } => kind.label().to_string(),
            PeerSource::Lan { .. } => "LAN".to_string(),
            PeerSource::Tailscale { route, .. } => format!("Tailscale, {}", route.label()),
            PeerSource::Iroh => "Internet".to_string(),
        }
    }
}

/// A peer Pravera has found but not necessarily connected to or authenticated.
///
/// The name and OS are display hints taken from whichever discovery source
/// produced them. They are never an identity claim: identity is the ed25519
/// public key, proven during the TLS handshake, and `device_id` stays `None`
/// until that has happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredPeer {
    /// Display name, e.g. the tailnet or mDNS hostname.
    pub name: String,
    /// Addresses to try, best first.
    pub addresses: Vec<IpAddr>,
    pub source: PeerSource,
    /// Reported operating system, when the source knows it.
    pub os: Option<String>,
    /// Whether the source believes the peer is reachable right now.
    pub online: bool,
    /// Set only after a handshake has proven the peer's key.
    pub device_id: Option<DeviceId>,
}

impl DiscoveredPeer {
    /// True once this peer's identity has been cryptographically established.
    pub const fn is_verified(&self) -> bool {
        self.device_id.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(route: TsRoute) -> PeerSource {
        PeerSource::Tailscale {
            magic_dns: "box.example.ts.net".into(),
            route,
        }
    }

    fn lan() -> PeerSource {
        PeerSource::Lan {
            port: 41_337,
            claims: "not-a-real-key".into(),
            version: Some(2),
        }
    }

    #[test]
    fn a_cable_always_outranks_every_network_route() {
        let cable = PeerSource::DirectLink {
            kind: LinkKind::Usb4Net,
        };
        assert!(cable.rank() < lan().rank());
        assert!(cable.rank() < ts(TsRoute::Direct).rank());
        assert!(cable.rank() < PeerSource::Iroh.rank());
    }

    #[test]
    fn direct_tailscale_outranks_iroh_but_relayed_does_not() {
        assert!(ts(TsRoute::Direct).rank() < PeerSource::Iroh.rank());
        assert!(
            ts(TsRoute::Derp {
                region: "lhr".into()
            })
            .rank()
                > PeerSource::Iroh.rank()
        );
    }

    #[test]
    fn relay_label_names_the_region_so_the_ui_can_explain_itself() {
        let route = TsRoute::Derp {
            region: "fra".into(),
        };
        assert_eq!(route.label(), "relay (fra)");
        assert!(!route.is_direct());
    }

    #[test]
    fn an_undiscovered_peer_is_not_verified() {
        let peer = DiscoveredPeer {
            name: "workstation".into(),
            addresses: vec![],
            source: lan(),
            os: None,
            online: true,
            device_id: None,
        };
        assert!(
            !peer.is_verified(),
            "identity must come from the handshake, not from discovery"
        );
    }
}
