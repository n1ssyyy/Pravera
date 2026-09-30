//! The screen a live session is watched and driven from.
//!
//! Two layers. Underneath, a shader widget showing the decoded picture at
//! whatever size the window is; on top, a toolbar that hides itself when it is
//! not wanted. Everything the person does with the mouse or keyboard is turned
//! into an [`InputEvent`] here and sent to the host over the control stream.
//!
//! ## Keys are positions, not letters
//!
//! A key press travels as the HID usage of the *position* that was pressed,
//! and the host applies its own keyboard layout to it. That is the only
//! arrangement that types the right character when the two machines disagree
//! about what the keyboard is: sending letters would apply the client's layout
//! and then the host's on top of it. [`crate::net::keys`] does the naming, and
//! explains why the character iced reports alongside each press is thrown
//! away rather than sent as well.
//!
//! ## Nothing may be left held down
//!
//! A key press without its release leaves the host with a stuck modifier, and
//! a stuck Ctrl makes every subsequent keystroke a shortcut. That happens
//! whenever capture stops while a key is down: releasing the chord, clicking
//! away, alt-tabbing out of the window. So every press is recorded and
//! [`State::release_everything`] undoes them all. It is called on every path
//! that stops forwarding, and there is a test per path.
//!
//! ## What this cannot do
//!
//! Ctrl+Alt+Delete is intercepted by Windows below any process and cannot be
//! forwarded from here; sending it needs the privileged service, which is P5.
//! Pointer motion is absolute, so a game that reads raw relative input will not
//! see the mouse move — that is the `SendInput` ceiling recorded in the plan,
//! and it is a host-side limit rather than anything this screen can fix.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iced::keyboard::key::Physical;
use iced::widget::{button, column, container, mouse_area, row, shader, stack, text, Space};
use iced::{mouse, Alignment, Color, Element, Event, Length, Rectangle};

use pravera_client::VideoStats;
use pravera_core::{QualityProfile, Resolution};
use pravera_proto::{InputEvent, KeyCode, MonitorId, PointerButton};

use crate::components;
use crate::icon;
use crate::motion;
use crate::net::keys;
use crate::net::link::{Command, Link};
use crate::theme::{self, tokens as t};
use crate::widget::remote_cursor::{self, Remote, Shape};
use crate::widget::video::{point_in, Picture, Video};

/// How long the toolbar stays up after something happens that the person
/// should see the result of.
const REVEAL_FOR: Duration = Duration::from_millis(2200);

/// The band at the top of the remote screen that summons the toolbar.
///
/// Expressed as a fraction of the picture rather than in pixels so it feels
/// the same on a 1080p host in a small window and a 4K host filling a monitor.
const REVEAL_BAND: f32 = 0.10;

/// Pixels of trackpad travel that count as one wheel detent.
///
/// Wheels report whole lines and trackpads report pixels; the protocol carries
/// detents, so the pixel figure has to be divided by something. Fifty is what
/// makes a trackpad flick move about as far as a wheel click, and the host
/// banks the remainder, so an imperfect divisor loses nothing — it only makes
/// slow scrolling take slightly more or less travel.
const PIXELS_PER_DETENT: f32 = 50.0;

/// Everything the person can do to a live session.
#[derive(Debug, Clone)]
pub enum Message {
    /// The pointer moved. `None` means it is somewhere that is part of the
    /// window but not part of the remote screen: a letterbox bar, the title
    /// bar, or a toolbar button that has claimed the cursor.
    ///
    /// `content` is the size of the picture's rectangle inside the window, in
    /// logical pixels, which is what raw pointer samples are mapped against.
    Pointer {
        at: Option<(f32, f32)>,
        content: iced::Size,
    },
    /// Pointer positions the low-level hook took since the last frame, in
    /// physical screen pixels. See [`crate::net::pointer`] for why these exist
    /// alongside the coalesced positions above.
    RawPointer(Vec<crate::net::pointer::Sample>),
    /// The window's scale factor, measured once when the session opens and
    /// again whenever the window is resized onto another monitor.
    Scale(f32),
    Button {
        button: PointerButton,
        pressed: bool,
    },
    Scroll {
        dx: f32,
        dy: f32,
    },
    Key {
        code: KeyCode,
        pressed: bool,
    },
    /// The pointer is resting on the toolbar, so it must stay up.
    OverTools(bool),
    /// The window lost focus. Everything held has to be let go.
    LostFocus,
    /// The release chord, or the toolbar button. Stops forwarding the keyboard.
    ToggleKeyboard,
    ToggleStats,
    /// Gaming mode: the window fills the screen and the pointer is confined
    /// to it. Asked for here, carried out by the application — a screen has
    /// no access to the window itself.
    ToggleGaming,
    /// Leave the picture for the file panes, without ending the session.
    Files,
    Profile(QualityProfile),
    Monitor(MonitorId),
    /// Ask the host to generate Ctrl+Alt+Del.
    SendSas,
    Disconnect,
}

/// What the screen keeps between redraws.
pub struct State {
    /// The newest decoded frame. `None` until the first one arrives, which is
    /// shown as itself rather than as a black rectangle.
    picture: Option<Picture>,
    /// The last measured control round-trip. Never a guess: `None` until one
    /// has actually completed.
    latency: Option<Duration>,
    /// When this session began, so the wait for the first frame can stop
    /// looking like progress once it has gone on too long.
    started: Option<Instant>,

    /// Whether key presses are being forwarded.
    keyboard: bool,
    /// Whether the measurement panel is open.
    stats: bool,
    /// Set when the file panes were asked for, and taken by the application.
    ///
    /// A flag rather than a return value because `update` answers in link
    /// commands, and "show a different screen" is not one — the same shape as
    /// `Link::take_keyframe_request`.
    leaving: bool,

    /// Drives the toolbar's fade and slide.
    overlay: iced::Animation<bool>,
    /// Drives the reminder that appears in the toolbar's place.
    hint: iced::Animation<bool>,
    /// The toolbar stays up until this moment whatever the pointer is doing.
    reveal_until: Option<Instant>,
    /// The pointer is inside the top band of the picture.
    near_top: bool,
    /// The pointer is on the toolbar itself.
    over_tools: bool,

    /// The tiles under the chosen quality profile and the chosen display, and
    /// whether they have been put under the first choice yet: a session that
    /// opens on Adaptive starts there rather than sliding to it.
    quality_thumb: motion::Thumb,
    display_thumb: motion::Thumb,
    thumbs_synced: bool,

    /// Keys and buttons currently down on the *host*, so they can all be let
    /// go at once. A press that is never released is a modifier stuck on
    /// someone else's machine.
    held_keys: Vec<KeyCode>,
    held_buttons: Vec<PointerButton>,

    /// The picture's rectangle inside the window, in logical pixels. Raw
    /// pointer samples are deltas in physical screen pixels; mapping one onto
    /// the other needs this and the scale factor below.
    content: iced::Size,
    /// Physical pixels per logical pixel on the window's current monitor.
    scale: f32,
    /// Where the remote pointer is while the raw-rate hook is driving it, as a
    /// fraction of the picture. Integrated from hook deltas rather than read
    /// from the window, because the window's own positions are coalesced.
    virtual_pointer: Option<(f32, f32)>,
    /// The last hook sample, so the next one becomes a delta.
    last_sample: Option<crate::net::pointer::Sample>,
    /// The last position iced reported, which seeds the integrated pointer
    /// when the hook starts and when the pointer re-enters the picture.
    last_seen: Option<(f32, f32)>,
    /// Gaming mode: the window fills the screen and the pointer is confined.
    gaming: bool,
    /// Set when gaming mode was toggled, and taken by the application.
    gaming_request: Option<bool>,

    /// The host's cursor as last reported, drawn over the picture. `None` for a
    /// host that sends none, and then nothing here changes anything.
    remote: Option<Remote>,
    /// The serial of `remote`, so an unchanged report is not rebuilt.
    remote_serial: u64,
    /// Cursor images built into drawable handles, by the host's id for them.
    /// Ids are never reused within a session, so an entry is never stale, and
    /// the whole cache goes with the session.
    shapes: HashMap<u32, Arc<Shape>>,
}

/// Cursor images kept before the cache is emptied. The host resends a shape
/// that falls out of it, so this is a memory bound and nothing more.
const SHAPE_CACHE: usize = 128;

impl Default for State {
    fn default() -> Self {
        State {
            picture: None,
            latency: None,
            started: None,
            keyboard: true,
            stats: false,
            leaving: false,
            overlay: motion::standard(true),
            hint: motion::standard(false),
            // A session opens with the toolbar up, because the first thing
            // anyone wants to know is which machine they are looking at.
            reveal_until: None,
            near_top: false,
            over_tools: false,
            quality_thumb: motion::Thumb::at(0),
            display_thumb: motion::Thumb::at(0),
            thumbs_synced: false,
            held_keys: Vec::new(),
            held_buttons: Vec::new(),
            content: iced::Size::ZERO,
            scale: 1.0,
            virtual_pointer: None,
            last_sample: None,
            last_seen: None,
            gaming: false,
            gaming_request: None,
            remote: None,
            remote_serial: 0,
            shapes: HashMap::new(),
        }
    }
}

impl State {
    /// Start a session, with the toolbar shown for long enough to read.
    pub fn new(_monitors: usize, now: Instant) -> State {
        State {
            started: Some(now),
            reveal_until: Some(now + REVEAL_FOR),
            ..State::default()
        }
    }

    /// Point the toolbar's tiles at the profile and the display the session is
    /// on now, as the link reports them. A change slides them; the first look
    /// puts them there.
    pub fn follow(&mut self, link: &Link, now: Instant) {
        let config = link.config();
        let profile = QualityProfile::ALL
            .iter()
            .position(|&candidate| candidate == config.profile)
            .unwrap_or(0);
        let display = link
            .monitors()
            .iter()
            .position(|monitor| monitor.id == config.monitor)
            .unwrap_or(0);
        self.choose(profile, display, now);
    }

    fn choose(&mut self, profile: usize, display: usize, now: Instant) {
        if self.thumbs_synced {
            self.quality_thumb.select(profile, now);
            self.display_thumb.select(display, now);
        } else {
            self.quality_thumb.snap(profile);
            self.display_thumb.snap(display);
            self.thumbs_synced = true;
        }
    }

