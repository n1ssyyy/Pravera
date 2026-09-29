//! Runs one discovery pass and prints what Pravera can see.
//!
//!     cargo run -p pravera-discovery --example scan

use pravera_discovery::{mdns, scan_all, Browser, PLAIN_USB_C_EXPLANATION};

#[tokio::main]
async fn main() {
    // Started first and given time to hear the subnet answer. mDNS is not a
    // request-and-reply, so a browser read the instant it is created has heard
    // nothing yet — which would look exactly like an empty network.
    let browser = match Browser::start() {
        Ok(browser) => Some(browser),
        Err(error) => {
            println!("mDNS unavailable: {error}");
            None
        }
    };
    if browser.is_some() {
        tokio::time::sleep(mdns::SETTLE).await;
    }

    let lan = browser.as_ref().map(Browser::peers).unwrap_or_default();
    let found = scan_all(lan).await;

    println!("== local subnet ==");
    if found.lan.is_empty() {
        println!("  no Pravera machines answering");
    }
    for peer in &found.lan {
        println!("  {:<28} {:?}", peer.name, peer.source);
    }
    println!();

    println!("== direct links ==");
    if found.direct_links.is_empty() {
        println!("  none");
        println!("  note: {PLAIN_USB_C_EXPLANATION}");
    }
    for link in &found.direct_links {
        println!(
            "  {:<28} {:<20} {}  (~{} Gbps)",
            link.interface,
            link.local_addr,
            link.kind.label(),
            link.kind.expected_gbps()
        );
    }

    println!("\n== tailscale ==");
    match &found.tailnet {
        None => println!("  not running or not logged in"),
        Some(net) => {
            println!("  tailnet:  {}", net.magic_dns_suffix);
            println!(
                "  this box: {}",
                net.self_name.as_deref().unwrap_or("unknown")
            );
            println!("  {} peer(s)", net.peers.len());
        }
    }

    println!("\n== peers, best route first ==");
    for peer in found.peers() {
        println!(
            "  {:<3} {:<14} {:<10} {:<22} {}",
            if peer.online { "up" } else { "--" },
            peer.name,
            peer.os.as_deref().unwrap_or("?"),
            peer.source.label(),
            peer.addresses
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}
