//! A drawn diagram of how a peer is actually reached.
//!
//! Every other remote desktop tool hides the path and shows you a spinner. The
//! whole point of Pravera's discovery layer is that it knows the difference
//! between a cable, a subnet, a hole-punched WireGuard tunnel and a relay in
//! some other country, so the device list says which one you have got.
//!
//! The diagram is two endpoints and the path between them. What varies is the
//! path, and it varies in a way that maps onto the thing that actually costs
//! you milliseconds:
//!
//! | Route | Drawn as | Why |
//! |---|---|---|
//! | Cable | one heavy bar | shortest and fattest pipe there is |
//! | LAN | one plain line | direct, one hop of switching |
//! | Tunnel | line through a midpoint | direct, but encapsulated |
//! | Relay | line detouring up over a third node | the packets literally go somewhere else first |
//! | Offline | broken line | there is no path |
//!
//! Nothing here is invented. The relay shape appears when, and only when,
//! Tailscale reports `Relay` without a `CurAddr`, which is the same signal the
//! route ranking uses.

use iced::mouse;
use iced::widget::canvas::{self, Path, Stroke};
use iced::{Color, Element, Length, Point, Rectangle, Renderer, Size, Theme};

use pravera_discovery::{DiscoveredPeer, PeerSource, TsRoute};

/// Drawn width. Wide enough for the relay detour to be legible, narrow enough
/// to sit in a dense table row without becoming a column of its own.
pub const WIDTH: f32 = 56.0;
/// Drawn height. The relay node needs headroom above the baseline.
pub const HEIGHT: f32 = 16.0;

/// Which of the five diagrams to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// A physical cable. Ethernet, USB4 or an NCM bridge.
    Cable,
    /// Same subnet, found over mDNS.
    Lan,
    /// Encapsulated but direct: hole-punched WireGuard, or iroh after a
    /// successful hole punch.
    Tunnel,
    /// Bounced through a relay. Still end to end encrypted, measurably slower.
    Relay,
    /// Known, but not reachable right now.
    Broken,
}

impl Shape {
    /// Reads the shape off a discovered peer.
    ///
    /// Offline wins over everything: a peer that is not reachable has no path
    /// to draw, whatever route it would use if it came back.
    pub fn of(peer: &DiscoveredPeer) -> Shape {
        if !peer.online {
            return Shape::Broken;
        }
        match &peer.source {
            PeerSource::DirectLink { .. } => Shape::Cable,
            PeerSource::Lan { .. } => Shape::Lan,
            PeerSource::Tailscale { route, .. } => match route {
                TsRoute::Direct => Shape::Tunnel,
                TsRoute::Derp { .. } => Shape::Relay,
                // Tailscale is up and the peer is online, but the status output
                // named neither a direct address nor a relay. Draw the tunnel,
                // which is what it is, and let the route label carry the doubt.
                TsRoute::Unknown => Shape::Tunnel,
            },
            PeerSource::Iroh => Shape::Tunnel,
        }
    }

    /// Whether this path is a straight shot. Drives the tint.
    pub fn is_direct(self) -> bool {
        matches!(self, Shape::Cable | Shape::Lan | Shape::Tunnel)
    }

    /// Stroke weight of the path. Heavier reads as more bandwidth, which is
    /// true: a USB4 link is roughly two orders of magnitude fatter than a DERP
    /// relay and the diagram should not pretend otherwise.
    fn weight(self) -> f32 {
        match self {
            Shape::Cable => 3.0,
            Shape::Lan => 2.0,
            Shape::Tunnel => 1.5,
            Shape::Relay => 1.25,
            Shape::Broken => 1.25,
        }
    }
}

/// A [`canvas::Program`] drawing one route diagram.
#[derive(Debug)]
pub struct RouteMeter {
    shape: Shape,
    tint: Color,
    /// Multiplied into every alpha, so a row can fade the whole diagram in with
    /// the rest of its contents rather than having it pop.
    opacity: f32,
}

impl<Message> canvas::Program<Message> for RouteMeter {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let Size { width, height } = bounds.size();

        // Endpoints sit a node radius plus a hair in from each edge so the
        // round cap of the path never clips.
        //
        // Only the relay shape needs headroom, so only the relay shape gives up
        // the vertical centre for it. Dropping every diagram to a low baseline
        // to accommodate the one that detours would leave the other four
        // sitting off-centre in their row for no reason.
        let node_r = 2.25;
        let baseline = match self.shape {
            Shape::Relay => (height - 3.25).max(2.0),
            _ => height / 2.0,
        };
        let left = Point::new(node_r + 0.75, baseline);
        let right = Point::new(width - node_r - 0.75, baseline);

        let line = self.alpha(1.0);
        let weight = self.shape.weight();