    /// A newly decoded frame.
    pub fn show(&mut self, picture: Picture) {
        self.picture = Some(picture);
    }

    /// Read the host's cursor, once per redraw.
    ///
    /// The frame subscription runs for as long as a picture is in front, so a
    /// cursor that moves over a desktop that does not is picked up on the next
    /// frame without a message of its own.
    pub fn follow_cursor(&mut self, link: &Link) {
        let Some((serial, reported)) = link.remote_cursor() else {
            // Nothing sent, or it stopped: the viewer's own cursor takes over.
            self.remote = None;
            self.remote_serial = 0;
            return;
        };
        if self.remote.is_some() && serial == self.remote_serial {
            return;
        }
        self.remote_serial = serial;

        let shape = reported.image.as_ref().map(|image| {
            if let Some(known) = self.shapes.get(&image.id) {
                return known.clone();
            }
            if self.shapes.len() >= SHAPE_CACHE {
                self.shapes.clear();
            }
            let built = Arc::new(Shape {
                handle: iced::widget::image::Handle::from_rgba(
                    u32::from(image.width),
                    u32::from(image.height),
                    image.rgba.clone(),
                ),
                width: image.width,
                height: image.height,
                hot_x: image.hot_x,
                hot_y: image.hot_y,
            });
            self.shapes.insert(image.id, built.clone());
            built
        });
        self.remote = Some(Remote {
            x: reported.x,
            y: reported.y,
            visible: reported.visible,
            shape,
        });
    }

    /// Whether the viewer's own cursor is replaced by the host's over the
    /// picture.
    ///
    /// Only while controlling: the host's cursor is drawn where the hand is,
    /// so the two must not both show. Watching without control leaves the local
    /// pointer alone, because it is the viewer's own and there is no hand on
    /// the host to follow. Gaming mode always hides it, as it did before there
    /// was anything to replace it with.
    ///
    /// False whenever there is nothing to draw, which is the fallback: a host
    /// that hid its cursor, or one that has not sent one yet, leaves the
    /// viewer's arrow visible rather than nothing at all.
    fn replaces_local_cursor(&self, can_control: bool) -> bool {
        self.gaming || (can_control && self.remote.as_ref().is_some_and(Remote::is_drawable))
    }

    /// A control round-trip completed.
    pub fn measured(&mut self, rtt: Duration) {
        self.latency = Some(rtt);
    }

    /// The stream was reconfigured, so the monitor list may have a new length
    /// and the old picture is the wrong size.
    pub fn reconfigured(&mut self, now: Instant) {
        self.picture = None;
        self.reveal(now);
    }

    /// A frame elapsed.
    ///
    /// The toolbar's visibility depends on a deadline as well as on the
    /// pointer, and a deadline passing is not an event anything else would
    /// notice. This is where that is checked, once per redraw, which a live
    /// session has anyway.
    pub fn tick(&mut self, now: Instant) {
        let wanted = self.wanted(now);
        if self.overlay.value() != wanted {
            self.overlay.go_mut(wanted, now);
        }

        // The reminder is for when the controls are not up. Showing both at
        // once would be telling someone how to reach the thing they are
        // already looking at.
        let hinting = self.near_top && !wanted;
        if self.hint.value() != hinting {
            self.hint.go_mut(hinting, now);
        }
    }

    /// Put the toolbar up and start the clock on hiding it again.
    pub fn reveal(&mut self, now: Instant) {
        self.reveal_until = Some(now + REVEAL_FOR);
        self.overlay.go_mut(true, now);
    }

    /// Whether the toolbar should currently be up.
    ///
    /// It stays up unconditionally until a picture arrives. The toolbar hides
    /// to get out of the way of the remote screen, and with no remote screen
    /// there is nothing to get out of the way of — but every means of calling
    /// it back lives on the picture. Pointer position is reported by the video
    /// widget, and the release chord is handled inside it, so on a session that
    /// never shows anything both are absent: the toolbar would slide away after
    /// two seconds and leave no way to reach Disconnect, on exactly the screen
    /// where someone most wants it.
    fn wanted(&self, now: Instant) -> bool {
        self.picture.is_none()
            || self.over_tools
            || !self.keyboard
            || self.reveal_until.is_some_and(|until| now < until)
    }

    /// Whether the reminder of how to open the controls should be up.
    ///
    /// Only when the pointer has gone looking for them and they are not
    /// already there. Reaching for the top of the screen is what someone does
    /// when they want the controls, so it is the right moment to answer — but
    /// answering by opening the dock means it also opens every time the
    /// pointer merely passes through the top of the remote screen, which is
    /// where that machine's own menus and window buttons live.
    fn hinting(&self, now: Instant) -> f32 {
        if self.picture.is_none() {
            return 0.0;
        }
        self.hint.interpolate(0.0, 1.0, now) * (1.0 - self.showing(now))
    }

    /// How far through the toolbar's entrance we are, 0 to 1.
    fn showing(&self, now: Instant) -> f32 {
        self.overlay.interpolate(0.0, 1.0, now)
    }

    /// Whether key presses are being sent to the far machine.
    ///
    /// Drives the low-level hook as well as the dock's indicator: the two must
    /// agree, or the Windows key would be swallowed locally and forwarded
    /// nowhere.
    pub fn forwards_keyboard(&self) -> bool {
        self.keyboard
    }

    pub fn is_animating(&self, now: Instant) -> bool {
        // The reveal timer is not an animation, but it does change what is
        // drawn when it expires, and nothing else would wake the loop up to
        // notice. A session is redrawing for the video anyway.
        self.overlay.is_animating(now)
            || self.hint.is_animating(now)
            || self.quality_thumb.is_animating(now)
            || self.display_thumb.is_animating(now)
            || self.reveal_until.is_some_and(|until| now < until)
    }

    /// Let go of everything currently held on the host.
    ///
    /// Order matters only in that it must be complete: a release for a key
    /// that is not down is harmless, a missing release is a stuck key.
    fn release_everything(&mut self) -> Vec<Command> {
        let keys = self.held_keys.drain(..).map(|code| {
            Command::Input(InputEvent::Key {
                code,
                pressed: false,
            })
        });
        let buttons = self.held_buttons.drain(..).map(|button| {
            Command::Input(InputEvent::PointerButton {
                button,
                pressed: false,
            })
        });
        keys.chain(buttons).collect()
    }

    /// Whether the file panes were asked for. Answers `true` once.
    ///
    /// Taken rather than read, so the application acts on the request exactly
    /// once and returning to the picture does not immediately leave it again.
    pub fn take_leaving(&mut self) -> bool {
        std::mem::take(&mut self.leaving)
    }

    /// A change of gaming mode to carry out, if one was asked for. Answers
    /// once, the same way [`State::take_leaving`] does: the application owns
    /// the window, and acting twice would toggle it straight back.
    pub fn take_gaming(&mut self) -> Option<bool> {
        std::mem::take(&mut self.gaming_request)
    }

    /// Whether gaming mode is on right now: fullscreen, pointer confined.
    pub fn gaming_mode(&self) -> bool {
        self.gaming
    }

    /// Coming back to the picture. The next frame will be a keyframe, because
    /// the decoder has been sitting idle while another screen was up.
    pub fn resumed(&mut self, now: Instant) {
        self.leaving = false;
        self.reveal(now);
    }
}

/// Act on something the person did. The commands go to the host in order.
pub fn update(state: &mut State, message: Message, now: Instant) -> Vec<Command> {
    match message {
        Message::Pointer { at, content } => {
            state.near_top = at.is_some_and(|(_, y)| y <= REVEAL_BAND);
            // The geometry the raw samples are mapped against. Published with
            // every movement because the window can resize under the pointer;
            // a zero arrives only before the first layout.
            if content.width > 0.0 && content.height > 0.0 {
                state.content = content;
            }
            state.last_seen = at;
            if at.is_none() {
                // Over a letterbox bar or a toolbar button: the remote pointer
                // parks where it left rather than jumping to an edge. The next
                // sample over the picture re-seeds from where it re-entered.
                state.virtual_pointer = None;
                state.last_sample = None;
            }

            // While the raw-rate hook is running it is the source of motion:
            // forwarding these coalesced positions as well would move the
            // remote cursor twice as far. This event then only keeps the
            // toolbar logic and the geometry fresh.
            if crate::net::pointer::is_recording() {
                Vec::new()
            } else {
                at.map(|(x, y)| vec![Command::Input(InputEvent::PointerMoveAbsolute { x, y })])
                    .unwrap_or_default()
            }
        }

        Message::RawPointer(samples) => {
            let mut commands = Vec::new();
            for sample in samples {
                // The first sample after a gap is a position, not a movement.
                let Some(last) = state.last_sample.replace(sample) else {
                    continue;
                };
                // Only while the pointer is over the picture. Reaching for the
                // toolbar or across a letterbox bar must not slide the remote
                // cursor along under it.
                let Some(seen) = state.last_seen else {
                    continue;
                };

                let dx = (sample.x - last.x) as f32 / state.scale;
                let dy = (sample.y - last.y) as f32 / state.scale;
                if dx == 0.0 && dy == 0.0 {
                    // A sample that repeats the last position is the mouse at
                    // rest, and a packet saying so is a packet wasted.
                    continue;
                }
                let (vx, vy) = state.virtual_pointer.unwrap_or(seen);
                let x = (vx + dx / state.content.width).clamp(0.0, 1.0);
                let y = (vy + dy / state.content.height).clamp(0.0, 1.0);
                state.virtual_pointer = Some((x, y));
                commands.push(Command::Input(InputEvent::PointerMoveAbsolute { x, y }));
            }
            commands
        }

        Message::Scale(scale) => {
            if scale > 0.0 {
                state.scale = scale;
            }
            Vec::new()
        }

        Message::Button { button, pressed } => {
            if pressed {
                if !state.held_buttons.contains(&button) {
                    state.held_buttons.push(button);
                }
            } else if let Some(at) = state.held_buttons.iter().position(|&b| b == button) {
                state.held_buttons.remove(at);
            } else {
                // A release for a button that was pressed on the toolbar and
                // let go over the video. Forwarding it would put a phantom
                // mouse-up on the host.
                return Vec::new();
            }
            vec![Command::Input(InputEvent::PointerButton {
                button,
                pressed,
            })]
        }

        Message::Scroll { dx, dy } => vec![Command::Input(InputEvent::Scroll { dx, dy })],

        Message::Key { code, pressed } => {
            if pressed {
                if !state.held_keys.contains(&code) {
                    state.held_keys.push(code);
                }
            } else if let Some(at) = state.held_keys.iter().position(|&k| k == code) {
                state.held_keys.remove(at);
            } else {
                return Vec::new();
            }
            vec![Command::Input(InputEvent::Key { code, pressed })]
        }

        Message::OverTools(over) => {
            state.over_tools = over;
            if over {
                state.overlay.go_mut(true, now);
            }
            Vec::new()
        }

        // Alt-tabbing away with a key down is the ordinary way to leave a
        // modifier stuck on the far machine.
        Message::LostFocus => state.release_everything(),

        Message::ToggleKeyboard => {
            state.keyboard = !state.keyboard;
            state.reveal(now);
            // Turning capture off while keys are down is exactly the case that
            // strands them, because their releases will not be forwarded.
            if state.keyboard {
                Vec::new()
            } else {
                state.release_everything()
            }
        }

        Message::ToggleGaming => {
            state.gaming = !state.gaming;
            state.gaming_request = Some(state.gaming);
            state.reveal(now);
            Vec::new()
        }

        Message::ToggleStats => {
            state.stats = !state.stats;
            state.reveal(now);
            Vec::new()
        }

        // Everything held on the host is released first. Walking away from the
        // picture with a modifier still down leaves that machine holding Ctrl
        // with nobody there to let go of it.
        Message::Files => {
            state.leaving = true;
            state.reveal(now);
            state.release_everything()
        }

        Message::Profile(profile) => {
            state.reveal(now);
            vec![Command::SetProfile(profile)]
        }

        Message::Monitor(monitor) => {
            state.reveal(now);
            // The picture about to arrive is a different size, and the host
            // sends a keyframe with the new configuration.
            vec![Command::SelectMonitor(monitor)]
        }

        Message::SendSas => {
            state.reveal(now);
            vec![Command::SendSas]
        }

        // Let go of everything first: the host tears the session down, but a
        // host that stays up because only this session ended would otherwise
        // keep the keys down.
        Message::Disconnect => {
            let mut commands = state.release_everything();
            commands.push(Command::Disconnect);
            commands
        }
    }
}

