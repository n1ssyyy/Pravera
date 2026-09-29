//! Which discovery sources are live, and what each one found.
//!
//! Exists to answer the question the device list cannot: *why is machine X not
//! here?* Pravera looks for peers four different ways, three of which can be
//! off, unconfigured, or not written yet. Without this panel a missing machine
//! is indistinguishable from a broken app.
//!
//! It is also the one place the interface admits what is unbuilt. A source that
//! ships in a later phase says so and names the phase, rather than sitting
//! there greyed out and unexplained.

use iced::widget::{column, container, row, text, Space};
use iced::{Alignment, Color, Element, Length};

use pravera_discovery::Discovered;

use crate::components;
use crate::icon;
use crate::theme::{self, tokens as t};

/// What a source has to say for itself.
enum Health {
    /// Working, with a summary of what it found.
    Live(String),
    /// Working and correctly configured, but nothing found. Not a fault.
    Idle(String),
    /// Not available: switched off, not installed, or not yet written.
    Down(String),
}

impl Health {
    fn tint(&self) -> Color {
        match self {
            Health::Live(_) => t::ROUTE_DIRECT,
            Health::Idle(_) => t::NEUTRAL_600,
            Health::Down(_) => t::NEUTRAL_700,
        }
    }

    fn detail(&self) -> &str {
        match self {
            Health::Live(d) | Health::Idle(d) | Health::Down(d) => d,
        }
    }

    fn is_live(&self) -> bool {
        matches!(self, Health::Live(_))
    }
}

/// One card at the foot of the Devices page: a heading band, then the four
/// sources side by side, split by hairlines — DigiClip's health strip. A
/// strip rather than a list, so it costs the device list as little height as
/// it can.
pub fn view<'a, Message: 'a>(discovered: &Discovered) -> Element<'a, Message> {
    let sources = [
        (icon::CABLE, "Direct link", direct_link_health(discovered)),
        (icon::WIRELESS, "Local network", lan_health(discovered)),
        (icon::RELAY, "Tailscale", tailscale_health(discovered)),
        (icon::GLOBE, "Internet", iroh_health()),
    ];

    let live = sources.iter().filter(|(_, _, health)| health.is_live()).count();

    let mut strip = row![].height(Length::Fixed(CELL_HEIGHT));
    for (index, (glyph, name, health)) in sources.into_iter().enumerate() {
        if index > 0 {
            strip = strip.push(components::vrule(Length::Fill));
        }
        strip = strip.push(source_cell(glyph, name, health));
    }

    components::panel(column![
        container(
            row![
                components::section_label("Discovery"),
                Space::new().width(Length::Fill),
                text(format!("{live} of 4 live"))
                    .size(t::TEXT_XS)
                    .wrapping(text::Wrapping::None)
                    .style(theme::subtle),
            ]
            .align_y(Alignment::Center),
        )
        .padding([0.0, t::SPACE_4])
        .height(Length::Fixed(t::ROW_HEIGHT + 4.0))
        .center_y(Length::Fixed(t::ROW_HEIGHT + 4.0)),
        components::hairline(),
        strip,
    ])
    .padding(0)
    .width(Length::Fill)
    .into()
}

/// The height of one source's cell: its name over its state.
const CELL_HEIGHT: f32 = 60.0;

fn source_cell<'a, Message: 'a>(
    glyph: &'static str,
    name: &'static str,
    health: Health,
) -> Element<'a, Message> {
    let live = health.is_live();

    container(
        row![
            icon::stroked(glyph, t::ICON, if live { t::FOREGROUND } else { t::SUBTLE_FOREGROUND }),
            column![
                text(name)
                    .size(t::TEXT_SM)
                    .font(t::FONT_UI_MEDIUM)
                    .wrapping(text::Wrapping::None)
                    .style(theme::tinted(if live { t::FOREGROUND } else { t::NEUTRAL_300 })),
                row![
                    components::dot(health.tint(), 6.0),
                    text(health.detail().to_string())
                        .size(t::TEXT_XS)
                        .wrapping(text::Wrapping::None)
                        .style(theme::muted),
                ]
                .spacing(t::SPACE_1_5)
                .align_y(Alignment::Center),
            ]
            .spacing(3.0),
        ]
        .spacing(t::SPACE_3)
        .align_y(Alignment::Center),
    )
    .padding([0.0, t::SPACE_4])
    .width(Length::Fill)
    .height(Length::Fill)
    .center_y(Length::Fill)
    .clip(true)
    .into()
}

