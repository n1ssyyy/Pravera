//! Tailscale as a discovery source.
//!
//! Detection is two-stage. Presence is confirmed cheaply from the interface
//! list: any address inside the CGNAT range `100.64.0.0/10`, or the Tailscale
//! ULA prefix `fd7a:115c:a1e0::/48`, means the tunnel is up. Peer enumeration
//! then comes from the daemon.
//!
//! Peers are reported as display hints only. A machine being on the tailnet is
//! not authentication: Pravera still verifies the ed25519 key and still demands
//! a username and password, because Tailscale ACLs gate *reachability* while
//! Pravera roles gate *capability*.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;

use serde::Deserialize;
use tracing::{debug, warn};

use crate::peer::{DiscoveredPeer, PeerSource, TsRoute};

/// Tailscale hands out IPv4 addresses from the shared-address (CGNAT) block.
const CGNAT_NET: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 0);
const CGNAT_BITS: u32 = 10;

/// The IPv6 ULA prefix Tailscale uses, `fd7a:115c:a1e0::/48`.
const TAILSCALE_ULA: [u16; 3] = [0xfd7a, 0x115c, 0xa1e0];

/// True if this address was handed out by Tailscale.
pub fn is_tailscale_addr(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let mask = u32::MAX << (32 - CGNAT_BITS);
            (u32::from(v4) & mask) == (u32::from(CGNAT_NET) & mask)
        }
        IpAddr::V6(v6) => in_tailscale_ula(v6),
    }
}

fn in_tailscale_ula(v6: Ipv6Addr) -> bool {
    let s = v6.segments();
    s[0] == TAILSCALE_ULA[0] && s[1] == TAILSCALE_ULA[1] && s[2] == TAILSCALE_ULA[2]
}

// ---------------------------------------------------------------------------
// Daemon status
// ---------------------------------------------------------------------------

/// Mirrors the parts of `tailscale status --json` we rely on.
///
/// Every field is optional with a default. The JSON shape the daemon emits is
/// not a stability promise, so an unknown or renamed field must degrade to
/// "peer present, details unknown" rather than failing discovery outright.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Status {
    #[serde(rename = "BackendState")]
    backend_state: String,
    #[serde(rename = "MagicDNSSuffix")]
    magic_dns_suffix: String,
    #[serde(rename = "Self")]
    self_node: Option<Node>,
    #[serde(rename = "Peer")]
    peer: HashMap<String, Node>,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
struct Node {
    #[serde(rename = "HostName")]
    host_name: String,
    #[serde(rename = "DNSName")]
    dns_name: String,
    #[serde(rename = "OS")]
    os: String,
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Vec<String>,
    #[serde(rename = "Online")]
    online: bool,
    /// Non-empty once a direct path has been hole-punched.
    #[serde(rename = "CurAddr")]
    cur_addr: String,
    /// DERP region code while relayed.
    #[serde(rename = "Relay")]
    relay: String,
}

impl Node {
    fn route(&self) -> TsRoute {
        if !self.cur_addr.is_empty() {
            TsRoute::Direct
        } else if !self.relay.is_empty() {
            TsRoute::Derp {
                region: self.relay.clone(),
            }
        } else {
            TsRoute::Unknown
        }
    }

    /// MagicDNS name without its trailing dot, falling back to the hostname
    /// when MagicDNS is disabled on the tailnet.
    fn magic_dns(&self) -> String {
        let trimmed = self.dns_name.trim_end_matches(DOT);
        if trimmed.is_empty() {
            self.host_name.clone()
        } else {
            trimmed.to_string()
        }
    }

    fn addresses(&self) -> Vec<IpAddr> {
        self.tailscale_ips
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect()
    }

    fn into_peer(self) -> Option<DiscoveredPeer> {
        let addresses = self.addresses();
        if addresses.is_empty() {
            // Nothing to dial; skip rather than showing an entry that cannot
            // be connected to.
            return None;
        }
        let route = self.route();
        let magic_dns = self.magic_dns();
        let name = if self.host_name.is_empty() {
            magic_dns
                .split(DOT)
                .next()
                .unwrap_or("tailnet peer")
                .to_string()
        } else {
            self.host_name.clone()
        };
        let os = (!self.os.is_empty()).then(|| self.os.clone());

        Some(DiscoveredPeer {
            name,
            addresses,
            source: PeerSource::Tailscale { magic_dns, route },
            os,
            online: self.online,
            device_id: None,
        })
    }
}

const DOT: char = '.';

// ---------------------------------------------------------------------------
// Locating the CLI
// ---------------------------------------------------------------------------

/// Well-known install locations, tried after bare `PATH` lookup.
fn candidate_binaries() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let mut v = vec![PathBuf::from("tailscale.exe")];
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(root) = std::env::var_os(var) {
                v.push(PathBuf::from(root).join("Tailscale").join("tailscale.exe"));
            }
        }
        v
    }
    #[cfg(not(windows))]
    {
        vec![
            PathBuf::from("tailscale"),
            PathBuf::from("/usr/bin/tailscale"),
            PathBuf::from("/usr/local/bin/tailscale"),
            PathBuf::from("/Applications/Tailscale.app/Contents/MacOS/Tailscale"),
        ]
    }
}

/// Start the CLI without giving it a window.
///
/// A console application launched from a windowed process gets a console of its
/// own, and Windows shows it. Discovery runs on a timer, so without this a
/// black box flashes on screen every few seconds — on the machine of somebody
/// who never asked to run anything. `CREATE_NO_WINDOW` is the flag that says
/// the process is ours and not the user's to look at.
fn command(bin: &std::path::Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(bin);
    command.args(["status", "--json"]);

    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    command
}