/// The whole screen: picture underneath, toolbar over it.
pub fn view<'a>(state: &'a State, link: &'a Link, now: Instant) -> Element<'a, Message> {
    let showing = state.showing(now);

    let surface: Element<'_, Message> = match &state.picture {
        Some(picture) => {
            let control = link.can_control();
            let picture_widget: Element<'_, Message> = shader(Surface {
                video: Video::new(picture.clone()),
                resolution: picture.resolution,
                keyboard: state.keyboard,
                hide_cursor: state.replaces_local_cursor(control),
            })
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
            match &state.remote {
                // The host's cursor over the picture. A layer that takes no
                // events and reports no interaction, so everything below it
                // behaves exactly as it does without it.
                Some(remote) => stack![
                    picture_widget,
                    remote_cursor::layer(remote_cursor::Layer {
                        picture: picture.resolution,
                        remote: Some(remote.clone()),
                        follow_local: control && !state.gaming,
                    })
                ]
                .into(),
                None => picture_widget,
            }
        }
        None => waiting(state, link, now),
    };

    let base = container(surface)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_| container::Style {
            // Black rather than the app background: the letterbox bars around
            // a picture of a different shape read as the edge of the screen,
            // not as part of the interface.
            background: Some(iced::Background::Color(t::LETTERBOX)),
            ..Default::default()
        });

    let hinting = state.hinting(now);
    if showing <= 0.001 {
        return if hinting <= 0.001 {
            base.into()
        } else {
            stack![base, hint(hinting)].into()
        };
    }

    stack![base, overlay(state, link, now, showing)].into()
}

/// How long the first frame is allowed to take before the screen stops
/// describing the wait and starts describing the problem.
///
/// A host answers a session request by encoding whatever is on screen right
/// then, so the first frame is limited by one encode and one round trip. Six
/// seconds is far past that on any path Pravera can use, including a relayed
/// one. Anything still waiting here is not slow, it is stuck.
const FIRST_FRAME_PATIENCE: Duration = Duration::from_secs(6);

/// How wide the explanation under the headline is allowed to run.
///
/// It is centred on a screen that is otherwise empty, and a sentence set across
/// a full 1180 pixels is one the eye loses its place in.
const WAITING_WIDTH: f32 = 460.0;

/// Vertical padding inside the reminder strip.
///
/// Small enough that the strip stays a strip: the point of it is to occupy far
/// less of the picture than the dock it stands in for.
const HINT_PAD_Y: f32 = 5.0;

/// Shown until the first frame is decoded.
///
/// Says what is happening rather than showing a black rectangle, because a
/// black rectangle and a broken session look identical.
///
/// Past [`FIRST_FRAME_PATIENCE`] it says something more useful than "waiting",
/// and the distinction it draws is the one that actually narrows the fault:
/// whether *anything at all* has arrived. Nothing arriving means the host has
/// sent nothing — its capture is producing no frames, which is what a machine
/// with no monitor attached does. Datagrams arriving without a picture is the
/// opposite problem, in the decoder or in the loss between here and there.
/// Reporting "waiting" forever describes neither, and looks like progress.
fn waiting<'a>(state: &'a State, link: &'a Link, now: Instant) -> Element<'a, Message> {
    let waited = state
        .started
        .map(|started| now.saturating_duration_since(started))
        .unwrap_or_default();

    let (headline, detail) = waiting_words(link.host_name(), waited, &link.stats());

    container(
        column![
            text(headline)
                .size(t::TEXT_BASE)
                .font(t::FONT_UI_STRONG)
                .style(theme::heading),
            container(
                text(detail)
                    .size(t::TEXT_SM)
                    .style(theme::muted)
                    .align_x(iced::alignment::Horizontal::Center)
            )
            .max_width(WAITING_WIDTH),
        ]
        .spacing(t::SPACE_2)
        .align_x(Alignment::Center),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(Alignment::Center)
    .align_y(Alignment::Center)
    .into()
}

/// What the waiting screen says, given only what has actually been counted.
///
/// Separate from [`waiting`] so the wording can be tested: every sentence here
/// is a claim about the session, and a claim the app cannot support is worse
/// than no claim at all.
fn waiting_words(host: &str, waited: Duration, stats: &VideoStats) -> (String, String) {
    if waited < FIRST_FRAME_PATIENCE {
        return (
            "Connected".to_string(),
            format!("Waiting for the first frame from {host}"),
        );
    }

    // Nothing has arrived. Both explanations are offered because the client
    // genuinely cannot tell them apart from here: it sees the absence, not the
    // cause. Naming only the likely one would be a guess presented as a finding.
    // (A headless host refuses rather than streaming a test pattern, so a
    // session that connects but shows nothing points at the path first and
    // at "no display attached" second — the host log says which.)
    if stats.datagrams_received == 0 {
        return (
            format!("No video is arriving from {host}"),
            "The session is up and the control channel is answering, but not one \
             video packet has arrived. Either the host's encoder has produced \
             nothing yet, or nothing it sends is getting through on this path."
                .to_string(),
        );
    }

    // Sound arriving and picture not is a different fault from either of them
    // failing, and it is the one this check exists to catch: the two share a
    // path, so a session carrying only audio counts frames, fills the graphs
    // and looks alive from every angle except the one that matters.
    if stats.frames_offered == 0 && stats.frames_audio > 0 {
        return (
            format!("{host} is sending sound but no picture"),
            format!(
                "{} packets received and {} of them carried audio. Not one video frame has \
                 arrived, so there is nothing here to decode. The host's video pipeline \
                 is producing nothing; its log names the stage (capture, encoder, or sends).",
                stats.datagrams_received, stats.frames_audio
            ),
        );
    }

    let counts = format!(
        "{} packets received, {} frames assembled ({} video, {} audio), {} incomplete.",
        stats.datagrams_received,
        stats.frames_assembled,
        stats.frames_offered,
        stats.frames_audio,
        stats.frames_incomplete
    );

    // Three ways to arrive at a black window, and they are not the same
    // problem. Saying only "nothing has decoded" describes all three and
    // distinguishes none, which leaves the one person who could act on it with
    // nothing to act on.

    // The decoder was given frames and refused them. Its own words, because
    // this is the case where a specific thing is wrong and the decoder is the
    // only component that knows what.
    if stats.frames_failed > 0 {
        let reason = stats.decode_error.as_deref().unwrap_or("no reason given");
        return (
            "The video cannot be decoded".to_string(),
            format!(
                "{counts} {} frames were given to the decoder and refused: {reason}",
                stats.frames_failed
            ),
        );
    }

    // The decoder accepted them and had nothing to show yet. Ordinary for a
    // moment at the start; past the patience above it means the keyframe it
    // needs is not coming.
    if stats.frames_pending > 0 {
        return (
            "Waiting for a frame to start from".to_string(),
            format!(
                "{counts} The decoder has read {} frames and is still waiting for a keyframe to \
                 begin the picture from.",
                stats.frames_pending
            ),
        );
    }

    // Assembled, and never handed to the decoder at all. This is the queue
    // discarding them while it resynchronises, which happens when no frame is
    // marked as one the picture can start from — so the wait never ends on its
    // own. Worth naming precisely: it is a fault on the sending side, and
    // nothing on this machine will fix it.
    (
        "Video is arriving but never reaches the decoder".to_string(),
        format!(
            "{counts} Not one frame has been offered to the decoder, which means none of them is \
             marked as a frame the picture can start from. That is decided by the machine sending \
             the video."
        ),
    )
}

/// The floating toolbar, and the measurement panel when it is open.
fn overlay<'a>(
    state: &'a State,
    link: &'a Link,
    now: Instant,
    showing: f32,
) -> Element<'a, Message> {
    // Slides down as it fades in. Eight pixels: enough to read as arriving
    // from behind the title bar, small enough not to look like a drawer.
    let drop = t::SPACE_2 * (1.0 - showing);

    let mut pill = row![
        identity(link, showing),
        divider(showing),
        quality(state, now, showing),
    ]
    .spacing(t::SPACE_3)
    .align_y(Alignment::Center);

    if link.monitors().len() > 1 {
        pill = pill
            .push(divider(showing))
            .push(displays(state, link, now, showing));
    }

    let tools = row![
        tool(icon::KEYBOARD, state.keyboard, showing, Message::ToggleKeyboard),
        tool(icon::GAUGE, state.stats, showing, Message::ToggleStats),
        // Leaves the picture without ending the session. The session keeps
        // running in its own task while the file panes are on screen, which is
        // the whole reason files were kept off the control stream.
        tool(icon::TRANSFERS, false, showing, Message::Files),
        tool(icon::GAMEPAD, state.gaming, showing, Message::ToggleGaming),
        tool(icon::LOCK, false, showing, Message::SendSas),
        end_session(showing),
    ]
    .spacing(t::SPACE_1)
    .align_y(Alignment::Center);

    let bar = container(pill.push(divider(showing)).push(tools).padding(
        // Tighter vertically than horizontally, and tighter than it used to
        // be: the dock floats over someone else's screen, and every pixel of
        // height is a pixel of their desktop it covers. Five pixels of inset
        // around a fourteen-pixel mark keeps every target over the ~24 pixels
        // a pointer needs while reading as a strip rather than a panel.
        iced::Padding {
            top: 5.0,
            right: t::SPACE_2,
            bottom: 5.0,
            left: t::SPACE_4,
        },
    ))
    .style(move |_| container::Style {
        // Translucent enough to see the picture move behind it: this floats
        // over someone else's screen, so it should read as an object resting
        // on top rather than as a panel cut into it. The raised bevel's edge
        // and a soft shadow are what lift it off the picture.
        background: Some(iced::Background::Color(t::with_alpha(
            t::POPOVER,
            0.92 * showing,
        ))),
        border: iced::Border {
            color: t::with_alpha(t::BEVEL_RAISED.sides, showing),
            width: t::BORDER_WIDTH,
            radius: t::RADIUS_LG.into(),
        },
        shadow: iced::Shadow {
            color: t::with_alpha(t::SHADOW_INK, 0.45 * showing),
            ..theme::SHADOW_FLOAT
        },
        ..Default::default()
    });

    // An interaction of its own, so the stack stops here: without one the
    // toolbar said "nothing", and the picture under it hid the pointer and
    // took the moves meant for the buttons.
    let mut stacked = column![mouse_area(bar)
        .interaction(mouse::Interaction::Idle)
        .on_enter(Message::OverTools(true))
        .on_exit(Message::OverTools(false))]
    .spacing(t::SPACE_2)
    .align_x(Alignment::Center);

    if state.stats {
        stacked = stacked.push(measurements(state, link, showing));
    }

    container(column![
        Space::new().height(Length::Fixed(t::SPACE_3 + drop)),
        stacked,
    ])
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(Alignment::Center)
    .into()
}

