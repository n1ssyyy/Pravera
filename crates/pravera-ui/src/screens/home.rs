//! Devices: everything Pravera can currently see, and how good the path to
//! each one is.
//!
//! The screen answers one question, and the layout is arranged so the answer
//! arrives in the order you would ask it: how many machines, how many
//! reachable, then which, then how. The leading column of the table is a drawn
//! diagram of the actual route, because the path is the thing this product
//! knows that others do not.
//!
//! ## What is deliberately absent
//!
//! There is no grid of bordered metric tiles across the top. Four boxes each
//! holding one number is the default shape for a screen like this, and it
//! spends a whole band of vertical space saying what one line can say. The
//! figures live in the subtitle instead.
//!
//! There are also no latency numbers. Nothing has measured one yet, and a
//! plausible-looking figure the app did not measure is worse than no figure.

use std::time::Instant;

use iced::widget::{button, column, container, mouse_area, opaque, pin, row, stack, text, Space};
use iced::{Alignment, Background, Border, Element, Length, Padding, Point, Size};

use pravera_core::DeviceId;
use pravera_discovery::{Discovered, DiscoveredPeer};

use crate::components::route_meter::{self, Shape};
use crate::components::{self, sources, stat};
use crate::icon;
use crate::motion::{self, HoverTracker, Tween};
use crate::net::known::{self, Known};
use crate::theme::{self, tokens as t};
use crate::Message;

/// One row of the Devices list: everything Pravera knows about a machine,
/// however it learned it.
///
/// A machine appears once, whatever discovered it and however many times it
/// has been connected to. Discovery and the remembered list are two views of
/// the same machines, and showing them as two lists made the person reconcile
/// them by eye — the one job a list of machines should never hand back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// What to call it. The discovered name when there is one — it is the
    /// name the machine answers to on the network right now — otherwise the
    /// name it called itself the last time a session ran.
    pub name: String,
    /// The fingerprint, when something gave enough to compute one: a
    /// remembered connect code, or a discovery source that announced the key.
    pub device_id: Option<DeviceId>,
    /// A connect code, when one is held. A code remembered from a past
    /// session outranks one advertised on the network: the pin was proved by
    /// a completed handshake, the advertisement is only a claim.
    pub code: Option<String>,
    /// The account signed in with last time, if remembered.
    pub username: Option<String>,
    /// The discovery view, when the machine is visible right now.
    pub peer: Option<DiscoveredPeer>,
    /// Whether a session has actually been made to this machine.
    pub saved: bool,
}

impl Entry {
    /// Whether the machine is reachable right now. A remembered machine no
    /// discovery can see is not online, whatever its past.
    pub fn online(&self) -> bool {
        self.peer.as_ref().is_some_and(|peer| peer.online)
    }

    /// How the path reads, in the row's own words.
    pub fn route_label(&self) -> String {
        match &self.peer {
            Some(peer) if peer.online => peer.source.label(),
            Some(_) => "offline".to_string(),
            None => "remembered".to_string(),
        }
    }

    /// Where it appears to be, if discovery gave an address: an IPv4 one when
    /// there is one, then any other IPv6, and a link-local one (`fe80::`, which
    /// means nothing outside its own cable) only when it is all there is.
    pub fn address(&self) -> Option<String> {
        let addresses = &self.peer.as_ref()?.addresses;
        addresses
            .iter()
            .min_by_key(|address| match address {
                std::net::IpAddr::V4(_) => 0,
                std::net::IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 != 0xfe80 => 1,
                std::net::IpAddr::V6(_) => 2,
            })
            .map(|address| address.to_string())
    }

    /// Whether a remembered machine and this row are the same machine.
    ///
    /// Fingerprints decide when both sides have one, and they are decisive:
    /// the code is the key, and the key is the identity. Hostnames decide
    /// otherwise, which is an offer rather than a proof — see
    /// [`known::Machine::answers_to`] for why that is still safe.
    fn matches(&self, machine: &known::Machine) -> bool {
        if let (Some(held), Some(past)) = (self.device_id, machine.device_id()) {
            return held == past;
        }
        first_label(&self.name) == first_label(&machine.name)
    }
}