        match self.shape {
            Shape::Relay => {
                // The detour is the whole message: the packets climb to a third
                // node and come back down. Apex sits at the top of the box.
                let apex = Point::new(width / 2.0, 2.5);
                frame.stroke(
                    &Path::new(|p| {
                        p.move_to(left);
                        p.line_to(apex);
                        p.line_to(right);
                    }),
                    self.stroke(weight, line),
                );
                frame.fill(&Path::circle(apex, node_r - 0.35), line);
            }
            Shape::Broken => {
                // A dashed rule reads as "there is supposed to be something
                // here", which is exactly the state being reported.
                frame.stroke(
                    &Path::line(left, right),
                    Stroke {
                        line_dash: canvas::LineDash {
                            segments: &[2.5, 3.0],
                            offset: 0,
                        },
                        ..self.stroke(weight, line)
                    },
                );
            }
            Shape::Tunnel => {
                // The midpoint node is the encapsulation: still one path, but
                // the bytes are wrapped on the way through. The line is drawn
                // in two segments rather than one behind the node, because a
                // stroke showing through a ring at this size reads as noise.
                let mid = width / 2.0;
                let gap = node_r + 1.25;
                frame.stroke(
                    &Path::line(left, Point::new(mid - gap, baseline)),
                    self.stroke(weight, line),
                );
                frame.stroke(
                    &Path::line(Point::new(mid + gap, baseline), right),
                    self.stroke(weight, line),
                );
                frame.stroke(
                    &Path::circle(Point::new(mid, baseline), node_r - 0.25),
                    self.stroke(1.25, line),
                );
            }
            Shape::Cable | Shape::Lan => {
                frame.stroke(&Path::line(left, right), self.stroke(weight, line));
            }
        }

        // Endpoints last, so the path never draws over them. Hollow when there
        // is no connection to be had.
        for end in [left, right] {
            if self.shape == Shape::Broken {
                frame.stroke(&Path::circle(end, node_r - 0.4), self.stroke(1.25, line));
            } else {
                frame.fill(&Path::circle(end, node_r), line);
            }
        }

        vec![frame.into_geometry()]
    }
}

impl RouteMeter {
    fn alpha(&self, a: f32) -> Color {
        Color {
            a: self.tint.a * a * self.opacity,
            ..self.tint
        }
    }

    fn stroke(&self, width: f32, color: Color) -> Stroke<'static> {
        Stroke {
            style: canvas::Style::Solid(color),
            width,
            line_cap: canvas::LineCap::Round,
            line_join: canvas::LineJoin::Round,
            ..Stroke::default()
        }
    }
}

/// Builds the widget.
pub fn view<'a, Message: 'a>(shape: Shape, tint: Color, opacity: f32) -> Element<'a, Message> {
    iced::widget::Canvas::new(RouteMeter {
        shape,
        tint,
        opacity,
    })
    .width(Length::Fixed(WIDTH))
    .height(Length::Fixed(HEIGHT))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pravera_discovery::LinkKind;
    use std::net::{IpAddr, Ipv4Addr};

    fn peer(source: PeerSource, online: bool) -> DiscoveredPeer {
        DiscoveredPeer {
            name: "test".into(),
            addresses: vec![IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))],
            source,
            os: None,
            online,
            device_id: None,
        }
    }

    #[test]
    fn a_cable_draws_the_heaviest_path() {
        // The weight ordering is the claim the diagram makes about bandwidth.
        // If it ever inverted, the picture would be actively misleading.
        assert!(Shape::Cable.weight() > Shape::Lan.weight());
        assert!(Shape::Lan.weight() > Shape::Tunnel.weight());
        assert!(Shape::Tunnel.weight() > Shape::Relay.weight());
    }

    #[test]
    fn an_offline_peer_has_no_path_whatever_route_it_would_use() {
        let p = peer(
            PeerSource::DirectLink {
                kind: LinkKind::Usb4Net,
            },
            false,
        );
        assert_eq!(Shape::of(&p), Shape::Broken);
    }

    #[test]
    fn a_relayed_tailnet_peer_is_drawn_as_a_detour() {
        let p = peer(
            PeerSource::Tailscale {
                magic_dns: "box.example.ts.net".into(),
                route: TsRoute::Derp {
                    region: "fra".into(),
                },
            },
            true,
        );
        assert_eq!(Shape::of(&p), Shape::Relay);
        assert!(!Shape::of(&p).is_direct());
    }

    #[test]
    fn a_hole_punched_tailnet_peer_is_drawn_as_direct() {
        let p = peer(
            PeerSource::Tailscale {
                magic_dns: "box.example.ts.net".into(),
                route: TsRoute::Direct,
            },
            true,
        );
        assert_eq!(Shape::of(&p), Shape::Tunnel);
        assert!(Shape::of(&p).is_direct());
    }

    #[test]
    fn every_cable_kind_draws_the_same_diagram() {
        for kind in [LinkKind::Ethernet, LinkKind::Usb4Net, LinkKind::Ncm] {
            assert_eq!(
                Shape::of(&peer(PeerSource::DirectLink { kind }, true)),
                Shape::Cable
            );
        }
    }

    #[test]
    fn only_the_relay_shape_claims_an_indirect_path() {
        assert!(Shape::Cable.is_direct());
        assert!(Shape::Lan.is_direct());
        assert!(Shape::Tunnel.is_direct());
        assert!(!Shape::Relay.is_direct());
        assert!(!Shape::Broken.is_direct());
    }
}