/// The reminder that appears where the controls would be.
///
/// A strip rather than the dock itself, because the top edge of a remote screen
/// is somewhere the pointer goes for the *other* machine's menus and window
/// buttons, and a dock that unfurls over them every time is a dock in the way.
/// This says how to ask for the controls and then gets out of the way.
///
/// Translucent on purpose: it sits over live video, and the picture underneath
/// should still be readable through it. That is the one place in Pravera where
/// a see-through surface earns itself — everywhere else the background is a
/// known colour and glass would only be decoration.
fn hint<'a>(showing: f32) -> Element<'a, Message> {
    let keys = row![
        keycap("Ctrl", showing),
        keycap("Alt", showing),
        keycap("Shift", showing),
        keycap("P", showing),
    ]
    .spacing(t::SPACE_1)
    .align_y(Alignment::Center);

    let strip = container(
        row![
            keys,
            text("for controls")
                .size(t::TEXT_2XS)
                .style(theme::tinted(t::with_alpha(t::MUTED_FOREGROUND, showing))),
        ]
        .spacing(t::SPACE_2)
        .align_y(Alignment::Center),
    )
    .padding([HINT_PAD_Y, t::SPACE_3])
    .style(move |_| container::Style {
        // Well under the dock's own opacity. It is a label, not a surface, and
        // a strip this solid over moving video reads as damage to the picture.
        background: Some(iced::Background::Color(t::with_alpha(
            t::POPOVER,
            0.72 * showing,
        ))),
        border: iced::Border {
            color: t::with_alpha(t::BORDER, 0.9 * showing),
            width: t::BORDER_WIDTH,
            radius: t::RADIUS_FULL.into(),
        },
        ..Default::default()
    });

    container(column![
        Space::new().height(Length::Fixed(t::SPACE_3)),
        strip,
    ])
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(Alignment::Center)
    .into()
}

/// One key in the reminder, drawn as the key it is.
///
/// Set in mono and boxed, because a chord written as prose — "control alt shift
/// escape" — has to be parsed into keys before it can be pressed, and four
/// small boxes are already the shape of the thing.
fn keycap<'a>(label: &'static str, showing: f32) -> Element<'a, Message> {
    container(
        text(label)
            .size(t::TEXT_2XS)
            .font(t::FONT_MONO_STRONG)
            .style(theme::tinted(t::with_alpha(t::FOREGROUND, showing))),
    )
    .padding([2.0, t::SPACE_2])
    .style(move |_| container::Style {
        background: Some(iced::Background::Color(t::with_alpha(
            t::SECONDARY,
            0.8 * showing,
        ))),
        border: iced::Border {
            color: t::with_alpha(t::BORDER, showing),
            width: t::BORDER_WIDTH,
            radius: t::RADIUS_SM.into(),
        },
        ..Default::default()
    })
    .into()
}

/// Which machine this is, how the packets are getting there, and how long the
/// control stream takes to answer.
fn identity<'a>(link: &'a Link, showing: f32) -> Element<'a, Message> {
    let (tint, label) = match link.route() {
        Some(kind) => (route_tint(kind), route_label(kind)),
        None => (t::ROUTE_OFFLINE, "connecting"),
    };

    let mut facts = row![
        container(Space::new().width(6.0).height(6.0))
            .style(theme::badge(t::with_alpha(tint, showing))),
        text(link.host_name().to_string())
            .size(t::TEXT_SM)
            .font(t::FONT_UI_STRONG)
            .style(theme::tinted(t::with_alpha(t::FOREGROUND, showing))),
        text(label)
            .size(t::TEXT_XS)
            .font(t::FONT_MONO)
            .style(theme::tinted(t::with_alpha(t::MUTED_FOREGROUND, showing))),
    ]
    .spacing(t::SPACE_2)
    .align_y(Alignment::Center);

    // The same rule the badges below follow, applied to sound. Playing is a
    // quiet glyph; asked-for-and-absent is a badge, because that is a thing
    // the person is waiting for that will never arrive. Silence somebody
    // chose gets nothing at all — a dock that announces every absence is a
    // dock nobody reads.
    if link.is_playing_audio() {
        facts = facts.push(icon::stroked(
            icon::SPEAKER,
            13.0,
            t::with_alpha(t::SUBTLE_FOREGROUND, showing),
        ));
    } else if link.is_missing_audio() {
        facts = facts.push(
            container(
                text("NO SOUND")
                    .size(t::TEXT_2XS)
                    .font(t::FONT_MONO_STRONG)
                    .style(theme::tinted(t::with_alpha(t::WARNING, showing))),
            )
            .padding([1.0, t::SPACE_1])
            .style(theme::badge(t::with_alpha(t::WARNING, 0.18 * showing))),
        );
    }

    // Stated only where it is true, and stated plainly. A viewer whose clicks
    // vanish with no explanation would reasonably conclude the app is broken.
    if !link.can_control() {
        facts = facts.push(
            container(
                text("VIEW ONLY")
                    .size(t::TEXT_2XS)
                    .font(t::FONT_MONO_STRONG)
                    .style(theme::tinted(t::with_alpha(t::WARNING, showing))),
            )
            .padding([1.0, t::SPACE_1])
            .style(theme::badge(t::with_alpha(t::WARNING, 0.18 * showing))),
        );
    }

    facts.into()
}

/// The three quality profiles as one segmented control.
fn quality<'a>(state: &'a State, now: Instant, showing: f32) -> Element<'a, Message> {
    segments(
        QualityProfile::ALL
            .iter()
            .map(|&profile| (profile_label(profile), Message::Profile(profile)))
            .collect(),
        &state.quality_thumb,
        now,
        showing,
    )
}

/// The displays, when there is more than one and this login may see them.
fn displays<'a>(state: &'a State, link: &'a Link, now: Instant, showing: f32) -> Element<'a, Message> {
    segments(
        link.monitors()
            .iter()
            .enumerate()
            .map(|(index, monitor)| {
                (
                    // The host's own name for the display, shortened to what
                    // fits a segment. Numbering them here would invent a
                    // numbering the host does not use.
                    short_name(&monitor.name, index),
                    Message::Monitor(monitor.id),
                )
            })
            .collect(),
        &state.display_thumb,
        now,
        showing,
    )
}

/// The gap between one cell of a segmented control and the next.
const SEGMENT_GAP: f32 = 1.0;

/// JetBrains Mono's advance, in ems. Every weight has the same one, so a
/// label's width is its length in characters however heavy it is set, which
/// is what lets a control be laid out without measuring any text.
const MONO_ADVANCE: f32 = 0.6;

/// How wide every cell of a control is when its longest label is `longest`
/// characters: the label and the room either side of it. One width for all,
/// so the tile that slides under the chosen one is a fixed size.
fn cell_width(longest: usize) -> f32 {
    longest as f32 * MONO_ADVANCE * t::TEXT_XS + 2.0 * t::SPACE_3
}

/// How far in from the left of the control the tile is when it is
/// `position` cells along: on a cell exactly at a whole number.
fn tile_x(position: f32, width: f32) -> f32 {
    position * (width + SEGMENT_GAP)
}