/// The first label of a hostname, lowercased. `Evercore` and
/// `evercore.ts.net` are the same machine to a person scanning the list.
fn first_label(name: &str) -> String {
    name.trim()
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// The Devices list: every machine, each once.
///
/// Discovery supplies the live rows; the remembered list fills them in and
/// adds the machines that are not visible right now. A pinned connect code
/// always displaces an advertised one, because the pin was earned.
pub fn entries(discovered: &Discovered, known: &Known) -> Vec<Entry> {
    let mut merged: Vec<Entry> = discovered
        .peers()
        .iter()
        .map(|peer| Entry {
            name: peer.name.clone(),
            device_id: peer.device_id,
            code: match &peer.source {
                pravera_discovery::PeerSource::Lan { claims, .. } => Some(claims.clone()),
                _ => None,
            },
            username: None,
            peer: Some(peer.clone()),
            saved: false,
        })
        .collect();

    for machine in known.machines() {
        match merged.iter_mut().find(|entry| entry.matches(machine)) {
            Some(entry) => {
                entry.code = Some(machine.code.clone());
                entry.username = Some(machine.username.clone());
                entry.saved = true;
                if entry.device_id.is_none() {
                    entry.device_id = machine.device_id();
                }
            }
            None => merged.push(Entry {
                name: machine.name.clone(),
                device_id: machine.device_id(),
                code: Some(machine.code.clone()),
                username: Some(machine.username.clone()),
                peer: None,
                saved: true,
            }),
        }
    }

    merged
}

/// Column widths. Fixed so every row lines up without a table widget; the
/// name column takes whatever these leave.
const COL_PATH: f32 = route_meter::WIDTH;
const COL_ROUTE: f32 = 150.0;

/// Height of one device row: two lines of text and an avatar with room around
/// them, and an easy target.
const ROW_HEIGHT: f32 = 52.0;

/// The machine's initial on its colour.
const AVATAR: f32 = 28.0;

/// The context menu's width, and the height of one of its entries.
const MENU_WIDTH: f32 = 220.0;
const MENU_ITEM: f32 = t::ROW_HEIGHT;

/// How many entries a context menu offers.
const MENU_ITEMS: usize = 3;

/// Animation and interaction state owned by this screen.
///
/// `view` takes `&self` and so cannot start an animation; everything that moves
/// is set up here in response to a message and merely read during rendering.
#[derive(Debug)]
pub struct State {
    /// When the page last arrived. Its panels cascade in from this moment.
    arrived: Instant,
    hover: HoverTracker,
    /// The row whose context menu is open, and where. One at a time: two
    /// menus would both claim to be about a machine, and neither would be.
    menu: Option<Menu>,
    /// The menu arriving and leaving. The menu stays mounted until this has
    /// gone all the way back to zero.
    menu_shown: Tween,
    menu_hover: HoverTracker,
    /// Fixed point for the scan spinner, so its angle is a pure function of
    /// the current time rather than something advanced by hand.
    epoch: Instant,
}

#[derive(Debug, Clone, Copy)]
struct Menu {
    row: usize,
    /// Where the pointer was, in window coordinates.
    at: Point,
    /// Set once the menu has been asked to leave.
    closing: bool,
}

impl Default for State {
    fn default() -> Self {
        let now = Instant::now();
        State {
            arrived: now,
            hover: HoverTracker::default(),
            menu: None,
            menu_shown: Tween::default(),
            menu_hover: HoverTracker::new(MENU_ITEMS),
            epoch: now,
        }
    }
}

impl State {
    /// The list changed, for any reason. Only the hover slots follow it: the
    /// list's panel arrived with the page, and rows that appear later simply
    /// appear, rather than replaying an entrance on a timer.
    pub fn on_entries(&mut self, count: usize, now: Instant) {
        self.hover.resize(count, now);
        // Rows the list forgot cannot keep a menu open about them.
        if self.menu.is_some_and(|menu| menu.row >= count) {
            self.menu = None;
            self.menu_shown.snap(0.0);
        }
    }

    /// The page is arriving: its panels cascade in from `at`.
    pub fn replay(&mut self, at: Instant) {
        self.arrived = at;
    }

    pub fn set_hovered(&mut self, index: usize, entering: bool, now: Instant) {
        self.hover.set(index, entering, now);
    }

    /// Open the menu for `row` at the pointer.
    pub fn open_menu(&mut self, row: usize, at: Point, now: Instant) {
        self.menu = Some(Menu {
            row,
            at,
            closing: false,
        });
        self.menu_hover.clear(now);
        self.menu_shown.snap(0.0);
        self.menu_shown.enter(now, motion::MENU_IN);
    }

    /// Put the menu away through its exit. It stays mounted until
    /// [`State::take_finished_menu_close`] sees the exit land.
    pub fn close_menu(&mut self, now: Instant) {
        if let Some(menu) = &mut self.menu {
            if !menu.closing {
                menu.closing = true;
                self.menu_shown.exit(now, motion::MENU_OUT);
            }
        }
    }

    /// Whether a menu is open and not on its way out.
    pub fn is_menu_open(&self) -> bool {
        self.menu.is_some_and(|menu| !menu.closing)
    }

    /// Unmounts a menu whose exit has finished. Called from the pump.
    pub fn take_finished_menu_close(&mut self, now: Instant) -> bool {
        if self.menu.is_some_and(|menu| menu.closing) && self.menu_shown.is_gone(now) {
            self.menu = None;
            true
        } else {
            false
        }
    }

    /// The row whose menu is open, taken so the action is answered once. An
    /// instant close: the caller is acting on the row, and the dialog that
    /// follows is the next thing to look at.
    pub fn take_menu(&mut self) -> Option<usize> {
        let row = self.menu.map(|menu| menu.row);
        self.menu = None;
        self.menu_shown.snap(0.0);
        row
    }

    pub fn set_menu_hovered(&mut self, slot: usize, entering: bool, now: Instant) {
        self.menu_hover.set(slot, entering, now);
    }

    fn hovered(&self, index: usize, now: Instant) -> f32 {
        self.hover.amount(index, now)
    }

    /// Turns of the scan spinner at `now`. Roughly one revolution per second.
    fn spin(&self, now: Instant) -> f32 {
        now.saturating_duration_since(self.epoch).as_secs_f32().fract()
    }

    pub fn is_animating(&self, now: Instant) -> bool {
        motion::cascading(self.arrived, now)
            || self.hover.is_animating(now)
            || self.menu_hover.is_animating(now)
            || self.menu_shown.is_animating(now)
    }
}

// --------------------------------------------------------------------- page

pub fn view<'a>(
    state: &'a State,
    entries: &'a [Entry],
    discovered: &'a Discovered,
    scanning: bool,
    now: Instant,
) -> Element<'a, Message> {
    let since = state.arrived;

    let mut content = column![].width(Length::Fill);
    if let Some(banner) = direct_link_banner(discovered) {
        content = content.push(motion::settle(banner, motion::cascade(since, now, 1)));
    }

    let body: Element<'a, Message> = if entries.is_empty() {
        content = content.push(
            container(motion::settle(empty_state(scanning), motion::cascade(since, now, 1)))
                .center(Length::Fill),
        );
        content.height(Length::Fill).into()
    } else {
        content = content.push(device_groups(state, entries, now));
        content = content.push(hint());
        components::scroll(content)
    };

    // Answers the question the list above cannot: why a machine you expected
    // is not in it. Three of the four discovery sources can be off,
    // unconfigured, or not written yet, and a missing peer is otherwise
    // indistinguishable from a broken app.
    let footer = motion::settle(sources::footer(discovered), motion::cascade(since, now, 2));

    components::page_footed(
        motion::settle(header(entries, scanning, state.spin(now)), motion::cascade(since, now, 0)),
        body,
        footer,
    )
}

