//! Finding Pravera machines on the local subnet, and being found by them.
//!
//! Two halves that never talk to each other: [`Advertisement`] publishes this
//! machine while it is hosting, and [`Browser`] watches for everybody else's.
//! A machine doing both sees itself and filters itself out by key.
//!
//! # What goes on the wire, and why it is safe to put there
//!
//! The advertisement carries this machine's **full ed25519 public key**, not
//! its device ID. That is deliberate and it is the whole point: a device ID is
//! a 40-bit BLAKE3 truncation that cannot be dialled, so an advertisement
//! carrying only the ID would find a machine nobody could then connect to, and
//! the person would still have to carry a 52-character code between the two
//! machines by hand. Publishing the public key is what makes clicking a LAN
//! device work.
//!
//! A public key is not a secret. Possession of it grants nothing: the TLS
//! handshake proves the *private* key, which never leaves the machine that
//! generated it. This is the same trade `ssh-keyscan` makes, and the same one
//! every mDNS pairing scheme makes.
//!
//! # What it is not
//!
//! Every byte of an advertisement was written by whoever is on the subnet.
//! Anyone can advertise any name, and anyone can advertise their own key under
//! somebody else's name. So:
//!
//! - the name is a display hint and never an identity claim;
//! - [`DiscoveredPeer::device_id`] stays `None`, because nothing here has been
//!   proven — the announced key lives in [`PeerSource::Lan::claims`] under a
//!   name that says what it is;
//! - a substituted key is caught where every other substituted key is caught,
//!   by the pinned entry in the known-machines file, at handshake time.
//!
//! Discovery establishes reachability. It never establishes identity.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use parking_lot::Mutex;
use pravera_core::for_log;
use tracing::debug;

use crate::peer::{DiscoveredPeer, PeerSource};

/// The service type Pravera answers to.
///
/// `_udp` rather than `_tcp` because the transport underneath is QUIC, and the
/// port in the SRV record is a UDP port. Announcing `_tcp` would be a lie that
/// some other tool on the network might act on.
pub const SERVICE_TYPE: &str = "_pravera._udp.local.";

/// TXT key holding the connect code: the full public key, base32.
const KEY_PROPERTY: &str = "key";
/// TXT key holding the machine's display name.
const NAME_PROPERTY: &str = "name";
/// TXT key holding the wire-protocol version this machine speaks.
const VERSION_PROPERTY: &str = "v";

/// Longest name that will be read out of an advertisement.
///
/// The wire allows a 255-byte TXT string. This is a display name in a device
/// list, and something far shorter than that is either a mistake or somebody
/// trying to push the rest of the row off the screen.
const MAX_NAME: usize = 48;

/// This machine, published on the subnet for as long as the value is held.
///
/// Dropping it withdraws the advertisement. That is the whole lifecycle: there
/// is no pause, because a host that has stopped accepting sessions should stop
/// saying it accepts sessions.
pub struct Advertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advertisement {
    /// Publish this machine.
    ///
    /// `port` is the UDP port the transport is bound to; `key` is the connect
    /// code from [`PeerKey::to_code`]. Addresses are left to the daemon, which
    /// keeps them current as interfaces come and go — a laptop that moves from
    /// wired to wireless mid-session should not have to be told.
    ///
    /// [`PeerKey::to_code`]: https://docs.rs/pravera-transport
    pub fn publish(name: &str, key: &str, port: u16, version: u16) -> Result<Advertisement, Error> {
        let daemon = ServiceDaemon::new().map_err(|e| Error::Daemon(e.to_string()))?;

        // The instance name is what appears in other people's browsers before
        // anything is resolved, so it is the display name rather than the key.
        // Collisions are the daemon's problem: it renames, and the key in the
        // TXT record is what actually distinguishes two machines.
        let host = hostname_for(name);
        let properties: HashMap<String, String> = HashMap::from([
            (KEY_PROPERTY.to_string(), key.to_string()),
            (NAME_PROPERTY.to_string(), name.to_string()),
            (VERSION_PROPERTY.to_string(), version.to_string()),
        ]);

        let service = ServiceInfo::new(SERVICE_TYPE, name, &host, "", port, properties)
            .map_err(|e| Error::Daemon(e.to_string()))?
            .enable_addr_auto();

        let fullname = service.get_fullname().to_string();
        daemon
            .register(service)
            .map_err(|e| Error::Daemon(e.to_string()))?;

        debug!(name = %for_log(name), port, "advertising on the local subnet");
        Ok(Advertisement { daemon, fullname })
    }