/// One choice of a few, as one control: cells all of one width, and a tile that
/// slides under the one chosen.
fn segments<'a>(
    choices: Vec<(String, Message)>,
    thumb: &motion::Thumb,
    now: Instant,
    showing: f32,
) -> Element<'a, Message> {
    let width = cell_width(choices.iter().map(|(label, _)| label.chars().count()).max().unwrap_or(0));

    let mut cells = row![].spacing(SEGMENT_GAP);
    for (index, (label, message)) in choices.into_iter().enumerate() {
        // How much of the tile is under this cell: its words light with it,
        // and it stops lifting for the pointer once the tile is there.
        let held = thumb.amount(index, now);
        let chosen = thumb.chosen() == index;
        cells = cells.push(components::glide(move |hover| {
            button(
                container(text(label).size(t::TEXT_XS).wrapping(text::Wrapping::None).font(if chosen {
                    t::FONT_UI_STRONG
                } else {
                    t::FONT_UI
                }))
                .center_x(Length::Fill),
            )
            .width(Length::Fixed(width))
            .padding([t::SPACE_1, 0.0])
            .style(move |_, status| {
                let lift = hover.get() * (1.0 - held);
                let background = match status {
                    button::Status::Pressed => t::ACCENT,
                    _ => t::with_alpha(t::SECONDARY, 0.75 * lift),
                };
                let ink = theme::blend(theme::blend(t::MUTED_FOREGROUND, t::FOREGROUND, hover.get()), t::FOREGROUND, held);
                button::Style {
                    background: Some(iced::Background::Color(t::with_alpha(background, background.a * showing))),
                    text_color: t::with_alpha(ink, showing),
                    border: iced::Border {
                        radius: t::RADIUS_SM.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }
            })
            .on_press(message)
            .into()
        }));
    }

    let tile = container(Space::new().width(Length::Fixed(width)).height(Length::Fill)).style(move |_| {
        container::Style {
            background: Some(iced::Background::Color(t::with_alpha(t::SECONDARY, showing))),
            border: iced::Border {
                radius: t::RADIUS_SM.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    });
    let under = row![Space::new().width(Length::Fixed(tile_x(thumb.position(now), width))), tile].height(Length::Fill);

    iced::widget::Stack::with_children([Element::from(cells)])
        .push_under(under)
        .into()
}

/// A square icon button that can be on or off.
fn tool<'a>(glyph: &'static str, active: bool, showing: f32, message: Message) -> Element<'a, Message> {
    let tint = if active { t::FOREGROUND } else { t::SUBTLE_FOREGROUND };
    components::glide(move |hover| {
        button(icon::stroked(glyph, 14.0, t::with_alpha(tint, showing)))
            .padding([5.0, t::SPACE_2])
            .style(move |theme, status| {
                let mut style = theme::glided(theme::ghost_button, hover.get(), theme, status);
                if active && !matches!(status, button::Status::Pressed) {
                    style.background = Some(iced::Background::Color(t::SECONDARY));
                }
                style.border.radius = t::RADIUS_SM.into();
                theme::fade_button(style, showing)
            })
            .on_press(message)
            .into()
    })
}

/// Ending the session is the one destructive control here, and it is the only
/// one that carries a word as well as a mark.
fn end_session<'a>(showing: f32) -> Element<'a, Message> {
    components::glide(move |hover| {
        button(
            row![
                icon::stroked(icon::DISCONNECT, 14.0, t::with_alpha(t::DESTRUCTIVE_TEXT, showing)),
                text("End").size(t::TEXT_XS),
            ]
            .spacing(t::SPACE_1)
            .align_y(Alignment::Center),
        )
        .padding([t::SPACE_2, t::SPACE_2])
        .style(move |theme, status| {
            let mut style = theme::glided(theme::danger_ghost_button, hover.get(), theme, status);
            style.border.radius = t::RADIUS_SM.into();
            theme::fade_button(style, showing)
        })
        .on_press(Message::Disconnect)
        .into()
    })
}

/// The measurement panel.
///
/// Every figure here was measured. The round-trip is the control stream's, and
/// it is labelled as that rather than as "latency", because the time a pixel
/// takes to arrive is a different and larger number that nothing has measured
/// yet.
fn measurements<'a>(state: &'a State, link: &'a Link, showing: f32) -> Element<'a, Message> {
    let stats = link.stats();
    let config = link.config();
    let host = link.host_name();

    let rtt = match state.latency {
        Some(rtt) => format!("{:.1} ms", rtt.as_secs_f32() * 1000.0),
        None => "measuring".to_string(),
    };

    let lost = stats
        .frames_assembled
        .saturating_add(stats.frames_incomplete);
    let loss = if lost == 0 {
        "0.0%".to_string()
    } else {
        format!(
            "{:.1}%",
            stats.frames_incomplete as f32 / lost as f32 * 100.0
        )
    };

    let mut rows = vec![
        ("control round trip", rtt),
        (
            "picture",
            format!(
                "{}x{}",
                config.format.resolution.width, config.format.resolution.height
            ),
        ),
        // Not the enum name. `OpenH264` looks like a codec choice somebody made
        // rather than what it is — the software fallback, running several times
        // slower than the GPU path — and where the encoding happens is the far
        // machine, which is the part nobody guesses.
        (
            "encoder",
            format!(
                "{} on {}",
                if config.format.codec.is_hardware() {
                    "GPU"
                } else {
                    "CPU"
                },
                host
            ),
        ),
        ("frames decoded", stats.frames_decoded.to_string()),
        (
            "frames incomplete",
            format!("{} ({loss})", stats.frames_incomplete),
        ),
        // The two that tell a frozen picture apart from a slow one. Frames
        // failing means the decoder lost the thread it was working from and
        // every frame since is unusable; overruns mean this machine cannot
        // decode as fast as the host encodes. Both look identical on screen —
        // a picture that stops moving — and they call for opposite fixes.
        ("frames failed", stats.frames_failed.to_string()),
        ("decoder overruns", stats.decoder_overruns.to_string()),
        ("datagrams", stats.datagrams_received.to_string()),
        // Waiting for the first decodable frame while others arrive: the
        // keyframe the picture must start from has not come yet.
        ("frames pending", stats.frames_pending.to_string()),
        // Newest-wins: a fast host and a slow window drop pictures here
        // rather than queueing stale ones.
        ("frames superseded", stats.frames_superseded.to_string()),
    ];
    // The decoder's own words for the last refusal, truncated to a row.
    // The count above says how many; this says what. Cleared implicitly —
    // a working decoder overwrites it with the next failure, and none means
    // the last failure is old news, not current state.
    if let Some(reason) = stats.decode_error.as_deref() {
        let short: String = reason.chars().take(72).collect();
        rows.push(("last decode error", short));
    }

    // Sound is three states, not two, and the middle one is the whole reason
    // this row exists: the host agreed to send it and this machine turned out
    // to have nowhere to play it. Without the row that reads as the host
    // refusing, and the person goes looking on the wrong machine.
    match (config.audio, link.audio_stats()) {
        (Some(format), Some(sound)) => {
            rows.push(("sound", format!("{} from {host}", format.codec.name())));
            // What playback is holding, which is this machine's share of the
            // delay between the two. Measured, not the figure it aims for.
            rows.push(("sound buffered", format!("{} ms", sound.buffered_millis)));
            // The audible counterpart of an incomplete frame: five
            // milliseconds of silence where a lost packet should have been.
            rows.push(("sound concealed", sound.packets_concealed.to_string()));
        }
        (Some(_), None) => rows.push(("sound", "no output on this machine".to_string())),
        // Why the host sent none is deliberately not knowable here: it refuses
        // without saying whether the reason was this login's permissions or its
        // own hardware. So the row says what happened, not why.
        (None, _) if link.is_missing_audio() => {
            rows.push(("sound", format!("{host} sent none")));
        }
        (None, _) => rows.push(("sound", "off".to_string())),
    }

    let body = rows
        .iter()
        .fold(column![].spacing(t::SPACE_1), |list, (label, value)| {
            list.push(
                row![
                    text(*label)
                        .size(t::TEXT_XS)
                        .style(theme::tinted(t::with_alpha(t::MUTED_FOREGROUND, showing)))
                        .width(Length::Fixed(132.0)),
                    text(value.clone())
                        .size(t::TEXT_XS)
                        .font(t::FONT_MONO)
                        .style(theme::tinted(t::with_alpha(t::NEUTRAL_200, showing))),
                ]
                .spacing(t::SPACE_3),
            )
        });

    container(body)
        .padding(t::SPACE_3)
        .style(move |_| container::Style {
            background: Some(iced::Background::Color(t::with_alpha(
                t::POPOVER,
                0.94 * showing,
            ))),
            border: iced::Border {
                color: t::with_alpha(t::BORDER, showing),
                width: t::BORDER_WIDTH,
                radius: t::RADIUS.into(),
            },
            ..Default::default()
        })
        .into()
}

fn divider<'a>(showing: f32) -> Element<'a, Message> {
    container(Space::new().width(1.0).height(Length::Fixed(14.0)))
        .style(move |_| container::Style {
            background: Some(iced::Background::Color(t::with_alpha(t::BORDER, showing))),
            ..Default::default()
        })
        .into()
}

// ------------------------------------------------------------------- surface

/// The shader program under everything: draws the picture and turns what the
/// person does over it into messages.
struct Surface {
    video: Video,
    resolution: Resolution,
    /// Whether key presses are being forwarded. Pointer events are always
    /// forwarded; the keyboard is the one that has to be lent back so the
    /// person can use their own machine.
    keyboard: bool,
    /// Whether the local pointer disappears over the picture: in gaming mode,
    /// where it is confined and the game draws its own, and while the host's
    /// cursor is being drawn in its place (`State::replaces_local_cursor`).
    hide_cursor: bool,
}

impl shader::Program<Message> for Surface {
    type State = ();
    type Primitive = <Video as shader::Program<Message>>::Primitive;