// ------------------------------------------------------------------- header

fn header(entries: &[Entry], scanning: bool, spin: f32) -> Element<'static, Message> {
    let online = entries.iter().filter(|e| e.online()).count();
    let direct = entries
        .iter()
        .filter(|e| e.peer.as_ref().is_some_and(|p| Shape::of(p).is_direct()))
        .count();
    let relayed = entries
        .iter()
        .filter(|e| e.peer.as_ref().is_some_and(|p| Shape::of(p) == Shape::Relay))
        .count();

    // One line of figures instead of a band of tiles. Each number is tinted
    // by what it means, so the line reads at a glance without any of them
    // needing a box around it.
    let mut ledger = row![].spacing(t::SPACE_2).align_y(Alignment::Center);
    ledger = ledger.push(stat::figure(entries.len(), "known", t::FOREGROUND));
    ledger = ledger.push(stat::separator());
    ledger = ledger.push(stat::figure(
        online,
        "online",
        if online > 0 { t::FOREGROUND } else { t::SUBTLE_FOREGROUND },
    ));
    if direct > 0 {
        ledger = ledger.push(stat::separator());
        ledger = ledger.push(stat::figure(direct, "direct", t::ROUTE_DIRECT));
    }
    if relayed > 0 {
        ledger = ledger.push(stat::separator());
        ledger = ledger.push(stat::figure(relayed, "relayed", t::ROUTE_RELAY));
    }

    components::header("Devices")
        .meta(ledger)
        .action(scan_button(scanning, spin))
        .action(components::small_button(Some(icon::ADD), "Add device", Some(Message::AddDevice)))
        .into()
}

fn scan_button(scanning: bool, spin: f32) -> Element<'static, Message> {
    // The spinner is the honest signal: the button stays pressable, and the
    // rotation says work is in flight without claiming a percentage the scan
    // cannot know.
    let tint = if scanning { t::MUTED_FOREGROUND } else { t::FOREGROUND };
    let label = row![
        icon::turned(icon::REFRESH, t::ICON_SM, tint, if scanning { -spin } else { 0.0 }),
        text(if scanning { "Scanning" } else { "Scan" })
            .size(t::TEXT_SM)
            .font(t::FONT_UI_MEDIUM)
            .wrapping(text::Wrapping::None)
            .style(theme::tinted(tint)),
    ]
    .spacing(t::SPACE_2)
    .align_y(Alignment::Center);

    components::glide(|hover| {
        button(label)
            .padding(components::BUTTON_PADDING_SM)
            .style(theme::gliding(hover, theme::ghost_button))
            .on_press(Message::Refresh)
            .into()
    })
}

// ------------------------------------------------------------------- banner

/// Shown when a cable is plugged in.
///
/// This is the headline event the whole direct-link subsystem exists for, so it
/// gets a band of its own across the top of the list rather than a row in it.
fn direct_link_banner(discovered: &Discovered) -> Option<Element<'static, Message>> {
    let link = discovered.direct_links.first()?;

    let detail = format!(
        "{} on {}, about {} Gbps",
        link.kind.label(),
        link.interface,
        link.kind.expected_gbps()
    );

    Some(
        column![
            container(
                row![
                    container(components::hero(icon::CABLE, t::ROUTE_DIRECT, "Direct link up", detail))
                        .width(Length::Fill),
                    components::pill("Fastest path", components::Tone::Success),
                ]
                .spacing(t::SPACE_3)
                .align_y(Alignment::Center),
            )
            .padding([t::SPACE_3, t::SPACE_6]),
            components::hairline(),
        ]
        .width(Length::Fill)
        .into(),
    )
}

