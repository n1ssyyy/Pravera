//! Finding other Pravera machines.
//!
//! Every source, a direct cable, mDNS on the LAN, the tailnet, or a device ID
//! dialled over iroh, funnels into one [`DiscoveredPeer`] stream. Nothing
//! downstream branches on transport, so a new source can be added here without
//! rippling through the app.
//!
//! Discovery establishes *reachability*, never *identity*. A peer is only ever
//! a display hint until the TLS handshake proves possession of the matching
//! ed25519 private key.

pub mod link;
pub mod mdns;
pub mod peer;
pub mod tailscale;

pub use link::{DirectLink, PLAIN_USB_C_EXPLANATION};
pub use mdns::{Advertisement, Browser};
pub use peer::{DiscoveredPeer, LinkKind, PeerSource, TsRoute};

/// Everything Pravera can currently see, from every source.
///
/// Comparable on purpose. Scanning runs by itself now, so the common result is
/// a scan that found exactly what the last one did — and replacing the list
/// with an identical list would restart the home screen's entrance animation
/// every few seconds, which reads as a flicker with no cause.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovered {
    /// Cable links present on this machine. Candidates, not confirmed peers.
    pub direct_links: Vec<DirectLink>,
    /// Machines answering on the local subnet.
    pub lan: Vec<DiscoveredPeer>,
    /// Tailnet peers, when Tailscale is running and logged in.
    pub tailnet: Option<tailscale::Tailnet>,
}

impl Discovered {
    /// All peers, best route first.
    ///
    /// Ordering is by [`PeerSource::rank`], so a machine reachable both over a
    /// cable and over a DERP relay is offered on the cable.
    ///
    /// A machine reachable two ways appears twice, once per route, rather than
    /// being merged. Merging would mean deciding that an mDNS name and a
    /// tailnet name belong to the same machine, which nothing here can prove —
    /// the two sources share no identifier, and guessing from a matching name
    /// would fold two machines into one the first time somebody reused a name.
    /// Two rows, each labelled with how it is reachable, is also the more
    /// useful answer: at home the LAN row is the right one, away from home the
    /// tailnet row is, and the ranking already puts them in that order.
    pub fn peers(&self) -> Vec<DiscoveredPeer> {
        let mut peers = self.lan.clone();
        if let Some(tailnet) = &self.tailnet {
            peers.extend(tailnet.peers.iter().cloned());
        }

        peers.sort_by_key(|p| (p.source.rank(), !p.online));
        peers
    }

    pub fn lan_available(&self) -> bool {
        !self.lan.is_empty()
    }

    pub fn has_direct_link(&self) -> bool {
        !self.direct_links.is_empty()
    }

    pub fn tailscale_available(&self) -> bool {
        self.tailnet.is_some()
    }
}

/// Runs one full discovery pass across every source.
///
/// Cheap enough to poll on a timer: the interface scan is a syscall and the
/// tailnet query is one short-lived subprocess.
///
/// `lan` comes from a long-lived [`Browser`] rather than being gathered here,
/// because mDNS is not a poll. Announcements arrive when they arrive, and a
/// browser that was started and torn down once a second would re-ask the
/// subnet every time and still know less than one that had been listening.
pub async fn scan_all(lan: Vec<DiscoveredPeer>) -> Discovered {
    Discovered {
        direct_links: link::scan(),
        lan,
        tailnet: tailscale::discover().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(name: &str, source: PeerSource, online: bool) -> DiscoveredPeer {
        DiscoveredPeer {
            name: name.into(),
            addresses: vec!["100.64.0.1".parse().unwrap()],
            source,
            os: None,
            online,
            device_id: None,
        }
    }

    #[test]
    fn peers_are_ordered_by_route_quality() {
        let discovered = Discovered {
            direct_links: vec![],
            lan: vec![],
            tailnet: Some(tailscale::Tailnet {
                magic_dns_suffix: "example.ts.net".into(),
                self_name: None,
                peers: vec![
                    peer(
                        "relayed",
                        PeerSource::Tailscale {
                            magic_dns: "a.example.ts.net".into(),
                            route: TsRoute::Derp {
                                region: "lhr".into(),
                            },
                        },
                        true,
                    ),
                    peer(
                        "direct",
                        PeerSource::Tailscale {
                            magic_dns: "b.example.ts.net".into(),
                            route: TsRoute::Direct,
                        },
                        true,
                    ),
                ],
            }),
        };

        let ordered = discovered.peers();
        assert_eq!(
            ordered[0].name, "direct",
            "a hole-punched peer must be offered first"
        );
        assert_eq!(ordered[1].name, "relayed");
    }

    #[test]
    fn offline_peers_sink_below_online_ones_of_the_same_rank() {
        let route = || PeerSource::Tailscale {
            magic_dns: "x.example.ts.net".into(),
            route: TsRoute::Direct,
        };
        let discovered = Discovered {
            direct_links: vec![],
            lan: vec![],
            tailnet: Some(tailscale::Tailnet {
                magic_dns_suffix: "example.ts.net".into(),
                self_name: None,
                peers: vec![peer("asleep", route(), false), peer("awake", route(), true)],
            }),
        };

        assert_eq!(discovered.peers()[0].name, "awake");
    }

    #[test]
    fn an_empty_discovery_reports_nothing_available() {
        let empty = Discovered::default();
        assert!(empty.peers().is_empty());
        assert!(!empty.has_direct_link());
        assert!(!empty.tailscale_available());
    }
}