    fn update(
        &self,
        _state: &mut (),
        event: &Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<shader::Action<Message>> {
        use iced::keyboard::Event as Key;
        use iced::window::Event as Window;
        use mouse::Event as Mouse;

        match event {
            Event::Mouse(Mouse::CursorMoved { .. }) => {
                // Read the position from the cursor rather than the event: a
                // cursor that has levitated onto the toolbar reports nothing,
                // which is exactly the answer wanted there.
                let at = cursor.position();
                // The content rectangle travels with every movement, because
                // it is what raw pointer deltas are divided against and the
                // window can change size under the pointer.
                let content = crate::widget::video::fit(bounds, self.resolution).size();
                Some(shader::Action::publish(Message::Pointer {
                    at: at.and_then(|p| point_in(bounds, self.resolution, p)),
                    content,
                }))
            }

            Event::Mouse(Mouse::ButtonPressed(button)) => {
                // A press with no position is a press on something else.
                cursor
                    .position()
                    .and_then(|p| point_in(bounds, self.resolution, p))?;
                let button = to_button(*button)?;
                Some(
                    shader::Action::publish(Message::Button {
                        button,
                        pressed: true,
                    })
                    .and_capture(),
                )
            }

            // Released wherever it happens: a button pressed over the picture
            // and let go elsewhere still has to come back up on the host, and
            // `update` drops the ones that were never pressed.
            Event::Mouse(Mouse::ButtonReleased(button)) => {
                let button = to_button(*button)?;
                Some(shader::Action::publish(Message::Button {
                    button,
                    pressed: false,
                }))
            }

            Event::Mouse(Mouse::WheelScrolled { delta }) => {
                cursor.position_over(bounds)?;
                let (dx, dy) = match delta {
                    mouse::ScrollDelta::Lines { x, y } => (*x, *y),
                    mouse::ScrollDelta::Pixels { x, y } => {
                        (x / PIXELS_PER_DETENT, y / PIXELS_PER_DETENT)
                    }
                };
                if dx == 0.0 && dy == 0.0 {
                    return None;
                }
                Some(shader::Action::publish(Message::Scroll { dx, dy }).and_capture())
            }

            Event::Keyboard(Key::KeyPressed {
                physical_key,
                modifiers,
                ..
            }) => {
                // Tested before the forwarding check, so the chord closes the
                // controls as well as opening them. Behind that check it was a
                // one-way door: it stopped forwarding, and stopping forwarding
                // is what made the chord itself stop being read.
                if is_release_chord(*physical_key, *modifiers) {
                    return Some(shader::Action::publish(Message::ToggleKeyboard).and_capture());
                }
                if is_gaming_chord(*physical_key, *modifiers) {
                    return Some(shader::Action::publish(Message::ToggleGaming).and_capture());
                }
                if !self.keyboard {
                    return None;
                }

                // The text this press produced under the *local* layout is
                // deliberately ignored: see `net::keys` for why forwarding it
                // types every accented character twice.
                let action = if let Some(code) = keys::usage(*physical_key) {
                    shader::Action::publish(Message::Key {
                        code,
                        pressed: true,
                    })
                } else {
                    // Nothing to send, but the key was aimed at the other
                    // machine. Letting it fall through would fire whatever
                    // local shortcut it happens to match.
                    shader::Action::capture()
                };
                Some(action.and_capture())
            }

            Event::Keyboard(Key::KeyReleased { physical_key, .. }) => {
                if !self.keyboard {
                    return None;
                }
                let code = keys::usage(*physical_key)?;
                Some(
                    shader::Action::publish(Message::Key {
                        code,
                        pressed: false,
                    })
                    .and_capture(),
                )
            }

            Event::Window(Window::Unfocused) => Some(shader::Action::publish(Message::LostFocus)),

            _ => None,
        }
    }

    fn draw(&self, _state: &(), cursor: mouse::Cursor, bounds: Rectangle) -> Self::Primitive {
        shader::Program::<Message>::draw(&self.video, &(), cursor, bounds)
    }

    fn mouse_interaction(
        &self,
        _state: &(),
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        // Hiding the local arrow is only safe when something else is drawing
        // the pointer. A host older than protocol version 3 composites its
        // cursor into the picture only when Windows is drawing one (not on a
        // headless box with no mouse, not in a view-only session, not over an
        // elevated window), and hiding the arrow on the strength of a cursor
        // that may not be there left people with no pointer at all. So this is
        // set only in gaming mode, and once the host has sent a cursor the
        // viewer is actually drawing.
        match cursor
            .position()
            .and_then(|p| point_in(bounds, self.resolution, p))
        {
            Some(_) if self.hide_cursor => mouse::Interaction::Hidden,
            Some(_) => mouse::Interaction::Idle,
            // Over a letterbox bar: this is the app's own surface again.
            None => mouse::Interaction::None,
        }
    }
}

/// The chord that opens and closes the controls.
///
/// `P` with all three of Ctrl, Alt and Shift held. It is the only key press
/// this screen refuses to forward.
///
/// Not Escape. Windows reserves `Ctrl+Shift+Esc` for Task Manager and
/// `Ctrl+Alt+Del` for the secure attention sequence, and both are claimed below
/// any application — a chord built around Escape is one the local machine may
/// answer instead of Pravera, which is the worst possible failure for the key
/// somebody presses when they want out.
///
/// Not `Ctrl+Alt+Home` either, which is what Remote Desktop uses for the same
/// job. That is the better precedent and the wrong key for the hardware: a lot
/// of laptop keyboards have no Home key at all, only `Fn` and an arrow, and the
/// escape hatch cannot depend on a key the machine might not have. Every
/// keyboard has a `P`.
fn is_release_chord(physical: Physical, modifiers: iced::keyboard::Modifiers) -> bool {
    physical == Physical::Code(iced::keyboard::key::Code::KeyP)
        && modifiers.control()
        && modifiers.alt()
        && modifiers.shift()
}

/// The chord that toggles gaming mode: `G` with Ctrl, Alt and Shift held.
///
/// The same reasoning as [`is_release_chord`] applies — it must be a key every
/// keyboard has, in a combination nothing below this program claims. It has to
/// work whether or not the keyboard is being forwarded, because gaming mode is
/// exactly the situation where the toolbar is buried under a fullscreen game
/// and the pointer cannot leave the window to reach anything else.
fn is_gaming_chord(physical: Physical, modifiers: iced::keyboard::Modifiers) -> bool {
    physical == Physical::Code(iced::keyboard::key::Code::KeyG)
        && modifiers.control()
        && modifiers.alt()
        && modifiers.shift()
}

fn to_button(button: mouse::Button) -> Option<PointerButton> {
    Some(match button {
        mouse::Button::Left => PointerButton::Left,
        mouse::Button::Right => PointerButton::Right,
        mouse::Button::Middle => PointerButton::Middle,
        mouse::Button::Back => PointerButton::Back,
        mouse::Button::Forward => PointerButton::Forward,
        // A button the protocol has no name for. Guessing which one it is
        // would press the wrong one.
        mouse::Button::Other(_) => return None,
    })
}

// ------------------------------------------------------------------ labelling

fn profile_label(profile: QualityProfile) -> String {
    match profile {
        QualityProfile::Quality => "Quality",
        QualityProfile::Adaptive => "Adaptive",
        QualityProfile::Latency => "Latency",
    }
    .to_string()
}

/// A display name short enough for a segment.
///
/// Host display names run to things like `\\.\DISPLAY1` and `Generic PnP
/// Monitor`, neither of which fits. The tail of the name is the part that
/// differs between two displays on one machine, so that is what is kept.
fn short_name(name: &str, index: usize) -> String {
    let trimmed = name.trim_matches(|c: char| !c.is_alphanumeric());
    if trimmed.is_empty() {
        return format!("Display {}", index + 1);
    }
    if trimmed.chars().count() <= 12 {
        return trimmed.to_string();
    }
    let tail: String = trimmed
        .chars()
        .skip(trimmed.chars().count().saturating_sub(11))
        .collect();
    format!("…{tail}")
}

fn route_tint(kind: pravera_transport::RouteKind) -> Color {
    use pravera_transport::RouteKind;
    match kind {
        RouteKind::Direct => t::ROUTE_DIRECT,
        RouteKind::Relay => t::ROUTE_RELAY,
    }
}

fn route_label(kind: pravera_transport::RouteKind) -> &'static str {
    use pravera_transport::RouteKind;
    match kind {
        RouteKind::Direct => "direct",
        RouteKind::Relay => "relayed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: u64) -> Instant {
        Instant::now() + Duration::from_secs(seconds)
    }