// --------------------------------------------------------------------- list

/// The list, in two groups: what can be reached now, then what cannot. Each
/// group is headed by a quiet label with its count, and each row keeps the
/// position it has in `entries`, because that is what its messages name.
fn device_groups<'a>(state: &'a State, entries: &'a [Entry], now: Instant) -> Element<'a, Message> {
    let since = state.arrived;
    let (online, offline): (Vec<usize>, Vec<usize>) =
        (0..entries.len()).partition(|&index| entries[index].online());

    let mut list = column![].width(Length::Fill);
    // Heads and rows arrive as one sequence, each a beat after the one above.
    let mut step = 0;
    for (label, members) in [("Online", online), ("Offline", offline)] {
        if members.is_empty() {
            continue;
        }
        list = list.push(motion::settle(
            group_head(label, members.len()),
            motion::row_cascade(since, now, 1, step),
        ));
        step += 1;
        for (place, index) in members.into_iter().enumerate() {
            if place > 0 {
                list = list.push(container(components::hairline()).padding([0.0, t::SPACE_6]));
            }
            list = list.push(motion::settle(
                device_row(index, &entries[index], state.hovered(index, now)),
                motion::row_cascade(since, now, 1, step),
            ));
            step += 1;
        }
    }
    list.into()
}

/// The label over a group of rows: what the group is, and how many are in it.
fn group_head(label: &str, count: usize) -> Element<'static, Message> {
    container(
        row![
            components::section_label(label),
            text(count.to_string())
                .size(t::TEXT_XS)
                .wrapping(text::Wrapping::None)
                .style(theme::subtle),
        ]
        .spacing(t::SPACE_2)
        .align_y(Alignment::Center),
    )
    .padding(Padding {
        top: t::SPACE_5,
        right: t::SPACE_6,
        bottom: t::SPACE_2,
        left: t::SPACE_6,
    })
    .into()
}

/// Under the last group, so a short list reads as finished rather than cut off.
fn hint() -> Element<'static, Message> {
    container(
        text("Machines on this network appear here on their own. Add one by its connect code from anywhere.")
            .size(t::TEXT_XS)
            .style(theme::subtle),
    )
    .padding(Padding {
        top: t::SPACE_6,
        right: t::SPACE_6,
        bottom: t::SPACE_6,
        left: t::SPACE_6,
    })
    .width(Length::Fill)
    .into()
}

/// What a machine's system is called on screen.
fn system_name(os: &str) -> String {
    match os.trim().to_ascii_lowercase().as_str() {
        "windows" => "Windows".to_string(),
        "linux" => "Linux".to_string(),
        "macos" | "darwin" => "macOS".to_string(),
        other => other.to_string(),
    }
}

/// The small line under a machine's name: where it is and what it runs. Parts
/// nothing has reported are left out rather than spelled "unknown". A machine
/// with neither says who it was last signed in as, when it remembers.
fn subline(entry: &Entry) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(address) = entry.address() {
        parts.push(address);
    }
    if let Some(system) = entry.peer.as_ref().and_then(|peer| peer.os.as_deref()) {
        let system = system_name(system);
        if !system.is_empty() {
            parts.push(system);
        }
    }
    if parts.is_empty() {
        return entry.username.as_ref().map(|user| format!("signed in as {user}"));
    }
    Some(parts.join(" \u{00b7} "))
}

/// The route in words: what carries the traffic, or that nothing does.
fn route_words(entry: &Entry) -> String {
    if entry.online() {
        entry.route_label()
    } else {
        "Offline".to_string()
    }
}

