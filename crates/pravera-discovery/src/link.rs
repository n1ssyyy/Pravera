//! Classification of network interfaces into Pravera link types.
//!
//! Ethernet crossover, USB4/Thunderbolt host-to-host, and CDC-NCM bridge cables
//! all announce themselves the same way: a new interface appears carrying a
//! link-local `169.254.0.0/16` address. One watcher therefore covers all three,
//! and classification only changes how the link is described and what
//! throughput the UI should promise.

use std::net::{IpAddr, Ipv4Addr};

use crate::peer::LinkKind;

/// IPv4 link-local, assigned automatically when no DHCP server answers. This is
/// what both ends of a direct cable settle on.
const LINK_LOCAL_NET: Ipv4Addr = Ipv4Addr::new(169, 254, 0, 0);
const LINK_LOCAL_BITS: u32 = 16;

/// True for an address in `169.254.0.0/16`.
pub fn is_link_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let mask = u32::MAX << (32 - LINK_LOCAL_BITS);
            (u32::from(v4) & mask) == (u32::from(LINK_LOCAL_NET) & mask)
        }
        // IPv6 link-local (fe80::/10) is present on essentially every
        // interface, so it carries no signal about a direct cable.
        IpAddr::V6(_) => false,
    }
}

/// Guesses the link type from an interface name.
///
/// Names are the only cross-platform signal available without pulling in
/// per-OS device enumeration. Windows names the USB4 peer-to-peer adapter after
/// its function, and Linux uses `thunderbolt-net`; both contain a recognisable
/// substring. Anything unrecognised is assumed to be Ethernet, which is the
/// safe default: it only understates the expected throughput.
pub fn classify(interface_name: &str) -> LinkKind {
    let n = interface_name.to_ascii_lowercase();

    const USB4_HINTS: [&str; 5] = ["usb4", "thunderbolt", "tbt", "thnd", "usb4net"];
    const NCM_HINTS: [&str; 4] = ["ncm", "usb bridge", "linkusb", "cdc"];

    if USB4_HINTS.iter().any(|h| n.contains(h)) {
        LinkKind::Usb4Net
    } else if NCM_HINTS.iter().any(|h| n.contains(h)) {
        LinkKind::Ncm
    } else {
        LinkKind::Ethernet
    }
}

/// A candidate direct-cable link found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectLink {
    pub interface: String,
    pub kind: LinkKind,
    /// Our own address on this link. The peer is somewhere else in the same
    /// `/16`, found by probing or mDNS.
    pub local_addr: IpAddr,
}

/// Scans the interface list for links that look like a direct cable.
///
/// This reports *candidates*, not confirmed peers: a machine plugged into a
/// switch with no DHCP server also lands on a link-local address. Confirming a
/// peer means actually reaching one, which the discovery loop does next.
pub fn scan() -> Vec<DirectLink> {
    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };

    interfaces
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| {
            let addr = i.ip();
            is_link_local(addr).then(|| DirectLink {
                kind: classify(&i.name),
                interface: i.name,
                local_addr: addr,
            })
        })
        .collect()
}

/// Explains why a cable the user just plugged in cannot carry a session.
///
/// Two USB hosts joined by a plain USB-C cable never negotiate a host/device
/// relationship, so no interface ever appears. Without this message the UI
/// would simply spin forever, and the user would reasonably conclude Pravera
/// is broken rather than that the cable cannot work.
pub const PLAIN_USB_C_EXPLANATION: &str = "Both ends of a plain USB-C cable are USB hosts, so no \
link can form. Use a Thunderbolt or USB4 port on both machines, a USB bridge cable with a built-in \
bridge chip, or an Ethernet cable.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_link_local_block() {
        assert!(is_link_local("169.254.76.91".parse().unwrap()));
        assert!(is_link_local("169.254.0.0".parse().unwrap()));
        assert!(is_link_local("169.254.255.255".parse().unwrap()));
    }

    #[test]
    fn rejects_routable_and_tailscale_addresses() {
        assert!(!is_link_local("192.168.0.178".parse().unwrap()));
        assert!(!is_link_local("100.101.102.103".parse().unwrap()));
        assert!(!is_link_local("169.253.255.255".parse().unwrap()));
        assert!(!is_link_local("169.255.0.0".parse().unwrap()));
    }

    #[test]
    fn ipv6_link_local_is_not_a_direct_cable_signal() {
        // fe80::/10 exists on nearly every interface and means nothing here.
        assert!(!is_link_local("fe80::1".parse().unwrap()));
    }

    #[test]
    fn identifies_usb4_and_thunderbolt_adapters_by_name() {
        for name in [
            "USB4 P2P Network Adapter",
            "thunderbolt0",
            "TBT Networking",
            "usb4net0",
        ] {
            assert_eq!(classify(name), LinkKind::Usb4Net, "name: {name}");
        }
    }

    #[test]
    fn identifies_ncm_bridge_cables_by_name() {
        for name in ["USB NCM Host Device", "cdc-ncm0"] {
            assert_eq!(classify(name), LinkKind::Ncm, "name: {name}");
        }
    }

    #[test]
    fn unknown_names_default_to_ethernet() {
        // Understating throughput is the safe failure: the link still works.
        assert_eq!(classify("Ethernet 2"), LinkKind::Ethernet);
        assert_eq!(classify("enp3s0"), LinkKind::Ethernet);
    }

    #[test]
    fn usb4_promises_far_more_throughput_than_ethernet() {
        assert!(LinkKind::Usb4Net.expected_gbps() > LinkKind::Ethernet.expected_gbps());
    }

    #[test]
    fn scanning_the_real_machine_does_not_panic() {
        // Whatever this box has plugged in, enumeration must be infallible.
        let links = scan();
        for link in &links {
            assert!(is_link_local(link.local_addr));
        }
    }
}