    fn inputs(commands: &[Command]) -> Vec<InputEvent> {
        commands
            .iter()
            .filter_map(|c| match c {
                Command::Input(event) => Some(event.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_pointer_position_travels_as_the_fraction_it_was_given() {
        let mut state = State::default();
        let now = Instant::now();

        let commands = update(
            &mut state,
            Message::Pointer {
                at: Some((0.25, 0.75)),
                content: iced::Size::new(800.0, 450.0),
            },
            now,
        );
        assert_eq!(
            inputs(&commands),
            vec![InputEvent::PointerMoveAbsolute { x: 0.25, y: 0.75 }]
        );
    }

    #[test]
    fn raw_samples_move_the_pointer_by_the_distance_they_report() {
        // The whole reason the hook exists: a fast hand produces many small
        // movements a second, and each one must reach the far machine rather
        // than being merged into one jump per redraw.
        let mut state = State::default();
        let now = Instant::now();
        let content = iced::Size::new(800.0, 450.0);

        update(
            &mut state,
            Message::Pointer {
                at: Some((0.5, 0.5)),
                content,
            },
            now,
        );

        // The first sample after a gap only establishes where the pointer
        // was; the second is the first movement.
        let commands = update(
            &mut state,
            Message::RawPointer(vec![
                crate::net::pointer::Sample { x: 1000, y: 500 },
                crate::net::pointer::Sample { x: 1100, y: 455 },
            ]),
            now,
        );

        // One hundred physical pixels across an eight-hundred-pixel picture
        // is an eighth of the screen; forty-five down a four-hundred-and-
        // fifty-pixel one is a tenth.
        assert_eq!(
            inputs(&commands),
            vec![InputEvent::PointerMoveAbsolute {
                x: 0.625,
                y: 0.4,
            }]
        );
    }

    #[test]
    fn raw_samples_do_nothing_until_the_pointer_has_been_placed() {
        // Without a position from the window there is nothing to integrate
        // against, and inventing one would put the remote cursor somewhere
        // the person never pointed.
        let mut state = State::default();

        let commands = update(
            &mut state,
            Message::RawPointer(vec![
                crate::net::pointer::Sample { x: 10, y: 10 },
                crate::net::pointer::Sample { x: 20, y: 20 },
                crate::net::pointer::Sample { x: 30, y: 30 },
            ]),
            Instant::now(),
        );

        assert!(inputs(&commands).is_empty());
    }

    #[test]
    fn reaching_for_the_toolbar_parks_the_remote_pointer() {
        // Movements over the window's own chrome belong to this machine.
        // Forwarding them would slide the remote cursor along under the
        // toolbar, and it would have drifted by the time the pointer came
        // back.
        let mut state = State::default();
        let now = Instant::now();

        update(
            &mut state,
            Message::Pointer {
                at: Some((0.5, 0.5)),
                content: iced::Size::new(800.0, 450.0),
            },
            now,
        );
        update(
            &mut state,
            Message::Pointer {
                at: None,
                content: iced::Size::new(800.0, 450.0),
            },
            now,
        );
        let commands = update(
            &mut state,
            Message::RawPointer(vec![
                crate::net::pointer::Sample { x: 100, y: 100 },
                crate::net::pointer::Sample { x: 140, y: 100 },
            ]),
            now,
        );

        assert!(inputs(&commands).is_empty());

        // And coming back to the picture resumes from where it re-entered,
        // not from where it parked.
        update(
            &mut state,
            Message::Pointer {
                at: Some((0.25, 0.25)),
                content: iced::Size::new(800.0, 450.0),
            },
            now,
        );
        let commands = update(
            &mut state,
            Message::RawPointer(vec![
                crate::net::pointer::Sample { x: 140, y: 100 },
                crate::net::pointer::Sample { x: 180, y: 100 },
            ]),
            now,
        );
        assert_eq!(
            inputs(&commands),
            vec![InputEvent::PointerMoveAbsolute { x: 0.3, y: 0.25 }]
        );
    }

    #[test]
    fn asking_for_gaming_mode_asks_exactly_once() {
        let mut state = State::default();
        let now = Instant::now();

        update(&mut state, Message::ToggleGaming, now);
        assert_eq!(state.take_gaming(), Some(true));
        assert!(state.take_gaming().is_none(), "the request was repeated");
        assert!(state.gaming_mode());

        update(&mut state, Message::ToggleGaming, now);
        assert_eq!(state.take_gaming(), Some(false));
        assert!(!state.gaming_mode());
    }

    #[test]
    fn the_scale_factor_divides_raw_movement() {
        // A window at one hundred and fifty percent receives one hundred and
        // fifty physical pixels for every hundred logical ones. Dividing by
        // the wrong thing scales every movement by half again.
        let mut state = State::default();
        let now = Instant::now();
        let content = iced::Size::new(800.0, 450.0);

        update(&mut state, Message::Scale(1.5), now);
        update(
            &mut state,
            Message::Pointer {
                at: Some((0.5, 0.5)),
                content,
            },
            now,
        );
        let commands = update(
            &mut state,
            Message::RawPointer(vec![
                crate::net::pointer::Sample { x: 0, y: 0 },
                crate::net::pointer::Sample { x: 150, y: 0 },
            ]),
            now,
        );

        // One hundred and fifty physical pixels is one hundred logical, which
        // is an eighth of the eight-hundred-pixel picture.
        assert_eq!(
            inputs(&commands),
            vec![InputEvent::PointerMoveAbsolute { x: 0.625, y: 0.5 }]
        );
    }

    #[test]
    fn every_key_still_down_is_let_go_when_the_keyboard_is_handed_back() {
        // The bug this prevents: release the chord while holding Ctrl and
        // Shift, and the host keeps them down forever. Every keystroke after
        // that is a shortcut on someone else's machine.
        let mut state = State::default();
        let now = Instant::now();

        for code in [KeyCode(0xE0), KeyCode(0xE1), KeyCode(0x04)] {
            update(
                &mut state,
                Message::Key {
                    code,
                    pressed: true,
                },
                now,
            );
        }

        let commands = update(&mut state, Message::ToggleKeyboard, now);
        let released: Vec<KeyCode> = inputs(&commands)
            .into_iter()
            .filter_map(|event| match event {
                InputEvent::Key {
                    code,
                    pressed: false,
                } => Some(code),
                _ => None,
            })
            .collect();

        assert_eq!(released.len(), 3, "{released:?}");
        for code in [KeyCode(0xE0), KeyCode(0xE1), KeyCode(0x04)] {
            assert!(released.contains(&code), "{code:?} was left held");
        }
    }

    #[test]
    fn losing_focus_lets_go_of_everything() {
        // Alt-tabbing away is the ordinary way to strand a modifier.
        let mut state = State::default();
        let now = Instant::now();

        update(
            &mut state,
            Message::Key {
                code: KeyCode(0xE2),
                pressed: true,
            },
            now,
        );
        update(
            &mut state,
            Message::Button {
                button: PointerButton::Left,
                pressed: true,
            },
            now,
        );

        let commands = update(&mut state, Message::LostFocus, now);
        let events = inputs(&commands);
        assert!(events.contains(&InputEvent::Key {
            code: KeyCode(0xE2),
            pressed: false
        }));
        assert!(events.contains(&InputEvent::PointerButton {
            button: PointerButton::Left,
            pressed: false
        }));
    }

    #[test]
    fn disconnecting_lets_go_before_it_leaves() {
        let mut state = State::default();
        let now = Instant::now();
        update(
            &mut state,
            Message::Key {
                code: KeyCode(0xE0),
                pressed: true,
            },
            now,
        );

        let commands = update(&mut state, Message::Disconnect, now);
        assert!(matches!(commands.last(), Some(Command::Disconnect)));
        assert_eq!(
            inputs(&commands),
            vec![InputEvent::Key {
                code: KeyCode(0xE0),
                pressed: false
            }],
            "the goodbye went out before the keys came up"
        );
    }

    #[test]
    fn a_key_held_down_is_recorded_once_however_many_repeats_arrive() {
        // Auto-repeat sends the same press over and over. Recording each one
        // would produce a pile of releases when capture stops.
        let mut state = State::default();
        let now = Instant::now();

        for _ in 0..8 {
            update(
                &mut state,
                Message::Key {
                    code: KeyCode(0x04),
                    pressed: true,
                },
                now,
            );
        }
        assert_eq!(state.held_keys.len(), 1);

        // The repeats still have to reach the host: injected input does not
        // auto-repeat by itself, so a held key that sends one press types one
        // letter.
        let commands = update(
            &mut state,
            Message::Key {
                code: KeyCode(0x04),
                pressed: true,
            },
            now,
        );
        assert_eq!(inputs(&commands).len(), 1);
    }

    #[test]
    fn a_release_for_something_never_pressed_here_is_not_forwarded() {
        // Press a toolbar button, drag onto the picture, let go. Forwarding
        // that release puts a mouse-up on the host that never had a mouse-down.
        let mut state = State::default();
        let commands = update(
            &mut state,
            Message::Button {
                button: PointerButton::Left,
                pressed: false,
            },
            Instant::now(),
        );
        assert!(inputs(&commands).is_empty());
    }

    #[test]
    fn reaching_the_top_offers_the_keybind_rather_than_the_dock() {
        // The dock used to unfurl on proximity. The top edge of the *remote*
        // screen is where that machine's own menus and window buttons are, so
        // that put a dock over them every time somebody reached for them.
        let mut state = State::new(1, Instant::now());
        state.show(a_picture());
        let later = at(10);
        state.tick(later);
        assert!(!state.wanted(later), "it should have hidden itself by now");

        update(&mut state, Message::Pointer { at: Some((0.5, 0.02)), content: iced::Size::new(800.0, 450.0) }, later);
        state.tick(later);
        assert!(!state.wanted(later), "proximity opened the dock");

        let settled = later + motion::STANDARD;
        assert_eq!(state.hinting(settled), 1.0, "no reminder was offered");
    }

    #[test]
    fn the_reminder_goes_away_again_when_the_pointer_leaves_the_top() {
        let mut state = State::new(1, Instant::now());
        state.show(a_picture());
        let later = at(10);

        update(&mut state, Message::Pointer { at: Some((0.5, 0.02)), content: iced::Size::new(800.0, 450.0) }, later);
        state.tick(later);
        update(&mut state, Message::Pointer { at: Some((0.5, 0.9)), content: iced::Size::new(800.0, 450.0) }, later);
        state.tick(later);

        assert_eq!(state.hinting(later + motion::STANDARD), 0.0);
    }

    #[test]
    fn the_reminder_is_not_shown_next_to_the_thing_it_describes() {
        // Both up at once would be telling someone how to reach the dock they
        // are already looking at.
        let mut state = State::new(1, Instant::now());
        state.show(a_picture());
        let now = Instant::now();

        update(&mut state, Message::Pointer { at: Some((0.5, 0.02)), content: iced::Size::new(800.0, 450.0) }, now);
        update(&mut state, Message::ToggleKeyboard, now);
        state.tick(now);

        let settled = now + motion::STANDARD;
        assert!(state.wanted(settled), "the dock should be up");
        assert_eq!(state.hinting(settled), 0.0, "the reminder was up as well");
    }

    #[test]
    fn the_release_chord_closes_the_controls_as_well_as_opening_them() {
        // It was a one-way door: the chord stopped forwarding, and stopping
        // forwarding is what made the chord itself stop being read, so the
        // only way back was the mouse.
        let now = Instant::now();
        let mut state = State::new(1, now);
        state.show(a_picture());

        update(&mut state, Message::ToggleKeyboard, now);
        assert!(state.wanted(now), "the chord did not open the controls");

        update(&mut state, Message::ToggleKeyboard, now);
        state.tick(at(10));
        assert!(!state.wanted(at(10)), "the chord did not close them again");
    }

    #[test]
    fn the_toolbar_stays_up_while_the_pointer_is_on_it() {
        // Otherwise it hides itself out from under the button being aimed at.
        let mut state = State::new(1, Instant::now());
        state.show(a_picture());
        let later = at(10);

        update(&mut state, Message::OverTools(true), later);
        assert!(state.wanted(later));
        update(&mut state, Message::OverTools(false), later);
        assert!(!state.wanted(later));
    }

    #[test]
    fn the_toolbar_will_not_hide_while_the_keyboard_is_handed_back() {
        // With capture off there is no other sign of it, and a person who
        // cannot see why their typing stopped going anywhere is stuck.
        let mut state = State::new(1, Instant::now());
        update(&mut state, Message::ToggleKeyboard, Instant::now());
        assert!(!state.keyboard);
        assert!(state.wanted(at(60)));
    }

    #[test]
    fn the_release_chord_needs_all_three_modifiers() {
        use iced::keyboard::key::Code;
        use iced::keyboard::Modifiers;

        let p = Physical::Code(Code::KeyP);
        let all = Modifiers::CTRL | Modifiers::ALT | Modifiers::SHIFT;

        assert!(is_release_chord(p, all));
        assert!(!is_release_chord(p, Modifiers::CTRL | Modifiers::ALT));
        assert!(!is_release_chord(p, Modifiers::empty()));
        // A plain P belongs to the other machine.
        assert!(!is_release_chord(Physical::Code(Code::KeyQ), all));
    }

    #[test]
    fn the_chord_avoids_the_keys_windows_claims_for_itself() {
        use iced::keyboard::key::Code;
        use iced::keyboard::Modifiers;

        // `Ctrl+Shift+Esc` is Task Manager and `Ctrl+Alt+Del` is the secure
        // attention sequence. Windows answers both below any application, so a
        // chord built on Escape or Delete is one Pravera may never see — on
        // the key somebody presses precisely when they want out.
        let all = Modifiers::CTRL | Modifiers::ALT | Modifiers::SHIFT;
        for claimed in [Code::Escape, Code::Delete, Code::Tab] {
            assert!(
                !is_release_chord(Physical::Code(claimed), all),
                "{claimed:?} is spoken for by the operating system"
            );
        }
    }

    #[test]
    fn a_mouse_button_with_no_name_in_the_protocol_is_dropped() {
        // Pressing a guess would press the wrong button on someone else's
        // machine, which is worse than the button doing nothing.
        assert_eq!(to_button(mouse::Button::Other(9)), None);
        assert_eq!(to_button(mouse::Button::Back), Some(PointerButton::Back));
    }

    #[test]
    fn a_long_display_name_keeps_the_end_that_tells_it_apart() {
        // `\\.\DISPLAY1` and `\\.\DISPLAY2` differ in the last character.
        assert_ne!(
            short_name(r"\\.\DISPLAY1", 0),
            short_name(r"\\.\DISPLAY2", 1)
        );
        assert_eq!(short_name("HDMI-1", 0), "HDMI-1");
        assert_eq!(short_name("", 3), "Display 4");
    }

    #[test]
    fn every_cell_of_a_control_is_as_wide_as_its_longest_label_needs() {
        // Seven characters of monospace at the small size, and the room either
        // side, and no measuring of anything.
        let width = cell_width(8);
        assert!(width > 8.0 * t::TEXT_XS * 0.6);
        assert_eq!(width, 8.0 * 0.6 * t::TEXT_XS + 2.0 * t::SPACE_3);
        // More characters, wider; never narrower than the room alone.
        assert!(cell_width(9) > cell_width(8));
        assert_eq!(cell_width(0), 2.0 * t::SPACE_3);
    }

    #[test]
    fn the_tile_sits_exactly_under_the_cell_it_is_on_and_between_two_on_the_way() {
        let width = cell_width(8);
        for index in 0..3 {
            // Cell `index` starts after `index` cells and the gaps between them.
            assert_eq!(tile_x(index as f32, width), index as f32 * width + index as f32 * SEGMENT_GAP);
        }
        let halfway = tile_x(0.5, width);
        assert!(halfway > tile_x(0.0, width) && halfway < tile_x(1.0, width));
    }

    #[test]
    fn the_first_look_at_the_link_puts_the_tiles_there_and_a_later_change_slides_them() {
        let now = Instant::now();
        let mut state = State::new(2, now);
        state.choose(1, 1, now);
        assert_eq!(state.quality_thumb.position(now), 1.0);
        assert!(!state.is_animating(now + REVEAL_FOR * 2));

        state.choose(2, 1, now);
        assert!(state.quality_thumb.is_animating(now));
        assert!(!state.display_thumb.is_animating(now));
        let part = state.quality_thumb.position(now + motion::STANDARD / 3);
        assert!(part > 1.0 && part < 2.0, "{part}");
    }

    // ------------------------------------------------------ waiting screen

    #[test]
    fn the_toolbar_never_hides_on_a_session_with_no_picture() {
        // The trap this closes: the toolbar slides away two seconds into a
        // session that is still waiting for its first frame, and neither way
        // of summoning it back exists yet. Pointer position comes from the
        // video widget and so does the release chord, so there is no picture,
        // no pointer, no chord, and no Disconnect button.
        let now = Instant::now();
        let mut state = State::new(1, now);

        let long_after = now + REVEAL_FOR * 4;
        state.tick(long_after);
        assert_eq!(
            state.showing(long_after + motion::STANDARD),
            1.0,
            "the only way out of a blank session disappeared"
        );
    }

    #[test]
    fn the_toolbar_goes_back_to_hiding_once_a_picture_arrives() {
        // The other half: it must not become permanent furniture over the
        // remote screen, which is the whole reason it hides.
        let now = Instant::now();
        let mut state = State::new(1, now);
        state.show(a_picture());

        let long_after = now + REVEAL_FOR * 4;
        state.tick(long_after);
        assert_eq!(state.showing(long_after + motion::STANDARD), 0.0);
    }

    #[test]
    fn a_reconfigure_puts_the_toolbar_back_up_with_the_picture_gone() {
        // Switching monitors drops the picture. Same trap, same escape.
        let now = Instant::now();
        let mut state = State::new(2, now);
        state.show(a_picture());
        state.reconfigured(now);

        let long_after = now + REVEAL_FOR * 4;
        state.tick(long_after);
        assert_eq!(state.showing(long_after + motion::STANDARD), 1.0);
    }

    fn a_picture() -> Picture {
        Picture::new(Resolution::new(4, 2), vec![0; 4 * 2 * 4], 1)
    }

    fn nothing_arrived() -> VideoStats {
        VideoStats::default()
    }

    fn arrived(datagrams: u64) -> VideoStats {
        VideoStats {
            datagrams_received: datagrams,
            frames_assembled: 2,
            frames_incomplete: 1,
            ..VideoStats::default()
        }
    }

    #[test]
    fn a_session_that_has_only_just_started_is_simply_waiting() {
        // A first frame takes an encode and a round trip. Announcing a problem
        // inside that window would call every healthy session broken.
        let (headline, detail) = waiting_words(
            "EVERCORE",
            FIRST_FRAME_PATIENCE - Duration::from_millis(1),
            &nothing_arrived(),
        );
        assert_eq!(headline, "Connected");
        assert!(detail.contains("EVERCORE"), "{detail}");
    }

    #[test]
    fn a_session_where_nothing_ever_arrives_says_so_rather_than_waiting_on() {
        let (headline, detail) =
            waiting_words("EVERCORE", Duration::from_secs(30), &nothing_arrived());
        assert!(headline.contains("No video"), "{headline}");
        assert!(headline.contains("EVERCORE"), "{headline}");
        assert!(!detail.contains("Waiting"), "{detail}");
    }

    #[test]
    fn neither_possible_cause_is_presented_as_the_established_one() {
        // The client can see that nothing arrived. It cannot see whether the
        // host sent nothing or whether the path ate it, and must not pick one.
        let (_, detail) = waiting_words("EVERCORE", Duration::from_secs(30), &nothing_arrived());
        assert!(detail.contains("Either"), "{detail}");
        assert!(detail.contains("or nothing"), "{detail}");
    }

    #[test]
    fn packets_arriving_without_a_picture_is_a_different_sentence() {
        // The two failures have nothing to do with each other, and telling
        // someone their host is not capturing when it plainly is sends them to
        // the wrong machine.
        let (headline, detail) = waiting_words("EVERCORE", Duration::from_secs(30), &arrived(41));
        assert!(headline.contains("arriving"), "{headline}");
        assert!(detail.contains("41"), "{detail}");
    }

    #[test]
    fn no_count_is_shown_that_was_not_counted() {
        // Every number in the sentence comes from the stats block. A figure
        // the client invented would be indistinguishable on screen from a
        // measured one.
        let (_, detail) = waiting_words("EVERCORE", Duration::from_secs(30), &arrived(7));
        for figure in ["7", "2", "1"] {
            assert!(detail.contains(figure), "{figure} missing from {detail}");
        }
    }

    fn remote(visible: bool, drawable_shape: bool) -> Remote {
        Remote {
            x: 10,
            y: 10,
            visible,
            shape: drawable_shape.then(|| {
                Arc::new(Shape {
                    handle: iced::widget::image::Handle::from_rgba(1, 1, vec![0u8; 4]),
                    width: 1,
                    height: 1,
                    hot_x: 0,
                    hot_y: 0,
                })
            }),
        }
    }

    #[test]
    fn a_host_that_sends_no_cursor_leaves_the_local_one_alone() {
        // Protocol version 2 hosts, and version 3 ones before their first
        // shape: exactly the behaviour before the host's cursor was drawn.
        let state = State::default();
        assert!(!state.replaces_local_cursor(true));
        assert!(!state.replaces_local_cursor(false));
    }

    fn with_remote(remote: Remote) -> State {
        State {
            remote: Some(remote),
            ..State::default()
        }
    }

    #[test]
    fn while_controlling_the_hosts_cursor_replaces_the_local_one() {
        let state = with_remote(remote(true, true));
        assert!(state.replaces_local_cursor(true));
    }

    #[test]
    fn a_view_only_login_keeps_its_own_cursor() {
        let state = with_remote(remote(true, true));
        assert!(!state.replaces_local_cursor(false));
    }

    #[test]
    fn a_cursor_the_host_hid_gives_the_local_one_back() {
        assert!(!with_remote(remote(false, true)).replaces_local_cursor(true));
        // A position naming a shape that never arrived is the same.
        assert!(!with_remote(remote(true, false)).replaces_local_cursor(true));
    }

    #[test]
    fn gaming_mode_hides_the_local_cursor_whatever_the_host_says() {
        let state = State {
            gaming: true,
            remote: Some(remote(false, false)),
            ..State::default()
        };
        assert!(state.replaces_local_cursor(false));
        assert!(state.replaces_local_cursor(true));
    }
}