/// One machine: who it is, how it is reached, and what can be done with it,
/// which shows only under the pointer.
///
/// The row is the connect button, so the whole width answers a press. The
/// actions on the right are separate buttons on top of it; they exist for the
/// two other things a machine can be asked, and to make the row's menu findable
/// without a right click.
fn device_row(index: usize, entry: &Entry, hover: f32) -> Element<'static, Message> {
    let shape = entry.peer.as_ref().map(Shape::of).unwrap_or(Shape::Broken);
    let tint = route_tint(shape);

    let online = entry.online();
    let alpha = if online { 1.0 } else { 0.55 };

    let name = text(entry.name.clone())
        .size(t::TEXT_SM)
        .wrapping(text::Wrapping::None)
        .font(t::FONT_UI_STRONG)
        .style(theme::tinted(if online {
            t::FOREGROUND
        } else {
            t::MUTED_FOREGROUND
        }));

    // A padlock beside the name means this machine has been connected to
    // before, so choosing it fills in its code and username and asks only for
    // the password. Drawn quietly: it answers a question not yet asked.
    let mut title = row![name].spacing(t::SPACE_2).align_y(Alignment::Center);
    if entry.saved {
        title = title.push(icon::stroked(icon::LOCK, 11.0, t::SUBTLE_FOREGROUND));
    }
    let mut lines = column![title].spacing(2.0);
    if let Some(line) = subline(entry) {
        lines = lines.push(
            text(line)
                .size(t::TEXT_XS)
                .font(t::FONT_MONO)
                .wrapping(text::Wrapping::None)
                .style(theme::tinted(theme::faded(t::SUBTLE_FOREGROUND, alpha))),
        );
    }

    // An unreachable peer gets no route description. Tailscale keeps reporting
    // the last relay it saw for a machine that has been asleep for a week, and
    // rendering that would say there is a path where there is none.
    let route = row![
        components::dot(theme::faded(tint, alpha), 6.0),
        text(route_words(entry))
            .size(t::TEXT_XS)
            .wrapping(text::Wrapping::None)
            .style(theme::tinted(theme::faded(t::NEUTRAL_300, alpha))),
    ]
    .spacing(t::SPACE_2)
    .align_y(Alignment::Center);

    let body = row![
        components::avatar_faded(&entry.name, AVATAR, alpha),
        container(lines).width(Length::Fill).clip(true),
        container(route_meter::view(shape, tint, alpha)).width(Length::Fixed(COL_PATH)),
        container(route).width(Length::Fixed(COL_ROUTE)).clip(true),
        actions(index, hover),
    ]
    .spacing(t::SPACE_4)
    .align_y(Alignment::Center);

    let surface = button(container(body).center_y(Length::Fill).width(Length::Fill).height(Length::Fill))
        .width(Length::Fill)
        .height(Length::Fixed(ROW_HEIGHT))
        .padding([0.0, t::SPACE_6])
        .style(move |_, status| button::Style {
            // Under the pointer the row lifts; pressed, it settles back
            // towards the surface it sits on.
            background: Some(Background::Color(match status {
                button::Status::Pressed => t::NEUTRAL_825,
                _ => t::with_alpha(t::NEUTRAL_800, hover * 0.8),
            })),
            text_color: t::FOREGROUND,
            border: Border::default(),
            ..button::Style::default()
        })
        .on_press(Message::ChooseDevice(index));

    mouse_area(surface)
        .on_right_press(Message::DeviceMenu(index))
        .on_enter(Message::HoverDevice(index, true))
        .on_exit(Message::HoverDevice(index, false))
        .into()
}

/// What can be done to a machine from its row: connect, open a terminal, or the
/// menu with the rest. All three fade in with the row's hover.
fn actions(index: usize, show: f32) -> Element<'static, Message> {
    let connect = components::glide(|hover| {
        button(components::label(
            Some(icon::CONNECT),
            "Connect",
            theme::faded(t::PRIMARY_FOREGROUND, show),
        ))
        .padding(components::BUTTON_PADDING_SM)
        .style(move |theme, status| {
            theme::fade_button(theme::glided(theme::primary_button, hover.get(), theme, status), show)
        })
        .on_press(Message::ChooseDevice(index))
        .into()
    });

    row![
        connect,
        action_icon(icon::TERMINAL, Message::MenuTerminal(index), show),
        action_icon(icon::MORE, Message::DeviceMenu(index), show),
    ]
    .spacing(t::SPACE_1)
    .align_y(Alignment::Center)
    .into()
}

fn action_icon(glyph: &'static str, message: Message, show: f32) -> Element<'static, Message> {
    components::glide(|hover| {
        button(
            container(icon::stroked(glyph, t::ICON_SM, theme::faded(t::MUTED_FOREGROUND, show)))
                .center_x(Length::Fill)
                .center_y(Length::Fill),
        )
        .width(Length::Fixed(t::CONTROL_HEIGHT_SM))
        .height(Length::Fixed(t::CONTROL_HEIGHT_SM))
        .padding(0)
        .style(move |theme, status| {
            theme::fade_button(theme::glided(theme::ghost_button, hover.get(), theme, status), show)
        })
        .on_press(message)
        .into()
    })
}

// --------------------------------------------------------------------- menu

/// The row's context menu, drawn at the pointer over the whole window.
///
/// `None` while no menu is mounted. The layer under the menu is transparent
/// and closes it on any press — a menu dims nothing, because it is a glance,
/// not a decision.
pub fn menu_view<'a>(
    state: &'a State,
    entries: &'a [Entry],
    window: Size,
    now: Instant,
) -> Option<Element<'a, Message>> {
    let menu = state.menu?;
    let entry = entries.get(menu.row)?;
    let alpha = state.menu_shown.value(now).clamp(0.0, 1.0);
    let row = menu.row;

    let heading = container(
        text(entry.name.clone())
            .size(t::TEXT_XS)
            .wrapping(iced::widget::text::Wrapping::None)
            .style(theme::tinted(theme::faded(t::SUBTLE_FOREGROUND, alpha))),
    )
    .padding([t::SPACE_1, t::SPACE_2])
    .clip(true);

    let items = column![
        heading,
        menu_item(state, now, alpha, 0, icon::CONNECT, "Connect", false, Message::MenuConnect(row)),
        menu_item(state, now, alpha, 1, icon::TERMINAL, "Open terminal", false, Message::MenuTerminal(row)),
        container(Space::new().height(1.0))
            .width(Length::Fill)
            .padding([t::SPACE_1, 0.0])
            .style(move |_| container::Style {
                background: Some(Background::Color(theme::faded(t::BORDER, alpha))),
                ..container::Style::default()
            }),
        menu_item(state, now, alpha, 2, icon::FORGET, "Forget", true, Message::MenuForget(row)),
    ]
    .spacing(1.0);

    let card = components::panel(items)
        .edge(t::BEVEL_RAISED)
        .fill(t::POPOVER)
        .padding(t::SPACE_1)
        .width(Length::Fixed(MENU_WIDTH))
        .opacity(alpha)
        .shadow(iced::Shadow {
            color: theme::faded(t::SHADOW_INK, 0.45 * alpha),
            ..theme::SHADOW_FLOAT
        });

    // Kept inside the window: a menu opened near the right or bottom edge
    // flips to the other side of the pointer rather than being cut off.
    let height = MENU_ITEM * MENU_ITEMS as f32 + 26.0 + 12.0;
    let x = if menu.at.x + MENU_WIDTH + t::SPACE_2 > window.width {
        (menu.at.x - MENU_WIDTH).max(t::SPACE_2)
    } else {
        menu.at.x
    };
    let y = if menu.at.y + height + t::SPACE_2 > window.height {
        (menu.at.y - height).max(t::SPACE_2)
    } else {
        menu.at.y
    };

    let catcher = mouse_area(container(Space::new()).width(Length::Fill).height(Length::Fill))
        .on_press(Message::MenuClosed)
        .on_right_press(Message::MenuClosed);

    Some(
        stack![
            catcher,
            pin(opaque(motion::pop(card, alpha))).x(x).y(y),
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .into(),
    )
}