fn direct_link_health(discovered: &Discovered) -> Health {
    match discovered.direct_links.first() {
        Some(link) => Health::Live(format!("{} on {}", link.kind.label(), link.interface)),
        // Not a failure. Most of the time there is simply no cable, and calling
        // that an error would train the user to ignore this panel.
        None => Health::Idle("no cable connected".into()),
    }
}

fn lan_health(discovered: &Discovered) -> Health {
    match discovered.lan.len() {
        // Listening, and nothing has answered. Not a failure: most subnets
        // have exactly one Pravera machine on them, and this one is it.
        0 => Health::Idle("listening, nothing answering".into()),
        1 => Health::Live("1 machine on this network".into()),
        n => Health::Live(format!("{n} machines on this network")),
    }
}

fn tailscale_health(discovered: &Discovered) -> Health {
    match &discovered.tailnet {
        Some(net) => {
            let online = net.peers.iter().filter(|p| p.online).count();
            Health::Live(format!(
                "{}, {} of {} peers up",
                net.magic_dns_suffix,
                online,
                net.peers.len()
            ))
        }
        None => Health::Down("not running or not logged in".into()),
    }
}

fn iroh_health() -> Health {
    Health::Down("iroh arrives in P4".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pravera_discovery::tailscale::Tailnet;
    use pravera_discovery::{DiscoveredPeer, LinkKind, PeerSource, TsRoute};
    use std::net::{IpAddr, Ipv4Addr};

    fn peer(online: bool) -> DiscoveredPeer {
        DiscoveredPeer {
            name: "p".into(),
            addresses: vec![IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))],
            source: PeerSource::Tailscale {
                magic_dns: "p.ts.net".into(),
                route: TsRoute::Direct,
            },
            os: None,
            online,
            device_id: None,
        }
    }

    #[test]
    fn a_missing_cable_is_reported_as_idle_not_as_a_fault() {
        // An error tint on the common case would make the panel noise.
        let health = direct_link_health(&Discovered::default());
        assert!(matches!(health, Health::Idle(_)));
        assert_ne!(health.tint(), t::DESTRUCTIVE);
        assert!(!health.is_live());
    }

    #[test]
    fn a_connected_cable_names_the_interface_it_came_up_on() {
        let discovered = Discovered {
            lan: vec![],
            direct_links: vec![pravera_discovery::DirectLink {
                interface: "Ethernet 2".into(),
                kind: LinkKind::Usb4Net,
                local_addr: IpAddr::V4(Ipv4Addr::new(169, 254, 3, 1)),
            }],
            tailnet: None,
        };
        let health = direct_link_health(&discovered);
        assert!(health.is_live());
        assert!(
            health.detail().contains("Ethernet 2"),
            "got {:?}",
            health.detail()
        );
    }

    #[test]
    fn tailscale_reports_how_many_of_its_peers_are_actually_up() {
        let discovered = Discovered {
            lan: vec![],
            direct_links: vec![],
            tailnet: Some(Tailnet {
                magic_dns_suffix: "example.ts.net".into(),
                self_name: Some("here".into()),
                peers: vec![peer(true), peer(false), peer(false)],
            }),
        };
        let health = tailscale_health(&discovered);
        assert!(health.is_live());
        assert!(
            health.detail().contains("1 of 3"),
            "got {:?}",
            health.detail()
        );
    }

    #[test]
    fn a_stopped_tailscale_is_down_rather_than_silently_absent() {
        let health = tailscale_health(&Discovered::default());
        assert!(matches!(health, Health::Down(_)));
        assert!(!health.is_live());
    }

    /// Every unbuilt source has to name the phase that brings it in. "Coming
    /// soon" with no date is the vagueness this panel exists to remove.
    #[test]
    fn unbuilt_sources_cite_the_phase_that_delivers_them() {
        let health = iroh_health();
        assert!(matches!(health, Health::Down(_)));
        assert!(
            health.detail().contains("P4"),
            "an unbuilt source must say when it lands, got {:?}",
            health.detail()
        );
    }

    #[test]
    fn only_a_working_source_is_allowed_the_direct_colour() {
        // Green means a path exists. Spending it on an idle or unbuilt source
        // would break the one rule the palette has.
        assert_eq!(Health::Live(String::new()).tint(), t::ROUTE_DIRECT);
        assert_ne!(Health::Idle(String::new()).tint(), t::ROUTE_DIRECT);
        assert_ne!(Health::Down(String::new()).tint(), t::ROUTE_DIRECT);
    }
}