/// Asks the daemon for its status.
///
/// Shells out to the CLI rather than opening the LocalAPI socket directly: on
/// Windows that named pipe is restricted to Administrators and the user-level
/// UI process cannot open it. The CLI works from both the elevated service and
/// the unprivileged UI, which is worth more than saving a process spawn on a
/// path that runs a few times a minute.
async fn fetch_status() -> Option<Status> {
    for bin in candidate_binaries() {
        let output = match command(&bin).output().await {
            Ok(o) => o,
            Err(_) => continue, // not at this path; try the next
        };

        if !output.status.success() {
            debug!(binary = %bin.display(), "tailscale CLI returned a failure status");
            continue;
        }

        return match serde_json::from_slice::<Status>(&output.stdout) {
            Ok(status) => Some(status),
            Err(e) => {
                warn!(error = %e, "could not parse tailscale status; treating Tailscale as unavailable");
                None
            }
        };
    }
    None
}

/// The tailnet as Pravera sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tailnet {
    /// For example `example-tailnet.ts.net`.
    pub magic_dns_suffix: String,
    /// This node's own MagicDNS name, when known.
    pub self_name: Option<String>,
    pub peers: Vec<DiscoveredPeer>,
}

/// Enumerates tailnet peers, or `None` when Tailscale is absent, stopped, or
/// logged out.
///
/// Offline peers are still returned, with `online: false`, so a known machine
/// shows greyed out in the grid rather than vanishing.
pub async fn discover() -> Option<Tailnet> {
    let status = fetch_status().await?;

    if status.backend_state != "Running" {
        debug!(state = %status.backend_state, "Tailscale is installed but not running");
        return None;
    }

    let self_name = status.self_node.as_ref().map(Node::magic_dns);
    let mut peers: Vec<DiscoveredPeer> = status
        .peer
        .into_values()
        .filter_map(Node::into_peer)
        .collect();

    // Online first, then by name: stable across polls, and the machines you can
    // actually reach sit at the top.
    peers.sort_by(|a, b| b.online.cmp(&a.online).then_with(|| a.name.cmp(&b.name)));

    Some(Tailnet {
        magic_dns_suffix: status.magic_dns_suffix,
        self_name,
        peers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_addresses_from_the_cgnat_block() {
        assert!(is_tailscale_addr("100.101.102.103".parse().unwrap()));
        assert!(is_tailscale_addr("100.64.0.0".parse().unwrap()));
        assert!(is_tailscale_addr("100.127.255.255".parse().unwrap()));
    }

    #[test]
    fn rejects_addresses_just_outside_the_cgnat_block() {
        assert!(!is_tailscale_addr("100.63.255.255".parse().unwrap()));
        assert!(!is_tailscale_addr("100.128.0.0".parse().unwrap()));
        assert!(!is_tailscale_addr("192.168.0.178".parse().unwrap()));
        assert!(!is_tailscale_addr("169.254.1.1".parse().unwrap()));
    }

    #[test]
    fn recognises_the_tailscale_ula() {
        assert!(is_tailscale_addr(
            "fd7a:115c:a1e0::5a01:4099".parse().unwrap()
        ));
        assert!(!is_tailscale_addr("fd00::1".parse().unwrap()));
    }

    #[test]
    fn a_current_address_means_the_peer_is_direct() {
        let node = Node {
            cur_addr: "192.0.2.7:41641".into(),
            relay: "lhr".into(),
            ..Default::default()
        };
        assert_eq!(node.route(), TsRoute::Direct, "CurAddr must win over Relay");
    }

    #[test]
    fn relay_only_means_derp() {
        let node = Node {
            relay: "fra".into(),
            ..Default::default()
        };
        assert_eq!(
            node.route(),
            TsRoute::Derp {
                region: "fra".into()
            }
        );
    }

    #[test]
    fn no_path_yet_is_unknown_rather_than_direct() {
        assert_eq!(Node::default().route(), TsRoute::Unknown);
    }

    #[test]
    fn magic_dns_name_loses_its_trailing_dot() {
        let node = Node {
            dns_name: "box.tail1234.ts.net.".into(),
            ..Default::default()
        };
        assert_eq!(node.magic_dns(), "box.tail1234.ts.net");
    }

    #[test]
    fn magic_dns_falls_back_to_hostname_when_disabled() {
        let node = Node {
            host_name: "workstation".into(),
            ..Default::default()
        };
        assert_eq!(node.magic_dns(), "workstation");
    }

    #[test]
    fn a_peer_with_no_address_is_skipped() {
        let node = Node {
            host_name: "ghost".into(),
            ..Default::default()
        };
        assert!(
            node.into_peer().is_none(),
            "an unreachable entry must not appear in the grid"
        );
    }

    #[test]
    fn unknown_fields_do_not_break_parsing() {
        let json = r#"{"BackendState":"Running","SomeFutureField":42,"Peer":{}}"#;
        let status: Status = serde_json::from_str(json).expect("must tolerate unknown fields");
        assert_eq!(status.backend_state, "Running");
    }

    #[test]
    fn a_relayed_peer_parses_into_a_display_hint_without_an_identity() {
        let node = Node {
            host_name: "Laptop".into(),
            dns_name: "laptop.example-tailnet.ts.net.".into(),
            os: "windows".into(),
            tailscale_ips: vec!["100.88.12.34".into()],
            online: true,
            cur_addr: String::new(),
            relay: "lhr".into(),
        };
        let peer = node.into_peer().unwrap();
        assert_eq!(peer.name, "Laptop");
        assert_eq!(peer.os.as_deref(), Some("windows"));
        assert!(peer.online);
        assert!(!peer.is_verified(), "discovery never establishes identity");
        assert_eq!(
            peer.source.rank(),
            5,
            "relayed tailscale ranks below iroh direct"
        );
    }
}