/// One entry of a context menu: a mark and what it does.
///
/// `destructive` entries carry the one hue the interface reserves for
/// something being taken away, and only under the pointer: at rest the menu
/// is monochrome, and the red arriving is the confirmation that this entry is
/// the one that costs something.
#[allow(clippy::too_many_arguments)]
fn menu_item<'a>(
    state: &'a State,
    now: Instant,
    alpha: f32,
    slot: usize,
    glyph: &'static str,
    label: &'a str,
    destructive: bool,
    message: Message,
) -> Element<'a, Message> {
    let hover = state.menu_hover.amount(slot, now);
    let tint = if destructive {
        theme::blend(t::NEUTRAL_300, t::DESTRUCTIVE_TEXT, hover)
    } else {
        theme::blend(t::NEUTRAL_300, t::FOREGROUND, hover)
    };
    let tint = theme::faded(tint, alpha);
    let background = if destructive {
        t::with_alpha(t::DESTRUCTIVE, 0.16 * hover * alpha)
    } else {
        t::with_alpha(t::NEUTRAL_800, hover * alpha)
    };

    let entry = button(
        row![
            icon::stroked(glyph, t::ICON_SM, tint),
            text(label).size(t::TEXT_SM).style(theme::tinted(tint)),
        ]
        .spacing(t::SPACE_2)
        .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .height(Length::Fixed(MENU_ITEM))
    .padding([0.0, t::SPACE_2])
    .style(move |_, status| button::Style {
        background: Some(Background::Color(match status {
            button::Status::Pressed => t::with_alpha(t::NEUTRAL_700, alpha),
            _ => background,
        })),
        text_color: tint,
        border: Border {
            radius: t::RADIUS_SM.into(),
            ..Border::default()
        },
        ..button::Style::default()
    })
    .on_press(message);

    // The button reports its own presses; the area around it reports the
    // pointer, because a button's hovered style is instant by contract and
    // the menu's is animated like everything else.
    mouse_area(entry)
        .on_enter(Message::MenuHover(slot, true))
        .on_exit(Message::MenuHover(slot, false))
        .into()
}

// -------------------------------------------------------------------- empty

fn empty_state(scanning: bool) -> Element<'static, Message> {
    let (glyph, headline, detail) = if scanning {
        (
            icon::WIRELESS,
            "Looking for devices",
            "Checking cables, the local subnet and the tailnet.",
        )
    } else {
        (
            icon::CABLE,
            "No devices found",
            "Plug a cable into another machine, start Tailscale, or add one by its connect code.",
        )
    };

    components::empty(glyph, headline, detail).into()
}