    /// The instance name the daemon settled on, which may differ from the one
    /// asked for if another machine on the subnet had claimed it.
    pub fn fullname(&self) -> &str {
        &self.fullname
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        // Both calls hand back a receiver reporting when the work finished.
        // Neither is waited on: this runs on whatever thread dropped the value,
        // and blocking it to hear that a goodbye packet went out would be a
        // stall in exchange for nothing anybody acts on.
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// Everybody else, kept current in the background.
///
/// The daemon has its own thread and pushes events into a channel; [`peers`]
/// drains whatever has arrived and returns the current picture. Nothing here
/// blocks, so it is safe to call from a frame handler.
///
/// [`peers`]: Browser::peers
pub struct Browser {
    daemon: ServiceDaemon,
    events: mdns_sd::Receiver<ServiceEvent>,
    /// Keyed by mDNS fullname, which is the identifier the removal event
    /// carries. Shared so that draining can happen behind `&self`.
    found: Arc<Mutex<HashMap<String, DiscoveredPeer>>>,
    /// This machine's own connect code, filtered out of every result.
    ///
    /// Behind a lock because browsing starts when the window opens and the key
    /// is not known until the endpoint has bound, which is later. Until then
    /// this machine is simply not filtered — it is also not yet advertising,
    /// so there is nothing to filter.
    own_key: Mutex<Option<String>>,
}

impl Browser {
    /// Start watching.
    pub fn start() -> Result<Browser, Error> {
        let daemon = ServiceDaemon::new().map_err(|e| Error::Daemon(e.to_string()))?;
        let events = daemon
            .browse(SERVICE_TYPE)
            .map_err(|e| Error::Daemon(e.to_string()))?;

        Ok(Browser {
            daemon,
            events,
            found: Arc::new(Mutex::new(HashMap::new())),
            own_key: Mutex::new(None),
        })
    }

    /// Stop listing the machine holding this connect code: us.
    ///
    /// Also drops anything already found under that key, because the browser
    /// may well have seen this machine's own advertisement before the endpoint
    /// finished binding.
    pub fn ignore(&self, key: String) {
        self.found.lock().retain(|_, peer| match &peer.source {
            PeerSource::Lan { claims, .. } => claims != &key,
            _ => true,
        });
        *self.own_key.lock() = Some(key);
    }

    /// Everything currently visible, after taking in whatever arrived since
    /// the last call.
    pub fn peers(&self) -> Vec<DiscoveredPeer> {
        self.drain();
        self.found.lock().values().cloned().collect()
    }

    /// Take in every event waiting, without blocking on an empty channel.
    fn drain(&self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                ServiceEvent::ServiceResolved(service) => {
                    let fullname = service.get_fullname().to_string();
                    match self.read(&service) {
                        Some(peer) => {
                            self.found.lock().insert(fullname, peer);
                        }
                        // Something on the subnet is answering to Pravera's
                        // service type without speaking Pravera. Not an error
                        // and not worth a warning every announcement.
                        None => debug!(name = %for_log(&fullname), "ignoring an advertisement"),
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    self.found.lock().remove(&fullname);
                }
                _ => {}
            }
        }
    }

    /// Turn a resolved advertisement into a peer, or refuse it.
    ///
    /// Refuses anything without a usable key, because a peer that cannot be
    /// dialled is a row in the list that does nothing when it is clicked.
    fn read(&self, service: &mdns_sd::ResolvedService) -> Option<DiscoveredPeer> {
        let key = service
            .get_property_val_str(KEY_PROPERTY)?
            .trim()
            .to_string();
        if key.is_empty() || self.own_key.lock().as_deref() == Some(key.as_str()) {
            return None;
        }

        let port = service.get_port();
        if port == 0 {
            return None;
        }

        let addresses: Vec<IpAddr> = service
            .get_addresses()
            .iter()
            .map(|scoped| scoped.to_ip_addr())
            .collect();
        if addresses.is_empty() {
            return None;
        }

        // Falls back to the instance name, which is where the display name came
        // from in the first place. Trimmed either way, because it is going into
        // a device list beside names this machine chose.
        let name = service
            .get_property_val_str(NAME_PROPERTY)
            .map(str::to_string)
            .unwrap_or_else(|| instance_name(service.get_fullname()));

        Some(DiscoveredPeer {
            name: display_name(&name),
            addresses,
            source: PeerSource::Lan {
                port,
                claims: key,
                version: service
                    .get_property_val_str(VERSION_PROPERTY)
                    .and_then(|v| v.parse().ok()),
            },
            os: None,
            // mDNS says a machine answered a moment ago. That is the only
            // liveness it has, and it is the same liveness a removal event
            // takes away.
            online: true,
            // Never set here. See the module docs: nothing on this path has
            // been proven, and the field that says "proven" must keep meaning
            // exactly that.
            device_id: None,
        })
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.daemon.stop_browse(SERVICE_TYPE);
        let _ = self.daemon.shutdown();
    }
}

/// Socket addresses to try for a LAN peer, best first.
///
/// IPv4 before IPv6: on a home subnet the v4 address is the one that is
/// actually routable between two machines, and a v6 link-local without its
/// scope is not dialable at all.
pub fn socket_addrs(peer: &DiscoveredPeer) -> Vec<SocketAddr> {
    let PeerSource::Lan { port, .. } = &peer.source else {
        return Vec::new();
    };

    let mut addrs: Vec<SocketAddr> = peer
        .addresses
        .iter()
        .map(|ip| SocketAddr::new(*ip, *port))
        .collect();
    addrs.sort_by_key(|addr| !addr.is_ipv4());
    addrs
}

/// How long to let a browse run before whatever it has found is what there is.
///
/// One-shot callers need a number; a long-lived [`Browser`] does not, because
/// it keeps taking in announcements for as long as it is held.
pub const SETTLE: Duration = Duration::from_millis(1_500);

/// A name fit to show in a device list.
fn display_name(raw: &str) -> String {
    let cleaned = for_log(raw.trim());
    if cleaned.chars().count() > MAX_NAME {
        cleaned.chars().take(MAX_NAME).collect()
    } else {
        cleaned
    }
}

/// `box._pravera._udp.local.` becomes `box`.
fn instance_name(fullname: &str) -> String {
    fullname
        .strip_suffix(SERVICE_TYPE)
        .and_then(|s| s.strip_suffix('.'))
        .unwrap_or(fullname)
        .to_string()
}

/// A hostname the daemon will accept, derived from the display name.
///
/// mDNS hostnames are a restricted alphabet, and a machine called
/// `Kleo's Laptop` has to become something that survives a DNS label.
fn hostname_for(name: &str) -> String {
    let mut host: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();

    host.truncate(48);
    let host = host.trim_matches('-').to_string();
    if host.is_empty() {
        "pravera.local.".to_string()
    } else {
        format!("{host}.local.")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The mDNS daemon refused to start or to register. Carries the library's
    /// own wording, which names sockets and interfaces, so it is logged and
    /// shown locally and never sent to a peer.
    #[error("local network discovery is unavailable: {0}")]
    Daemon(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_with_spaces_and_apostrophes_still_makes_a_hostname() {
        assert_eq!(hostname_for("Kleo's Laptop"), "kleo-s-laptop.local.");
        assert_eq!(hostname_for("EVERCORE"), "evercore.local.");
    }

    #[test]
    fn a_name_made_entirely_of_punctuation_falls_back_rather_than_producing_a_dot() {
        // `"...".local.` is not a hostname, and handing it to the daemon would
        // fail at registration rather than here.
        assert_eq!(hostname_for("!!!"), "pravera.local.");
        assert_eq!(hostname_for(""), "pravera.local.");
    }

    #[test]
    fn a_hostname_cannot_run_past_a_dns_label() {
        // The constraint is per label, not on the whole name: a label over 63
        // bytes is refused at registration, so the first one has to fit.
        let host = hostname_for(&"a".repeat(200));
        let label = host.split('.').next().unwrap();
        assert!(label.len() <= 63, "label was {} bytes", label.len());
        assert!(host.ends_with(".local."));
    }

    #[test]
    fn a_name_from_the_subnet_is_cut_to_something_that_fits_a_row() {
        let shouted = "X".repeat(200);
        assert_eq!(display_name(&shouted).chars().count(), MAX_NAME);
    }

    #[test]
    fn a_name_from_the_subnet_cannot_carry_control_characters_into_a_log() {
        // Anybody on the subnet writes this string. It reaches a log line and a
        // device list, so it goes through the same sanitiser every other
        // peer-supplied string does.
        let sneaky = "box\r\n[ERROR] fake log line";
        let shown = display_name(sneaky);
        assert!(!shown.contains('\n'), "got {shown:?}");
        assert!(!shown.contains('\r'), "got {shown:?}");
    }

    #[test]
    fn the_instance_name_is_recovered_when_an_advertisement_omits_the_display_name() {
        assert_eq!(instance_name("workshop._pravera._udp.local."), "workshop");
        // Anything that is not one of ours is left exactly as it came.
        assert_eq!(instance_name("odd-name"), "odd-name");
    }

    #[test]
    fn a_lan_peer_is_dialled_over_ipv4_first() {
        let peer = DiscoveredPeer {
            name: "workshop".into(),
            addresses: vec!["fe80::1".parse().unwrap(), "192.168.1.40".parse().unwrap()],
            source: PeerSource::Lan {
                port: 41_337,
                claims: "abc".into(),
                version: Some(2),
            },
            os: None,
            online: true,
            device_id: None,
        };

        let addrs = socket_addrs(&peer);
        assert!(addrs[0].is_ipv4(), "a v6 link-local has no scope here");
        assert_eq!(addrs[0].port(), 41_337);
        assert_eq!(addrs.len(), 2);
    }

    #[test]
    fn nothing_but_a_lan_peer_has_lan_addresses() {
        let peer = DiscoveredPeer {
            name: "elsewhere".into(),
            addresses: vec!["100.64.0.1".parse().unwrap()],
            source: PeerSource::Iroh,
            os: None,
            online: true,
            device_id: None,
        };
        assert!(socket_addrs(&peer).is_empty());
    }

    /// Publishing and finding, on one machine, over the real stack.
    ///
    /// Ignored by default because it needs multicast on a live interface, which
    /// a firewall or a CI container will refuse — and a discovery test that
    /// passes when it discovers nothing is not a test. Run it deliberately:
    ///
    /// ```text
    /// cargo test -p pravera-discovery -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs multicast on a real interface"]
    fn a_published_machine_can_be_found_again() {
        use std::time::Instant;

        let key = "TESTKEYTESTKEYTESTKEYTESTKEYTESTKEYTESTKEYTESTKEYTES";
        let _advert =
            Advertisement::publish("Pravera Test Host", key, 41_337, 2).expect("could not publish");

        // A different own_key, so the browser does not filter the advert out as
        // its own the way a real host-and-client machine would.
        let browser = Browser::start().expect("could not browse");
        browser.ignore("SOMETHINGELSE".into());

        let deadline = Instant::now() + Duration::from_secs(10);
        let found = loop {
            let peers = browser.peers();
            if let Some(peer) = peers.into_iter().find(|p| p.name == "Pravera Test Host") {
                break peer;
            }
            assert!(
                Instant::now() < deadline,
                "nothing was found in ten seconds"
            );
            std::thread::sleep(Duration::from_millis(100));
        };

        let PeerSource::Lan {
            port,
            claims,
            version,
        } = &found.source
        else {
            panic!("found on the wrong source: {:?}", found.source);
        };
        assert_eq!(*port, 41_337);
        assert_eq!(claims, key);
        assert_eq!(*version, Some(2));
        assert!(!found.addresses.is_empty(), "a peer with no address");
        assert!(!found.is_verified(), "mDNS must never prove an identity");
    }

    #[test]
    fn an_advertised_key_is_never_treated_as_a_proven_one() {
        // The distinction the whole module rests on. A peer found this way is
        // reachable; who it is stays unknown until the handshake.
        let peer = DiscoveredPeer {
            name: "claims-to-be-evercore".into(),
            addresses: vec!["192.168.1.9".parse().unwrap()],
            source: PeerSource::Lan {
                port: 1,
                claims: "somebody-elses-key".into(),
                version: Some(2),
            },
            os: None,
            online: true,
            device_id: None,
        };
        assert!(!peer.is_verified());
    }
}