/// The one place colour is spent on this screen.
fn route_tint(shape: Shape) -> iced::Color {
    match shape {
        Shape::Broken => t::ROUTE_OFFLINE,
        Shape::Relay => t::ROUTE_RELAY,
        _ => t::ROUTE_DIRECT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pravera_discovery::{LinkKind, PeerSource, TsRoute};
    use std::net::{IpAddr, Ipv4Addr};

    fn peer(name: &str, source: PeerSource, online: bool) -> DiscoveredPeer {
        DiscoveredPeer {
            name: name.into(),
            addresses: vec![IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))],
            source,
            os: Some("linux".into()),
            online,
            device_id: None,
        }
    }

    #[test]
    fn colour_is_only_ever_spent_on_the_path() {
        // Three outcomes, three colours, and nothing else on the screen gets a
        // hue. A fourth appearing here means the palette rule has slipped.
        assert_eq!(route_tint(Shape::Cable), t::ROUTE_DIRECT);
        assert_eq!(route_tint(Shape::Lan), t::ROUTE_DIRECT);
        assert_eq!(route_tint(Shape::Tunnel), t::ROUTE_DIRECT);
        assert_eq!(route_tint(Shape::Relay), t::ROUTE_RELAY);
        assert_eq!(route_tint(Shape::Broken), t::ROUTE_OFFLINE);
    }

    #[test]
    fn an_offline_peer_is_never_painted_as_reachable() {
        let p = peer(
            "gone",
            PeerSource::DirectLink {
                kind: LinkKind::Ethernet,
            },
            false,
        );
        assert_eq!(route_tint(Shape::of(&p)), t::ROUTE_OFFLINE);
    }

    #[test]
    fn the_ledger_counts_agree_with_the_diagrams() {
        let peers = [
            peer(
                "a",
                PeerSource::DirectLink {
                    kind: LinkKind::Usb4Net,
                },
                true,
            ),
            peer(
                "b",
                PeerSource::Lan {
                    port: 41_337,
                    claims: "not-a-real-key".into(),
                    version: Some(2),
                },
                true,
            ),
            peer(
                "c",
                PeerSource::Tailscale {
                    magic_dns: "c.ts.net".into(),
                    route: TsRoute::Derp {
                        region: "fra".into(),
                    },
                },
                true,
            ),
            peer("d", PeerSource::Iroh, false),
        ];

        let online = peers.iter().filter(|p| p.online).count();
        let direct = peers.iter().filter(|p| Shape::of(p).is_direct()).count();
        let relayed = peers.iter().filter(|p| Shape::of(p) == Shape::Relay).count();

        assert_eq!(online, 3);
        assert_eq!(direct, 2, "the offline iroh peer must not count as direct");
        assert_eq!(relayed, 1);
        assert!(direct + relayed <= online, "a peer cannot have a path while offline");
    }

    #[test]
    fn the_page_cascades_in_and_then_rests() {
        let start = Instant::now();
        let mut state = State::default();
        state.replay(start);
        assert!(state.is_animating(start));
        let settled = start + motion::ENTRANCE + motion::STAGGER_MAX + motion::STAGGER_MAX;
        assert!(!state.is_animating(settled));
    }

    #[test]
    fn the_spinner_stays_within_one_revolution() {
        let state = State::default();
        let start = state.epoch;
        for millis in [0, 250, 999, 1_500, 60_000] {
            let turns = state.spin(start + std::time::Duration::from_millis(millis));
            assert!((0.0..1.0).contains(&turns), "{millis}ms produced {turns} turns");
        }
    }

    #[test]
    fn hovering_one_row_leaves_the_others_untinted() {
        let now = Instant::now();
        let mut state = State::default();
        state.hover.resize(5, now);
        state.set_hovered(3, true, now);

        let settled = now + motion::MICRO + std::time::Duration::from_millis(1);
        assert_eq!(state.hovered(3, settled), 1.0);
        assert_eq!(state.hovered(2, settled), 0.0);
        assert_eq!(state.hovered(4, settled), 0.0);
    }

    #[test]
    fn a_menu_fades_out_before_it_unmounts() {
        let now = Instant::now();
        let mut state = State::default();
        state.on_entries(3, now);
        state.open_menu(1, Point::new(10.0, 10.0), now);
        assert!(state.is_menu_open());

        // Closed once it has fully arrived, so there is an exit to play.
        let later = now + motion::MENU_IN + motion::MICRO;
        state.close_menu(later);
        assert!(!state.is_menu_open(), "a closing menu no longer counts as open");
        assert!(!state.take_finished_menu_close(later), "unmounted before its exit played");
        assert!(state.take_finished_menu_close(later + motion::MENU_OUT));
        assert!(state.menu.is_none());
    }

    #[test]
    fn a_list_that_shrinks_under_an_open_menu_closes_it() {
        let now = Instant::now();
        let mut state = State::default();
        state.on_entries(3, now);
        state.open_menu(2, Point::ORIGIN, now);
        state.on_entries(1, now);
        assert!(state.menu.is_none());
    }


    // ------------------------------------------------------------- the row

    fn entry(peer: Option<DiscoveredPeer>, username: Option<&str>) -> Entry {
        Entry {
            name: "evercore".into(),
            device_id: None,
            code: None,
            username: username.map(str::to_string),
            peer,
            saved: username.is_some(),
        }
    }

    #[test]
    fn an_ipv4_address_outranks_a_link_local_one_however_they_arrive() {
        let mut found = peer("a", PeerSource::Iroh, true);
        found.addresses = vec![
            "fe80::5781:23f9:85a9:9f47".parse().unwrap(),
            "2001:db8::1".parse().unwrap(),
            "192.168.1.50".parse().unwrap(),
        ];
        assert_eq!(entry(Some(found.clone()), None).address().as_deref(), Some("192.168.1.50"));
        found.addresses.remove(2);
        assert_eq!(entry(Some(found.clone()), None).address().as_deref(), Some("2001:db8::1"));
        found.addresses.remove(1);
        assert_eq!(
            entry(Some(found), None).address().as_deref(),
            Some("fe80::5781:23f9:85a9:9f47")
        );
    }

    #[test]
    fn the_line_under_a_name_is_the_address_and_the_system() {
        let line = subline(&entry(Some(peer("a", PeerSource::Iroh, true)), None));
        assert_eq!(line.as_deref(), Some("100.64.0.1 \u{b7} Linux"));
    }

    #[test]
    fn a_part_nothing_reported_is_left_out_rather_than_called_unknown() {
        let mut found = peer("a", PeerSource::Iroh, true);
        found.os = None;
        assert_eq!(subline(&entry(Some(found), None)).as_deref(), Some("100.64.0.1"));

        let mut nowhere = peer("a", PeerSource::Iroh, true);
        nowhere.os = None;
        nowhere.addresses.clear();
        let line = subline(&entry(Some(nowhere), Some("driver")));
        assert_eq!(line.as_deref(), Some("signed in as driver"));
        assert_eq!(subline(&entry(None, None)), None);
    }

    #[test]
    fn a_machine_that_cannot_be_reached_says_offline_whatever_it_last_used() {
        let gone = entry(Some(peer("a", PeerSource::Iroh, false)), None);
        assert_eq!(route_words(&gone), "Offline");
        assert_eq!(route_words(&entry(None, Some("driver"))), "Offline");
        let here = entry(Some(peer("a", PeerSource::Iroh, true)), None);
        assert_eq!(route_words(&here), "Internet");
    }

    #[test]
    fn systems_are_written_the_way_their_makers_write_them() {
        assert_eq!(system_name("windows"), "Windows");
        assert_eq!(system_name("MacOS"), "macOS");
        assert_eq!(system_name("darwin"), "macOS");
        assert_eq!(system_name("freebsd"), "freebsd");
    }

    // ------------------------------------------------------------- the merge

    /// Two distinct, real connect codes, the way pravera-core writes them.
    fn code(seed: u8) -> String {
        let mut key = [0u8; 32];
        for (index, byte) in key.iter_mut().enumerate() {
            *byte = (index as u8).wrapping_mul(37).wrapping_add(seed);
        }
        pravera_core::connect_code::grouped(&key)
    }

    fn lan_peer(name: &str, claims: &str) -> DiscoveredPeer {
        DiscoveredPeer {
            name: name.into(),
            addresses: vec![IpAddr::V4(Ipv4Addr::new(192, 168, 0, 9))],
            source: PeerSource::Lan {
                port: 41_337,
                claims: claims.into(),
                version: Some(2),
            },
            os: Some("windows".into()),
            online: true,
            device_id: None,
        }
    }

    fn remembered(code: &str, name: &str) -> Known {
        let mut known = Known::default();
        known.remember(code, name, "driver");
        known
    }

    #[test]
    fn a_machine_seen_two_ways_appears_once() {
        // Discovery found it on the subnet; a past session remembers it under
        // the name it calls itself. Two rows for one machine would make the
        // person reconcile the lists by eye, which is the list's job.
        let discovered = Discovered {
            lan: vec![lan_peer("evercore", "PRV-NOT-A-REAL-CODE")],
            ..Discovered::default()
        };
        let known = remembered(&code(1), "Evercore");

        let merged = entries(&discovered, &known);
        assert_eq!(merged.len(), 1);
        assert!(merged[0].saved);
        assert_eq!(merged[0].code, Some(code(1)), "the pin outranks the claim");
        assert_eq!(merged[0].username.as_deref(), Some("driver"));
    }

    #[test]
    fn a_remembered_machine_discovery_cannot_see_stays_listed() {
        // The machine is asleep, not gone. A list that only showed what is
        // reachable right now would hide exactly the machines somebody is
        // trying to wake up.
        let discovered = Discovered::default();
        let known = remembered(&code(1), "Evercore");

        let merged = entries(&discovered, &known);
        assert_eq!(merged.len(), 1);
        assert!(!merged[0].online());
        assert_eq!(merged[0].route_label(), "remembered");
        assert!(merged[0].saved);
    }

    #[test]
    fn a_discovered_machine_nobody_remembers_stands_on_its_own() {
        let discovered = Discovered {
            lan: vec![lan_peer("fresh-machine", "PRV-NOT-A-REAL-CODE")],
            ..Discovered::default()
        };

        let merged = entries(&discovered, &Known::default());
        assert_eq!(merged.len(), 1);
        assert!(!merged[0].saved);
        // The advertisement is all there is, so it is what a connect would
        // try — an offer, never a proof.
        assert_eq!(
            merged[0].code.as_deref(),
            Some("PRV-NOT-A-REAL-CODE")
        );
    }

    #[test]
    fn a_tailnet_name_and_a_machine_name_are_one_entry() {
        // The machine reports Evercore; the tailnet calls it
        // evercore.ts.net. Comparing whole strings would put the same
        // machine on the list twice under two spellings.
        let discovered = Discovered {
            lan: vec![peer("evercore.example-tailnet.ts.net", PeerSource::Iroh, true)],
            ..Discovered::default()
        };
        let known = remembered(&code(1), "Evercore");

        let merged = entries(&discovered, &known);
        assert_eq!(merged.len(), 1);
        assert!(merged[0].saved);
    }

    #[test]
    fn two_machines_are_two_entries_even_when_one_is_only_remembered() {
        let discovered = Discovered {
            lan: vec![peer("homelab", PeerSource::Iroh, true)],
            ..Discovered::default()
        };
        let known = remembered(&code(2), "workshop");

        let merged = entries(&discovered, &known);
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|entry| entry.name == "homelab"));
        assert!(merged.iter().any(|entry| entry.name == "workshop"));
    }
}
