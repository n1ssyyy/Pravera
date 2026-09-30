//! This machine: who it is, whether it will let anyone in, and whether it does
//! that without being asked.
//!
//! ## Hosting from a window is a stated limit, not a hidden one
//!
//! Switching hosting on here runs the host inside this process, in the signed-in
//! desktop session. It cannot show the lock screen or a UAC prompt, and it only
//! exists once somebody has signed in. Both are properties of running in a user
//! session rather than as a service, both are written on the screen, and the
//! privileged service that removes them is P5.
//!
//! ## Accounts outlive the process
//!
//! The account created here is written to disk as an Argon2id hash, never as a
//! password. That is not a convenience: a machine with no monitor has nobody to
//! type a password into it, so an in-memory account would make unattended
//! hosting impossible rather than merely inconvenient.
//!
//! ## A host with no password is not offered
//!
//! An empty password is refused outright rather than warned about: the connect
//! code is public by design, so "no password" means "anyone who has seen this
//! screen can drive this machine".

use std::time::Instant;

use iced::widget::{button, column, container, keyed_column, mouse_area, row, text, text_input};
use iced::{Alignment, Background, Border, Element, Length};

use pravera_core::DeviceId;

use crate::components::{self, Tone};
use crate::icon;
use crate::motion::{self, HoverTracker, Tween};
use crate::net::host::Hosting;
use crate::theme::{self, tokens as t};

/// Hover slots: the role choices, then each fixed control.
///
/// The role count is not a copy of what `pravera-auth` currently has — the
/// list is read from [`crate::net::host::roles`], and this is only the size of
/// the tracker, generous enough that a role added there cannot silently share
/// a hover slot with a button.
const ROLE_SLOTS: usize = 8;
const SLOT_TOGGLE: usize = ROLE_SLOTS;
const SLOT_COPY: usize = ROLE_SLOTS + 1;
const SLOT_AT_SIGN_IN: usize = ROLE_SLOTS + 2;
const SLOT_HOST_AT_LAUNCH: usize = ROLE_SLOTS + 3;
const SLOT_ADD_DISPLAY: usize = ROLE_SLOTS + 4;
const SLOT_INSTALL_DRIVER: usize = ROLE_SLOTS + 5;
const SLOT_UPDATE: usize = ROLE_SLOTS + 6;
const SLOT_AUTO_UPDATE: usize = ROLE_SLOTS + 7;
const HOVER_SLOTS: usize = ROLE_SLOTS + 8;

#[derive(Debug, Clone)]
pub enum Message {
    UsernameChanged(String),
    PasswordChanged(String),
    Role(String),
    Hover(usize, bool),
    /// Start hosting, or stop it if it is already running.
    ToggleHosting,
    CopyCode,
    /// Register or unregister Pravera to start when this user signs in.
    ToggleAtSignIn,
    /// Register the boot service, or remove it if it is registered. Asks
    /// Windows for administrator rights, because that is what it takes.
    ToggleBootService,
    /// Clear the note or error under the Startup rows.
    DismissStartupNote,
    /// Whether starting Pravera should also start hosting.
    ToggleHostAtLaunch,
    DismissVirtualNotice,
    /// Add a real 1920x1080 virtual display via the bundled IDD driver.
    /// Needs administrator rights, which Windows is asked for.
    AddVirtualDisplay,
    /// Stage the bundled IDD driver package (`pnputil /add-driver`).
    /// Needs administrator rights, which Windows is asked for.
    InstallDriver,
    /// Ask the release feed now rather than at the next scheduled check.
    CheckForUpdates,
    /// Put the downloaded update in place and restart into it.
    RestartToUpdate,
    /// Whether a downloaded update may be applied without asking.
    ToggleAutoUpdate,
    /// Show another section.
    Section(Section),
    /// The pointer moved onto, or off of, an entry in the section list.
    HoverNav(usize, bool),
}

/// What the Updates section shows. Worked out by the application, which owns
/// the updater; this screen only draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Updates {
    pub headline: String,
    pub detail: String,
    pub action: UpdateAction,
    /// Apply downloaded updates by themselves when nothing is in use.
    pub auto: bool,
    /// Updating is possible in this build at all.
    pub enabled: bool,
    /// A newer version is on its way or waiting.
    pub pending: bool,
    /// The state in a few words, for the section list.
    pub brief: String,
}

/// The one button the Updates group offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    Check,
    Busy,
    Restart,
}

/// An account this machine already knows, as much of it as a screen may see.
///
/// No password and no hash: what is stored is an Argon2id verifier, and putting
/// it in front of an interface would only invite it into a log or a screenshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub username: String,
    pub role: String,
}

/// Whether Windows starts Pravera at boot, without anybody signing in.
///
/// Reduced from what the service control manager actually said: registering
/// for the first time, repointing an existing registration and finding it
/// already correct are three different things to do and one thing to read. A
/// screen that told them apart would be reporting on Pravera's bookkeeping
/// instead of on whether the machine comes back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Service {
    /// Registered. This machine starts Pravera at boot and again after every
    /// sign-out, whether or not anybody signs in.
    Installed,
    /// Not registered. Registering needs administrator rights, which the button
    /// beside this row asks Windows for. Not a fault: everything else works.
    NotRegistered,
    /// Registered, but for another copy of Pravera: this file moved, or a
    /// different one set the service up.
    Elsewhere,
    /// Windows refused, in its own words. Carried rather than summarised,
    /// because an unusual refusal is exactly the case a generic sentence
    /// would strand somebody in.
    Refused(String),
    /// This platform has no such service.
    Unavailable,
}

/// What this machine does when nobody is in front of it.
///
/// Assembled by the caller rather than read here, because most of these
/// answers live outside this screen's reach: the registry, the service control
/// manager, and a probe of the graphics outputs that is far too expensive to
/// repeat on every draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unattended {
    /// Pravera is registered to start when this user signs in.
    pub at_sign_in: bool,
    /// Starting Pravera should also start hosting.
    pub host_at_launch: bool,
    /// Whether there is a notification-area icon, and therefore whether
    /// closing the window leaves anything behind that could reopen it.
    pub tray: bool,
    /// How many displays this machine reports. `None` while it is still being
    /// worked out — unknown, not zero. `Some(0)` is the headless case; hosting
    /// refuses then until the virtual display driver lands.
    pub displays: Option<usize>,
    /// The virtual display driver, the device it runs, and whether this
    /// machine needs it — from the last background check, `None` before the
    /// first one lands. Never probed on the drawing path: it walks the device
    /// tree and every monitor.
    pub virtual_display: Option<pravera_capture::VirtualDisplayStatus>,
    /// Which capture backend is in use, named as the backend names itself.
    /// `synthetic` is the test-harness override.
    pub capture_backend: String,
    /// Whether the machine comes back at boot rather than at sign-in. Read on
    /// start and again on every visit to Settings.
    pub service: Service,
    /// The boot service is being changed, and Windows is waiting for a person
    /// to answer its prompt.
    pub boot_pending: bool,
    /// Any administrator job is running, so the others wait their turn.
    pub elevating: bool,
}

/// Something to say under the Startup rows.
///
/// Its own field rather than `error`: that one is Hosting's, and is drawn only
/// on the Hosting page, so a startup failure stored there could never be seen
/// from the page the switch is on. That is how "Start Pravera when I sign in"
/// used to fail with no sign of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Startup {
    /// Nothing went wrong, and nothing changed: the person said no to the
    /// Windows prompt.
    Note(String),
    /// Something went wrong, in words fit to show.
    Error(String),
}

pub struct State {
    username: String,
    password: String,
    role: String,
    /// What is already saved on this machine. Read once and after every save,
    /// never on the drawing path.
    accounts: Vec<Account>,
    /// Set while the endpoint is being bound.
    starting: bool,
    /// Why hosting could not start, in the words shown to the person.
    error: Option<String>,
    /// Confirmation that the code went to the clipboard, cleared on the next
    /// thing that happens.
    copied: bool,
    /// Last virtual-display action, if any. Cleared when the card is dismissed
    /// or another action is taken.
    virtual_notice: Option<String>,
    /// What last happened to the Startup rows, if it is worth saying.
    startup: Option<Startup>,
    hover: HoverTracker,
    /// When the page last arrived; its panels cascade in from here.
    arrived: Instant,
    /// One animation per switch, answering "how on is this".
    ///
    /// The switches show state the application owns, so the application
    /// tells this screen after every update — see [`State::sync_switches`].
    /// Adopting the change while drawing instead used to start the animation
    /// after the frame subscription had already been decided, and the knob
    /// sat still until something else happened to redraw the window.
    switches: std::collections::HashMap<usize, iced::Animation<bool>>,
    /// Whether this machine encodes in hardware; `None` until the probe,
    /// which loads Media Foundation and is kept off the drawing path.
    hardware: Option<bool>,
    /// The section on show, and when it came on show.
    section: Section,
    section_at: Instant,
    /// One hover per entry of the section list.
    nav_hover: HoverTracker,
    /// One hold per entry: 1 for the section on show, 0 for the rest, each
    /// moving on its own clock so the highlight crossfades between entries.
    nav_active: [Tween; Section::ALL.len()],
    /// The role that was chosen before this one, and how far the hand-over from
    /// it to the current one has got.
    previous_role: Option<String>,
    role_swap: Tween,
}

impl Default for State {
    fn default() -> Self {
        State {
            username: String::new(),
            password: String::new(),
            // Not `admin`. The default should be the role that does the job
            // people actually want, and managing users remotely is not it.
            role: "operator".to_string(),
            accounts: Vec::new(),
            starting: false,
            error: None,
            copied: false,
            virtual_notice: None,
            startup: None,
            hover: HoverTracker::new(HOVER_SLOTS),
            arrived: Instant::now(),
            switches: std::collections::HashMap::new(),
            hardware: None,
            section: Section::Hosting,
            section_at: Instant::now(),
            nav_hover: HoverTracker::new(Section::ALL.len()),
            nav_active: std::array::from_fn(|index| Tween::at(if index == 0 { 1.0 } else { 0.0 })),
            previous_role: None,
            role_swap: Tween::at(1.0),
        }
    }
}

impl State {
    /// The screen as it opens, knowing what this machine has already been told.
    pub fn new() -> State {
        let mut state = State::default();
        state.reload_accounts();
        state
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn password(&self) -> &str {
        &self.password
    }

    pub fn role(&self) -> &str {
        &self.role
    }

    /// The accounts already saved on this machine.
    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }

    /// Whether the form describes an account that should be written down.
    ///
    /// Both halves, or neither: a username typed with no password is somebody
    /// part-way through, and starting anyway would host under a different
    /// account than the one on screen.
    pub fn has_new_account(&self) -> bool {
        !self.username.trim().is_empty() && !self.password.is_empty()
    }

    /// Re-read what is on disk.
    ///
    /// A failure leaves the list empty rather than raising anything: the same
    /// failure will be reported properly, with its reason, the moment somebody
    /// tries to host.
    pub fn reload_accounts(&mut self) {
        self.accounts = match crate::net::host::accounts() {
            Ok(store) => store
                .usernames()
                .iter()
                .map(|username| Account {
                    username: (*username).to_string(),
                    role: store.role_of(username).unwrap_or("unknown").to_string(),
                })
                .collect(),
            Err(_) => Vec::new(),
        };
    }

    pub fn is_starting(&self) -> bool {
        self.starting
    }

    pub fn began(&mut self) {
        self.starting = true;
        self.error = None;
        self.copied = false;
    }

    /// Hosting is running. The typed account is done with either way: it was
    /// saved, and it is listed above now.
    pub fn started(&mut self) {
        self.starting = false;
        self.error = None;
        self.username.clear();
        self.password.clear();
        self.reload_accounts();
    }

    pub fn failed(&mut self, reason: String) {
        self.starting = false;
        self.error = Some(reason);
    }

    pub fn copied(&mut self) {
        self.copied = true;
    }

    /// Something happened to the Startup rows that is not a failure.
    pub fn startup_note(&mut self, words: String) {
        self.startup = Some(Startup::Note(words));
    }

    /// Something failed under the Startup rows.
    pub fn startup_error(&mut self, words: String) {
        self.startup = Some(Startup::Error(words));
    }

    pub fn clear_startup(&mut self) {
        self.startup = None;
    }

    pub fn startup(&self) -> Option<&Startup> {
        self.startup.as_ref()
    }

    pub fn virtual_result(&mut self, msg: String) {
        self.virtual_notice = Some(msg);
    }

    pub fn dismiss_virtual(&mut self) {
        self.virtual_notice = None;
    }

    pub fn is_animating(&self, now: Instant) -> bool {
        self.hover.is_animating(now)
            || self.nav_hover.is_animating(now)
            || self.nav_active.iter().any(|hold| hold.is_animating(now))
            || self.role_swap.is_animating(now)
            || self.switches.values().any(|a| a.is_animating(now))
            || motion::cascading(self.arrived, now)
            || motion::cascading(self.section_at, now)
    }

    /// Start the page's entrance at `at`.
    pub fn replay(&mut self, at: Instant) {
        self.arrived = at;
        self.section_at = at;
    }

    /// The section on show.
    pub fn section(&self) -> Section {
        self.section
    }

    /// Show `section`: the highlight hands over from the one before, and the
    /// section's own blocks arrive again.
    pub fn select(&mut self, section: Section, now: Instant) {
        if section == self.section {
            return;
        }
        let left = self.section.index();
        self.nav_active[left].go(0.0, now, motion::STANDARD, motion::EASE_CHANGE);
        self.nav_active[section.index()].go(1.0, now, motion::STANDARD, motion::EASE_CHANGE);
        self.section = section;
        self.section_at = now;
    }

    /// How chosen the role called `name` looks, from 0 to 1: rising for the one
    /// just picked, falling for the one it replaced, and 0 for the rest.
    fn role_amount(&self, name: &str, now: Instant) -> f32 {
        let swap = self.role_swap.value(now).clamp(0.0, 1.0);
        if self.role == name {
            swap
        } else if self.previous_role.as_deref() == Some(name) {
            1.0 - swap
        } else {
            0.0
        }
    }

    /// What the video encoder probe found.
    pub fn set_hardware(&mut self, hardware: bool) {
        self.hardware = Some(hardware);
    }

    /// Point the switches at what the application now says they are. A
    /// switch seen for the first time starts where it is, without travelling.
    pub fn sync_switches(&mut self, at_sign_in: bool, host_at_launch: bool, auto_update: bool, now: Instant) {
        for (slot, on) in [
            (SLOT_AT_SIGN_IN, at_sign_in),
            (SLOT_HOST_AT_LAUNCH, host_at_launch),
            (SLOT_AUTO_UPDATE, auto_update),
        ] {
            let animation = self.switches.entry(slot).or_insert_with(|| motion::standard(on));
            if animation.value() != on {
                animation.go_mut(on, now);
            }
        }
    }

    /// How far the switch in `slot` has travelled toward `on`, from 0 to 1.
    /// A switch the application has not synced yet is simply where it is.
    fn switch_travel(&self, slot: usize, on: bool, now: Instant) -> f32 {
        match self.switches.get(&slot) {
            Some(animation) if animation.value() == on => animation.interpolate(0.0, 1.0, now),
            _ => {
                if on {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }

    /// Whether pressing the button now would do anything.
    fn ready(&self) -> bool {
        if self.starting {
            return false;
        }
        // Anything typed means "host as this new account", and half of one is
        // not an account.
        if !self.username.trim().is_empty() || !self.password.is_empty() {
            return self.has_new_account();
        }
        !self.accounts.is_empty()
    }
}

/// Act on a change. `Some` means the person asked for something this screen
/// cannot do by itself.
pub fn update(state: &mut State, message: Message, now: Instant) -> Option<Message> {
    match message {
        Message::UsernameChanged(username) => {
            state.username = username;
            state.error = None;
            None
        }
        Message::PasswordChanged(password) => {
            state.password = password;
            state.error = None;
            None
        }
        Message::Role(role) => {
            if state.role != role {
                state.previous_role = Some(std::mem::replace(&mut state.role, role));
                state.role_swap.snap(0.0);
                state.role_swap.go(1.0, now, motion::STANDARD, motion::EASE_CHANGE);
            }
            None
        }
        Message::Section(section) => {
            state.select(section, now);
            None
        }
        Message::HoverNav(index, entering) => {
            state.nav_hover.set(index, entering, now);
            None
        }
        Message::Hover(slot, entering) => {
            state.hover.set(slot, entering, now);
            None
        }
        Message::ToggleHosting => Some(Message::ToggleHosting),
        Message::CopyCode => Some(Message::CopyCode),
        Message::ToggleAtSignIn => Some(Message::ToggleAtSignIn),
        Message::ToggleBootService => Some(Message::ToggleBootService),
        Message::DismissStartupNote => {
            state.clear_startup();
            None
        }
        Message::ToggleHostAtLaunch => Some(Message::ToggleHostAtLaunch),
        Message::AddVirtualDisplay => Some(Message::AddVirtualDisplay),
        Message::InstallDriver => Some(Message::InstallDriver),
        Message::CheckForUpdates => Some(Message::CheckForUpdates),
        Message::RestartToUpdate => Some(Message::RestartToUpdate),
        Message::ToggleAutoUpdate => Some(Message::ToggleAutoUpdate),
        Message::DismissVirtualNotice => {
            state.dismiss_virtual();
            None
        }
    }
}

/// The sections of the page, in the order the navigation lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Hosting,
    Displays,
    Unattended,
    Updates,
    Machine,
}

impl Section {
    pub const ALL: [Section; 5] = [
        Section::Hosting,
        Section::Displays,
        Section::Unattended,
        Section::Updates,
        Section::Machine,
    ];

    pub fn index(self) -> usize {
        Section::ALL.iter().position(|other| *other == self).unwrap_or(0)
    }

    pub fn label(self) -> &'static str {
        match self {
            Section::Hosting => "Hosting",
            Section::Displays => "Displays",
            Section::Unattended => "Unattended",
            Section::Updates => "Updates",
            Section::Machine => "This machine",
        }
    }

    /// What the section is for, under its title.
    fn about(self) -> &'static str {
        match self {
            Section::Hosting => "Whether this machine accepts sessions, and who may sign in.",
            Section::Displays => "What hosting can capture, and how a machine with no monitor gets a screen.",
            Section::Unattended => "Whether this machine comes back, and starts hosting, with nobody at it.",
            Section::Updates => "Which version this is, and whether a newer one is on its way.",
            Section::Machine => "Who this machine is, and what it encodes with.",
        }
    }
}

/// Width of the section list down the left of the page.
const NAV_WIDTH: f32 = 208.0;

/// Height of one entry in it: a name over a short state.
const NAV_ROW: f32 = 48.0;

/// The box every entry's mark sits in, wide enough for a badge on a glyph.
const NAV_MARK: f32 = t::ICON + 6.0;

pub fn view<'a>(
    state: &'a State,
    device_id: Option<DeviceId>,
    hosting: Option<&'a Hosting>,
    unattended: &Unattended,
    updates: &Updates,
    now: Instant,
) -> Element<'a, Message> {
    let since = state.arrived;

    let header = components::header("Settings")
        .meta(if hosting.is_some() {
            components::pill("Hosting", Tone::Success)
        } else {
            components::pill("Not hosting", Tone::Neutral)
        })
        .meta(
            text("this machine, and whether other machines may reach it")
                .size(t::TEXT_XS)
                .wrapping(text::Wrapping::None)
                .style(theme::subtle),
        );

    let nav = nav(state, hosting, unattended, updates, device_id, now);

    // Only the chosen section is drawn, keyed by it so that its scroll position
    // is its own and it opens at the top. Its blocks arrive one after another.
    let section = state.section;
    let blocks = match section {
        Section::Hosting => hosting_blocks(state, hosting, unattended.displays, now),
        Section::Displays => displays_blocks(state, unattended),
        Section::Unattended => unattended_blocks(state, unattended, now),
        Section::Updates => updates_blocks(state, updates, now),
        Section::Machine => machine_blocks(state, device_id),
    };
    let mut pane = column![].spacing(t::SPACE_6).width(Length::Fill);
    for (index, block) in std::iter::once(title(section)).chain(blocks).enumerate() {
        pane = pane.push(motion::settle(
            block,
            motion::row_cascade(state.section_at, now, 0, index),
        ));
    }
    let content = keyed_column([(section.index(), components::leading_body(pane))])
        .width(Length::Fill)
        .height(Length::Fill);

    components::page_split(
        motion::settle(header, motion::cascade(since, now, 0)),
        motion::settle(nav, motion::cascade(since, now, 1)),
        NAV_WIDTH,
        content,
    )
}

/// A section's name and what it is for.
fn title<'a>(section: Section) -> Element<'a, Message> {
    column![
        text(section.label())
            .size(t::TEXT_LG)
            .font(t::FONT_UI_STRONG)
            .style(theme::heading),
        text(section.about()).size(t::TEXT_XS).style(theme::muted),
    ]
    .spacing(t::SPACE_1_5)
    .into()
}

// ------------------------------------------------------------------ the list

/// The sections down the left, each with what state it is in, so the list is a
/// summary of the page as well as the way around it.
fn nav<'a>(
    state: &'a State,
    hosting: Option<&Hosting>,
    unattended: &Unattended,
    updates: &Updates,
    device_id: Option<DeviceId>,
    now: Instant,
) -> Element<'a, Message> {
    let mut list = column![].spacing(2.0).width(Length::Fill);
    for (index, section) in Section::ALL.into_iter().enumerate() {
        let (mark, brief): (Element<'a, Message>, String) = match section {
            Section::Hosting => (
                components::dot(if hosting.is_some() { t::LIME } else { t::ROUTE_OFFLINE }, 8.0),
                match hosting {
                    None => "Off".to_string(),
                    Some(hosting) => match hosting.connections() {
                        0 => "Ready".to_string(),
                        n => format!("{n} connected"),
                    },
                },
            ),
            Section::Displays => (
                icon::stroked(icon::DEVICES, t::ICON_SM, t::MUTED_FOREGROUND),
                match unattended.displays {
                    None => "Checking".to_string(),
                    Some(0) => "None found".to_string(),
                    Some(1) => "1 display".to_string(),
                    Some(n) => format!("{n} displays"),
                },
            ),
            Section::Unattended => (
                icon::stroked(icon::BOLT, t::ICON_SM, t::MUTED_FOREGROUND),
                if unattended.service == Service::Installed {
                    "At boot".to_string()
                } else if unattended.at_sign_in {
                    "At sign-in".to_string()
                } else {
                    "Off".to_string()
                },
            ),
            Section::Updates => {
                let glyph = icon::stroked(icon::DOWNLOAD, t::ICON_SM, t::MUTED_FOREGROUND);
                (
                    if updates.pending {
                        components::badged(glyph, t::LIME, t::CARD)
                    } else {
                        glyph
                    },
                    updates.brief.clone(),
                )
            }
            Section::Machine => (
                icon::stroked(icon::LOGO, t::ICON_SM, t::MUTED_FOREGROUND),
                device_id.map_or_else(|| "unavailable".to_string(), |id| short_id(&id.to_string())),
            ),
        };
        list = list.push(nav_row(
            index,
            section,
            mark,
            brief,
            state.nav_hover.amount(index, now),
            state.nav_active[index].value(now),
        ));
    }
    container(list)
        .padding([t::SPACE_3, t::SPACE_2])
        .width(Length::Fill)
        .into()
}

/// The start of a long identifier: enough to recognise it by.
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// One entry. Its fill is the sum of two animations: a lift while the pointer
/// is on it, and a hold while it is the section on screen, so the highlight
/// crossfades from the entry left to the entry chosen.
fn nav_row<'a>(
    index: usize,
    section: Section,
    mark: Element<'a, Message>,
    brief: String,
    hover: f32,
    active: f32,
) -> Element<'a, Message> {
    let ink = theme::blend(theme::blend(t::NEUTRAL_300, t::FOREGROUND, hover), t::FOREGROUND, active);
    let body = row![
        container(mark)
            .center_x(Length::Fixed(NAV_MARK))
            .center_y(Length::Fixed(NAV_MARK)),
        column![
            text(section.label())
                .size(t::TEXT_SM)
                .font(t::FONT_UI_MEDIUM)
                .wrapping(text::Wrapping::None)
                .style(theme::tinted(ink)),
            text(brief)
                .size(t::TEXT_XS)
                .font(t::FONT_MONO)
                .wrapping(text::Wrapping::None)
                .style(theme::subtle),
        ]
        .spacing(2.0)
        .width(Length::Fill),
    ]
    .spacing(t::SPACE_2)
    .align_y(Alignment::Center);

    let surface = button(container(body).center_y(Length::Fill).width(Length::Fill).height(Length::Fill))
        .width(Length::Fill)
        .height(Length::Fixed(NAV_ROW))
        .padding([0.0, t::SPACE_2 + 2.0])
        .style(move |_, status| button::Style {
            background: Some(Background::Color(match status {
                button::Status::Pressed => theme::blend(t::NEUTRAL_825, t::NEUTRAL_850, 0.5 + 0.5 * active),
                _ => theme::blend(t::with_alpha(t::NEUTRAL_800, 0.7 * hover), t::SELECTED, active),
            })),
            text_color: ink,
            border: Border {
                color: t::with_alpha(t::BEVEL_RAISED.sides, active),
                width: 1.0,
                radius: t::RADIUS.into(),
            },
            ..button::Style::default()
        })
        .on_press(Message::Section(section));

    mouse_area(surface)
        .on_enter(Message::HoverNav(index, true))
        .on_exit(Message::HoverNav(index, false))
        .into()
}

// -------------------------------------------------------------------- groups

/// A labelled run of rows: the label, a hairline, and each row split from the
/// next by another. There is no box around it; the rules and the label are what
/// say where it starts and ends.
fn group<'a>(label: &str, rows: Vec<Element<'a, Message>>) -> Element<'a, Message> {
    let mut list = column![
        container(components::section_label(label)).padding(iced::Padding {
            top: 0.0,
            right: t::SPACE_3,
            bottom: t::SPACE_2,
            left: t::SPACE_3,
        }),
        components::hairline(),
    ]
    .width(Length::Fill);
    for (index, row) in rows.into_iter().enumerate() {
        if index > 0 {
            list = list.push(components::hairline());
        }
        list = list.push(row);
    }
    list.into()
}

/// Whether this machine is accepting sessions, and who may sign in.
///
/// Takes the display count rather than the whole [`Unattended`]: it is the only
/// part of it this section can act on, and one that took the rest would look
/// like it might start using it.
fn hosting_blocks<'a>(
    state: &'a State,
    hosting: Option<&'a Hosting>,
    displays: Option<usize>,
    now: Instant,
) -> Vec<Element<'a, Message>> {
    let mut blocks = Vec::new();

    match hosting {
        Some(hosting) => {
            let peers = match hosting.connections() {
                0 => "No machine is connected.".to_string(),
                1 => "1 machine is connected.".to_string(),
                n => format!("{n} machines are connected."),
            };
            blocks.push(group(
                "Status",
                vec![state_row(
                    t::LIME,
                    "Accepting sessions",
                    peers,
                    Some(hovered(
                        SLOT_TOGGLE,
                        components::small_button(Some(icon::DISCONNECT), "Stop hosting", Some(Message::ToggleHosting)),
                    )),
                )],
            ));
            blocks.push(group(
                "Connect code",
                vec![pad(column![
                    components::code(
                        hosting.code(),
                        Some(hovered(SLOT_COPY, components::copy_button(state.copied, Message::CopyCode))),
                    ),
                    // Two different situations that look identical from the far
                    // end. Somebody staring at an empty device list needs to know
                    // whether this machine is findable at all before they go
                    // hunting for a reason on their own side.
                    components::note(if hosting.listed_on_lan() {
                        "This is the public key itself, which is why it is long. Machines on this \
                         network find this one by name; the code is for reaching it from anywhere else."
                    } else {
                        "This is the public key itself, which is why it is long. This machine is not \
                         listed on the local network, so paste the code into Connect on the other machine."
                    }),
                ]
                .spacing(t::SPACE_2))],
            ));
        }
        None => {
            let action = if state.is_starting() {
                "Starting…"
            } else if state.has_new_account() {
                "Save and start"
            } else {
                "Start hosting"
            };
            blocks.push(group(
                "Status",
                vec![state_row(
                    t::ROUTE_OFFLINE,
                    "Not accepting sessions",
                    if state.accounts().is_empty() {
                        "Create the account other machines will sign in with, then start."
                    } else {
                        "Other machines can sign in once hosting starts."
                    },
                    Some(hovered(
                        SLOT_TOGGLE,
                        components::primary_button(
                            Some(icon::BOLT),
                            action,
                            state.ready().then_some(Message::ToggleHosting),
                        ),
                    )),
                )],
            ));

            if let Some(error) = &state.error {
                blocks.push(components::callout(icon::ALERT, error.as_str(), Tone::Danger));
            }
            // The failure a headless machine hits, said before the password is
            // typed rather than after. Hosting refuses with the same words.
            if displays == Some(0) {
                blocks.push(components::callout(
                    icon::ALERT,
                    crate::net::host::NO_DISPLAY,
                    Tone::Danger,
                ));
            }

            blocks.push(group("New account", vec![pad(new_account_form(state, now))]));
        }
    }

    if !state.accounts().is_empty() {
        blocks.push(saved_accounts(state));
    }

    // What hosting from a signed-in session cannot do. Stated once, plainly,
    // and always, not behind a disclosure, because it changes whether this
    // feature is fit for what someone is about to use it for.
    blocks.push(components::callout(
        icon::ALERT,
        "Hosting runs inside your signed-in desktop session: it cannot show the lock screen or a \
         UAC prompt, and it exists only while somebody is signed in.",
        Tone::Warning,
    ));

    blocks
}

/// The typed half of hosting: an account to add, and what it may do.
fn new_account_form<'a>(state: &'a State, now: Instant) -> Element<'a, Message> {
    column![
        components::note(if state.accounts().is_empty() {
            "It is saved on this machine as an Argon2id hash, never as a password, so a machine \
             that reboots by itself can still be reached."
        } else {
            "Add another account, or leave both fields empty to host with the accounts below."
        }),
        row![
            field("Username", "username", state.username(), false, Message::UsernameChanged),
            field("Password", "password", state.password(), true, Message::PasswordChanged),
        ]
        .spacing(t::SPACE_4),
        roles(state, now),
    ]
    .spacing(t::SPACE_4)
    .into()
}

/// Who can already sign in to this machine.
///
/// Worth showing whether or not hosting is running: these are the accounts an
/// unattended machine will accept at three in the morning, and this is the only
/// place anyone would find that out.
fn saved_accounts<'a>(state: &'a State) -> Element<'a, Message> {
    group(
        "Accounts on this machine",
        state
            .accounts()
            .iter()
            .map(|account| {
                pad(row![
                    icon::stroked(icon::LOCK, 13.0, t::SUBTLE_FOREGROUND),
                    text(account.username.clone())
                        .size(t::TEXT_SM)
                        .font(t::FONT_MONO)
                        .wrapping(text::Wrapping::None)
                        .style(theme::tinted(t::NEUTRAL_200))
                        .width(Length::Fill),
                    components::pill(account.role.clone(), Tone::Outline),
                ]
                .spacing(t::SPACE_3)
                .align_y(Alignment::Center))
            })
            .collect(),
    )
}

/// Displays on this machine, and how a headless one gets a real one.
///
/// An elevated Pravera on a machine with no monitor installs the driver and
/// adds the display by itself. The section also offers both actions explicitly,
/// for a desktop where a virtual display is wanted anyway. Pressed unelevated
/// they ask Windows for administrator rights and wait for the answer.
fn displays_blocks<'a>(state: &'a State, unattended: &Unattended) -> Vec<Element<'a, Message>> {
    let backend = unattended.capture_backend.clone();
    let backend_pill = || -> Element<'a, Message> { components::pill(backend.clone(), Tone::Outline) };

    // `None` is "still counting", not headless: rendering it as 0 lied
    // about probe failures.
    let Some(count) = unattended.displays else {
        return vec![group(
            "Capture",
            vec![state_row(
                t::MUTED_FOREGROUND,
                "Still checking…",
                "Counting this machine's displays.",
                Some(backend_pill()),
            )],
        )];
    };

    let headline = match count {
        0 => "No displays found".to_string(),
        1 => "1 display".to_string(),
        n => format!("{n} displays"),
    };
    let status = unattended.virtual_display.as_ref();
    let active = status.is_some_and(|s| s.display_active);
    let headless = status.is_some_and(|s| s.needs_display);
    let elevated = status.is_some_and(|s| s.elevated);

    // Headless is decided from what is behind each display (its EDID), not
    // from the count: a machine with no monitor still lists Windows'
    // placeholder, and that is the state that once read "A display was found.
    // Hosting will capture it." while every session stayed black.
    let (tint, detail) = if backend == "synthetic" {
        (
            t::WARNING,
            "Capture is overridden to a test pattern (PRAVERA_CAPTURE=synthetic). Sessions will not show this screen.",
        )
    } else if status.is_none() {
        (t::MUTED_FOREGROUND, "Checking what is behind each display…")
    } else if headless && !elevated {
        (
            t::WARNING,
            "No monitor is attached, so there is nothing to share yet. Add the virtual display below and Windows will ask for permission once. Hosting refuses until then.",
        )
    } else if headless {
        (
            t::WARNING,
            "No monitor is attached. Pravera is adding its virtual display; hosting refuses until it appears.",
        )
    } else if active && count == 1 {
        (
            t::SUCCESS,
            "No monitor is attached, so Pravera's virtual display is this machine's screen. Hosting captures it, and it comes back by itself after a reboot.",
        )
    } else if active {
        (t::SUCCESS, "Hosting captures these, including Pravera's virtual display.")
    } else {
        (t::SUCCESS, "Hosting captures what is on them.")
    };

    let mut blocks = vec![group("Capture", vec![state_row(tint, headline, detail, Some(backend_pill()))])];

    // Explicit controls, for a person at the machine. The automatic path only
    // ever adds the display to a machine with no monitor; on a desktop, a
    // virtual display is something somebody asks for.
    let mut controls = row![].spacing(t::SPACE_2).align_y(Alignment::Center);
    let mut any = false;
    if status.is_some_and(|s| !s.driver_staged) {
        any = true;
        controls = controls.push(hovered(
            SLOT_INSTALL_DRIVER,
            components::small_button(
                Some(icon::DOWNLOAD),
                "Install driver",
                (!unattended.elevating).then_some(Message::InstallDriver),
            ),
        ));
    }
    if status.is_some() && !active {
        any = true;
        controls = controls.push(hovered(
            SLOT_ADD_DISPLAY,
            components::small_button(
                Some(icon::PLUS),
                if unattended.elevating {
                    "Waiting for permission…"
                } else {
                    "Add 1920×1080 display"
                },
                (!unattended.elevating).then_some(Message::AddVirtualDisplay),
            ),
        ));
    }
    if any {
        blocks.push(group(
            "Virtual display",
            vec![setting(
                "Add one",
                if !elevated && !headless {
                    "A display that exists without a monitor. Windows asks for permission first, because installing a display driver needs administrator rights."
                } else {
                    "A display that exists without a monitor, for hosting a machine that has none."
                },
                Some(controls.into()),
            )],
        ));
    }

    if let Some(notice) = &state.virtual_notice {
        blocks.push(
            row![
                components::callout(icon::ALERT, notice.as_str(), Tone::Warning),
                components::small_button(None, "Dismiss", Some(Message::DismissVirtualNotice)),
            ]
            .spacing(t::SPACE_2)
            .align_y(Alignment::Center)
            .into(),
        );
    }
    blocks
}

/// Whether this machine comes back by itself.
fn unattended_blocks<'a>(state: &'a State, unattended: &Unattended, now: Instant) -> Vec<Element<'a, Message>> {
    let closing = if unattended.tray {
        "Closing the window while hosting leaves Pravera in the notification area, still \
         accepting sessions. Closing it while not hosting quits."
    } else {
        "Closing the window stops hosting: this build has no notification-area icon, so there \
         would be nothing left to reopen it or stop it with."
    };

    let mut blocks = vec![group(
        "Startup",
        vec![
            boot_row(&unattended.service, unattended.boot_pending, unattended.elevating),
            components::switch_row(
                "Start Pravera when I sign in",
                "Registered under your own account as a task named Pravera, which Task Scheduler lists and can remove.",
                state.switch_travel(SLOT_AT_SIGN_IN, unattended.at_sign_in, now),
                state.hover.amount(SLOT_AT_SIGN_IN, now),
                Message::ToggleAtSignIn,
                Message::Hover(SLOT_AT_SIGN_IN, true),
                Message::Hover(SLOT_AT_SIGN_IN, false),
            ),
            components::switch_row(
                "Start hosting when Pravera starts",
                "Accepts sessions as soon as it runs, using the accounts saved on this machine.",
                state.switch_travel(SLOT_HOST_AT_LAUNCH, unattended.host_at_launch, now),
                state.hover.amount(SLOT_HOST_AT_LAUNCH, now),
                Message::ToggleHostAtLaunch,
                Message::Hover(SLOT_HOST_AT_LAUNCH, true),
                Message::Hover(SLOT_HOST_AT_LAUNCH, false),
            ),
        ],
    )];

    // Worth saying once both switches are on, because that is the point at
    // which somebody is relying on the machine coming back by itself, and
    // sign-in is not boot. Suppressed when the service is registered, since
    // then the machine genuinely does come back on its own and the warning
    // would be describing a gap that is already closed.
    if unattended.at_sign_in && unattended.host_at_launch && unattended.service != Service::Installed {
        blocks.push(components::callout(
            icon::ALERT,
            "This starts at sign-in, not at boot: a machine sitting at the sign-in screen after a \
             reboot is not running Pravera and cannot be reached. For a machine with no keyboard, \
             use Start at boot above, which starts Pravera before anybody signs in.",
            Tone::Warning,
        ));
    }
    if unattended.host_at_launch && state.accounts().is_empty() {
        blocks.push(components::callout(
            icon::ALERT,
            "There are no accounts on this machine yet, so hosting will refuse to start. Create \
             one under Hosting.",
            Tone::Danger,
        ));
    }

    match state.startup() {
        Some(Startup::Error(words)) => blocks.push(startup_notice(icon::ALERT, words, Tone::Danger)),
        Some(Startup::Note(words)) => blocks.push(startup_notice(icon::ALERT, words, Tone::Warning)),
        None => {}
    }

    blocks.push(components::note(closing));
    blocks
}

/// A note or an error under the Startup rows, with a way to put it away.
fn startup_notice<'a>(glyph: &'static str, words: &'a str, tone: Tone) -> Element<'a, Message> {
    row![
        components::callout(glyph, words, tone),
        components::small_button(None, "Dismiss", Some(Message::DismissStartupNote)),
    ]
    .spacing(t::SPACE_2)
    .align_y(Alignment::Center)
    .into()
}

/// Which version this is, and whether a newer one is on its way.
fn updates_blocks<'a>(state: &'a State, updates: &Updates, now: Instant) -> Vec<Element<'a, Message>> {
    let tint = if updates.pending {
        t::LIME
    } else if updates.enabled {
        t::SUCCESS
    } else {
        t::ROUTE_OFFLINE
    };
    let button: Option<Element<'a, Message>> = if !updates.enabled {
        None
    } else {
        Some(hovered(
            SLOT_UPDATE,
            match updates.action {
                UpdateAction::Check => components::small_button(
                    Some(icon::REFRESH),
                    "Check now",
                    Some(Message::CheckForUpdates),
                ),
                UpdateAction::Busy => components::small_button(Some(icon::REFRESH), "Checking…", None),
                UpdateAction::Restart => components::primary_button(
                    Some(icon::DOWNLOAD),
                    "Restart to update",
                    Some(Message::RestartToUpdate),
                ),
            },
        ))
    };

    let mut rows = vec![state_row(tint, updates.headline.clone(), updates.detail.clone(), button)];
    if updates.enabled {
        rows.push(components::switch_row(
            "Update by itself",
            "A downloaded update is applied when no session is open, nobody is connected and the \
             window is closed. Otherwise it waits for the button above.",
            state.switch_travel(SLOT_AUTO_UPDATE, updates.auto, now),
            state.hover.amount(SLOT_AUTO_UPDATE, now),
            Message::ToggleAutoUpdate,
            Message::Hover(SLOT_AUTO_UPDATE, true),
            Message::Hover(SLOT_AUTO_UPDATE, false),
        ));
    }
    vec![group("Release", rows)]
}

/// Who this machine is, and what it will encode with.
fn machine_blocks<'a>(state: &'a State, device_id: Option<DeviceId>) -> Vec<Element<'a, Message>> {
    let id: Element<'a, Message> = match device_id {
        Some(id) => text(id.to_string())
            .size(t::TEXT_SM)
            .font(t::FONT_MONO_STRONG)
            .wrapping(text::Wrapping::None)
            .style(theme::tinted(t::FOREGROUND))
            .into(),
        None => text("unavailable")
            .size(t::TEXT_SM)
            .wrapping(text::Wrapping::None)
            .style(theme::subtle)
            .into(),
    };

    // Encoding happens on the host, so the frame rate of a session is decided
    // here and not on the machine somebody is watching from. A fast desktop
    // viewing a slow laptop gets the laptop's frame rate, which is genuinely
    // not obvious, so a machine that has dropped to the software encoder says
    // so before anybody connects to it and wonders why the picture stutters.
    let encoder: Element<'a, Message> = match state.hardware {
        None => components::pill("Checking…", Tone::Outline),
        Some(true) => components::pill("Hardware", Tone::Success),
        Some(false) => components::pill("Software", Tone::Warning),
    };

    vec![
        group(
            "Identity",
            vec![setting(
                "Device ID",
                "A fingerprint. Short enough to read aloud, and one-way, so it cannot be dialled.",
                Some(id),
            )],
        ),
        group(
            "Encoding",
            vec![setting(
                "Video encoder",
                match state.hardware {
                    Some(false) => "No hardware encoder was found, so sessions hosted here are encoded on the CPU. That is several times slower, and it is what limits the frame rate.",
                    _ => "Sessions hosted here are encoded on this machine, so its encoder sets the frame rate.",
                },
                Some(encoder),
            )],
        ),
    ]
}

/// Whether this machine comes back at boot, and the one control that changes it.
///
/// The state is drawn as the dot-and-sentence the hosting section uses for
/// something nobody sets directly, with a button on the end, because changing
/// it is not a switch: it asks Windows for administrator rights, and the
/// answer can be no. So the button says what it does, shows that it is waiting
/// while the prompt is open, and the row is only redrawn from what the service
/// manager says afterwards.
fn boot_row<'a>(service: &Service, pending: bool, elevating: bool) -> Element<'a, Message> {
    let button = |glyph: Option<&'static str>, words: &'static str| -> Option<Element<'a, Message>> {
        Some(if pending {
            components::small_button(glyph, "Waiting for permission…", None)
        } else {
            components::small_button(
                glyph,
                words,
                (!elevating).then_some(Message::ToggleBootService),
            )
        })
    };

    let (tint, headline, detail, trailing) = match service {
        Service::Installed => (
            t::SUCCESS,
            "Starts at boot",
            "Windows starts Pravera before anybody signs in, and again after every sign-out. \
             Moving this file is fine; the next start repoints the service at wherever it now is."
                .to_string(),
            button(None, "Stop starting at boot"),
        ),
        Service::NotRegistered => (
            t::ROUTE_OFFLINE,
            "Does not start at boot",
            "Windows asks for permission first, because registering a service needs administrator \
             rights. Nothing else changes."
                .to_string(),
            button(Some(icon::BOLT), "Start at boot"),
        ),
        Service::Elsewhere => (
            t::WARNING,
            "Starts at boot, but another copy",
            "The boot service runs a different Pravera than this one, so a reboot would not \
             start this file. Windows asks for permission before it is pointed here."
                .to_string(),
            button(Some(icon::BOLT), "Use this copy"),
        ),
        // The one case that shows Windows' own wording. Everything else here
        // is a sentence Pravera chose; this is a sentence Windows chose, and
        // paraphrasing it would throw away the only clue there is.
        Service::Refused(reason) => (
            t::DESTRUCTIVE,
            "Could not start at boot",
            reason.clone(),
            button(Some(icon::REFRESH), "Try again"),
        ),
        Service::Unavailable => (
            t::ROUTE_OFFLINE,
            "Does not start at boot",
            "There is no Windows service on this platform. Starting at boot is a systemd unit, \
             installed by a package rather than by Pravera."
                .to_string(),
            None,
        ),
    };
    state_row(tint, headline, detail, trailing)
}

/// What each role can do, chosen from tiles that say so.
///
/// The list and the descriptions both come from the host's own roles, so a
/// role this screen offers is always one `pravera-auth` will accept, and no
/// description can drift out of step with the permissions it describes.
///
/// The tiles are controls, not decoration, so they answer the pointer and the
/// choice: the lift under the pointer and the hold on the chosen one are both
/// animated, and the hold hands over from the old choice to the new.
fn roles<'a>(state: &'a State, now: Instant) -> Element<'a, Message> {
    let available = crate::net::host::roles();

    let mut bar = row![].spacing(t::SPACE_2);
    for (index, role) in available.iter().take(ROLE_SLOTS).enumerate() {
        let name = role.name.clone();
        let chosen = state.role_amount(&name, now);
        let hover = state.hover.amount(index, now);

        let title = row![
            text(name.clone())
                .size(t::TEXT_SM)
                .font(t::FONT_UI_STRONG)
                .wrapping(text::Wrapping::None)
                .width(Length::Fill),
            icon::stroked(icon::CHECK, 12.0, theme::faded(t::FOREGROUND, chosen)),
        ]
        .align_y(Alignment::Center);

        let tile = button(
            column![
                title,
                text(describe(role)).size(t::TEXT_XS).style(theme::muted),
            ]
            .spacing(3.0),
        )
        .width(Length::Fill)
        .padding([t::SPACE_2 + 2.0, t::SPACE_3])
        .style(move |_, status| button::Style {
            background: Some(Background::Color(if status == button::Status::Pressed {
                theme::blend(t::NEUTRAL_825, t::NEUTRAL_800, chosen)
            } else {
                theme::blend(theme::blend(t::BACKGROUND, t::NEUTRAL_850, hover), t::NEUTRAL_800, chosen)
            })),
            text_color: theme::blend(t::NEUTRAL_300, t::FOREGROUND, chosen),
            border: Border {
                color: theme::blend(
                    theme::blend(t::NEUTRAL_825, t::BEVEL_RAISED.sides, hover),
                    t::BEVEL_HOVER.top,
                    chosen,
                ),
                width: 1.0,
                radius: t::RADIUS.into(),
            },
            ..button::Style::default()
        })
        .on_press(Message::Role(name));

        bar = bar.push(hovered(index, tile.into()));
    }

    column![components::section_label("Role for a new account"), bar]
        .spacing(t::SPACE_2)
        .into()
}

/// What a role can do, in one line, read off its permissions.
///
/// Derived rather than written down beside a role name, so a role whose
/// permissions change cannot keep a description of what it used to be. What it
/// is called here is the strongest thing it holds.
fn describe(role: &pravera_auth::Role) -> String {
    use pravera_core::Permission;

    let held = role.permissions;
    if held.allows(Permission::ADMIN) {
        "Everything, including users".into()
    } else if held.allows(Permission::CONTROL) && held.allows(Permission::FILE_WRITE) {
        "Watch, control and transfer files".into()
    } else if held.allows(Permission::CONTROL) {
        "Watch and control".into()
    } else if held.allows(Permission::VIEW) {
        "Watch only".into()
    } else {
        "Nothing".into()
    }
}

// -------------------------------------------------------------------- pieces

/// The inset every row in a group shares: the same as the label over it and
/// as a switch row's own, so words line up down the whole section.
fn pad<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content)
        .padding([t::SPACE_3, t::SPACE_3])
        .width(Length::Fill)
        .into()
}

/// One setting: what it is and what it means on the left, whatever answers
/// it on the right.
fn setting<'a>(
    title: &'a str,
    detail: &'a str,
    trailing: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    let mut line = row![column![
        text(title)
            .size(t::TEXT_SM)
            .font(t::FONT_UI_MEDIUM)
            .style(theme::heading),
        text(detail).size(t::TEXT_XS).style(theme::muted),
    ]
    .spacing(3.0)
    .width(Length::Fill)]
    .spacing(t::SPACE_4)
    .align_y(Alignment::Center);
    if let Some(trailing) = trailing {
        line = line.push(trailing);
    }
    pad(line)
}

/// A state nobody sets directly: its dot, what it is, and what that means,
/// with an action on the right when there is one.
fn state_row<'a>(
    tint: iced::Color,
    headline: impl text::IntoFragment<'a>,
    detail: impl text::IntoFragment<'a>,
    trailing: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    let mut line = row![
        container(components::dot(tint, 8.0))
            .center_x(Length::Fixed(t::ICON))
            .center_y(Length::Fixed(t::ICON)),
        column![
            text(headline)
                .size(t::TEXT_SM)
                .font(t::FONT_UI_STRONG)
                .style(theme::heading),
            text(detail).size(t::TEXT_XS).style(theme::muted),
        ]
        .spacing(3.0)
        .width(Length::Fill),
    ]
    .spacing(t::SPACE_3)
    .align_y(Alignment::Center);
    if let Some(trailing) = trailing {
        line = line.push(trailing);
    }
    pad(line)
}

fn field<'a>(
    caption: &'a str,
    placeholder: &'a str,
    value: &'a str,
    secure: bool,
    on_input: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    column![
        components::section_label(caption),
        text_input(placeholder, value)
            .on_input(on_input)
            .on_submit(Message::ToggleHosting)
            .secure(secure)
            .size(t::TEXT_SM)
            .padding([t::SPACE_2, t::SPACE_3])
            .style(theme::input),
    ]
    .spacing(t::SPACE_2)
    .width(Length::Fill)
    .into()
}

/// Report enter and exit for a widget that is not itself a `mouse_area`.
fn hovered<'a>(slot: usize, element: Element<'a, Message>) -> Element<'a, Message> {
    mouse_area(element)
        .on_enter(Message::Hover(slot, true))
        .on_exit(Message::Hover(slot, false))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hosting form filled in as far as the test needs, on a machine with
    /// nothing saved.
    fn filled(username: &str, password: &str) -> State {
        State {
            username: username.to_string(),
            password: password.to_string(),
            ..State::default()
        }
    }

    #[test]
    fn choosing_a_section_shows_it_and_hands_the_highlight_over() {
        let now = Instant::now();
        let mut state = State::default();
        assert_eq!(state.section(), Section::Hosting);

        state.select(Section::Updates, now);
        assert_eq!(state.section(), Section::Updates);
        assert!(state.is_animating(now));
        // Halfway, the section left is fading out and the one chosen in.
        let mid = now + motion::STANDARD / 2;
        let left = state.nav_active[Section::Hosting.index()].value(mid);
        let chosen = state.nav_active[Section::Updates.index()].value(mid);
        assert!(left > 0.0 && left < 1.0, "{left}");
        assert!(chosen > 0.0 && chosen < 1.0, "{chosen}");
        // When it has landed only the chosen one is held.
        let done = now + motion::STANDARD + std::time::Duration::from_millis(1);
        assert_eq!(state.nav_active[Section::Hosting.index()].value(done), 0.0);
        assert_eq!(state.nav_active[Section::Updates.index()].value(done), 1.0);
    }

    #[test]
    fn choosing_the_section_already_shown_starts_nothing() {
        let now = Instant::now();
        let mut state = State::default();
        // The page's own entrance is over: nothing else should be moving.
        let later = now + motion::STANDARD * 10;
        state.select(Section::Hosting, later);
        assert!(!state.is_animating(later + motion::ENTRANCE * 4));
    }

    #[test]
    fn the_role_just_chosen_rises_as_the_one_it_replaced_falls() {
        let now = Instant::now();
        let mut state = State::default();
        state.role = "operator".into();
        let _ = update(&mut state, Message::Role("admin".into()), now);
        let mid = now + motion::STANDARD / 2;
        let rising = state.role_amount("admin", mid);
        let falling = state.role_amount("operator", mid);
        assert!(rising > 0.0 && rising < 1.0, "{rising}");
        assert!((rising + falling - 1.0).abs() < 1e-5, "{rising} + {falling}");
        assert_eq!(state.role_amount("viewer", mid), 0.0);
        let done = now + motion::STANDARD + std::time::Duration::from_millis(1);
        assert_eq!(state.role_amount("admin", done), 1.0);
        assert_eq!(state.role_amount("operator", done), 0.0);
    }

    /// A machine that already knows somebody.
    fn with_account() -> State {
        State {
            accounts: vec![Account {
                username: "operator".into(),
                role: "operator".into(),
            }],
            ..State::default()
        }
    }

    #[test]
    fn a_new_account_cannot_be_created_without_a_password() {
        // The connect code is public by design. A host with no password is
        // reachable by anyone who has seen this screen.
        assert!(!filled("operator", "").ready());
        assert!(filled("operator", "hunter2").ready());
    }

    #[test]
    fn a_new_account_cannot_be_created_without_a_username() {
        assert!(!filled("", "hunter2").ready());
        assert!(
            !filled("   ", "hunter2").ready(),
            "whitespace is not a username"
        );
    }

    #[test]
    fn a_machine_with_no_accounts_at_all_cannot_start_hosting() {
        // Nothing typed and nothing saved: there is nobody who could sign in,
        // and the button says so by being dead rather than by failing later.
        assert!(!State::default().ready());
    }

    #[test]
    fn a_machine_that_already_knows_somebody_hosts_without_typing_anything() {
        // This is the whole point of saving accounts: a machine with no
        // monitor has nobody to fill the form in.
        assert!(with_account().ready());
    }

    #[test]
    fn a_half_filled_form_does_not_quietly_host_as_somebody_else() {
        // A username typed with no password, on a machine that has saved
        // accounts, is somebody part-way through. Starting would host under an
        // account other than the one on screen.
        let mut state = with_account();
        state.username = "newcomer".into();
        assert!(!state.ready());
    }

    #[test]
    fn a_second_press_while_starting_does_nothing() {
        let mut state = filled("operator", "hunter2");
        state.began();
        assert!(!state.ready());
    }

    #[test]
    fn the_password_does_not_outlive_the_account_it_created() {
        let mut state = filled("operator", "hunter2");
        state.started();
        assert!(state.password.is_empty());
        assert!(state.username.is_empty());
    }

    #[test]
    fn the_default_role_is_not_the_one_that_can_manage_users() {
        // The role that does the job people want is `operator`. Defaulting to
        // `admin` hands out remote user management to whoever logs in.
        assert_eq!(State::default().role(), "operator");
    }

    #[test]
    fn the_default_role_is_one_the_host_will_accept() {
        // A role named here that `pravera-auth` did not know would fail at the
        // moment hosting starts, with the password already typed.
        let known: Vec<String> = crate::net::host::roles()
            .into_iter()
            .map(|role| role.name)
            .collect();

        assert!(!known.is_empty());
        assert!(
            known.iter().any(|name| name == State::default().role()),
            "the default role is not one the host knows; known: {known:?}"
        );
    }

    #[test]
    fn every_control_has_a_hover_slot_of_its_own() {
        // The role buttons and the fixed controls share one tracker, and an
        // overlap would light up the wrong control.
        assert!(
            crate::net::host::roles().len() <= ROLE_SLOTS,
            "more roles than there are slots reserved for them"
        );
        let slots = [
            SLOT_TOGGLE,
            SLOT_COPY,
            SLOT_AT_SIGN_IN,
            SLOT_HOST_AT_LAUNCH,
            SLOT_ADD_DISPLAY,
            SLOT_INSTALL_DRIVER,
        ];
        for (i, a) in slots.iter().enumerate() {
            assert!(*a >= ROLE_SLOTS, "a fixed control sits in a role's slot");
            assert!(*a < HOVER_SLOTS, "a fixed control has no slot");
            for b in &slots[i + 1..] {
                assert_ne!(a, b, "two fixed controls share a hover slot");
            }
        }
    }

    #[test]
    fn a_role_is_described_by_the_strongest_thing_it_can_do() {
        for role in crate::net::host::roles() {
            let shown = describe(&role);
            assert!(!shown.is_empty(), "{} has no description", role.name);

            if role.name == "viewer" {
                assert_eq!(shown, "Watch only");
            }
            if role.name == "admin" {
                assert!(shown.contains("users"), "{shown}");
            }
        }
    }

    #[test]
    fn a_failure_is_shown_and_does_not_leave_the_screen_pretending_to_start() {
        let mut state = State::default();
        state.began();
        state.failed("Could not open a network endpoint.".into());

        assert!(!state.is_starting());
        assert_eq!(
            state.error.as_deref(),
            Some("Could not open a network endpoint.")
        );
    }

    // -------------------------------------------- the Startup rows, headless

    use crate::headless::Screen;
    use iced::{Point, Rectangle, Size};

    fn unattended(service: Service, boot_pending: bool, elevating: bool) -> Unattended {
        Unattended {
            at_sign_in: false,
            host_at_launch: false,
            tray: true,
            displays: Some(1),
            virtual_display: None,
            capture_backend: "dxgi".to_string(),
            service,
            boot_pending,
            elevating,
        }
    }

    fn updates() -> Updates {
        Updates {
            headline: "Up to date".into(),
            detail: String::new(),
            action: UpdateAction::Check,
            auto: true,
            enabled: true,
            pending: false,
            brief: "Up to date".into(),
        }
    }

    /// What clicking across the Startup section publishes, by message.
    fn clicked_across_startup(
        state: &State,
        unattended: &Unattended,
    ) -> std::collections::BTreeMap<String, usize> {
        let now = state.arrived + std::time::Duration::from_secs(5);
        let mut screen = Screen::new(view(state, None, None, unattended, &updates(), now), Size::new(1100.0, 800.0));
        let mut seen = std::collections::BTreeMap::new();
        for (_, published) in screen.sweep(Rectangle::new(Point::new(0.0, 0.0), Size::new(1100.0, 800.0)), 6.0) {
            for message in published {
                *seen.entry(format!("{message:?}")).or_insert(0) += 1;
            }
        }
        seen
    }

    fn on_startup() -> State {
        let mut state = State::default();
        state.select(Section::Unattended, state.arrived);
        state
    }

    #[test]
    fn a_machine_that_is_not_registered_offers_a_button_that_registers_it() {
        let seen = clicked_across_startup(&on_startup(), &unattended(Service::NotRegistered, false, false));
        assert!(seen.get("ToggleBootService").copied().unwrap_or(0) > 0, "{seen:?}");
        assert!(seen.get("ToggleAtSignIn").copied().unwrap_or(0) > 0, "{seen:?}");
    }

    #[test]
    fn a_registered_machine_offers_the_same_button_to_remove_it() {
        let seen = clicked_across_startup(&on_startup(), &unattended(Service::Installed, false, false));
        assert!(seen.get("ToggleBootService").copied().unwrap_or(0) > 0, "{seen:?}");
    }

    #[test]
    fn a_service_for_another_copy_offers_to_point_it_here() {
        let seen = clicked_across_startup(&on_startup(), &unattended(Service::Elsewhere, false, false));
        assert!(seen.get("ToggleBootService").copied().unwrap_or(0) > 0, "{seen:?}");
    }

    #[test]
    fn while_windows_is_asking_the_button_waits_and_cannot_be_pressed_again() {
        let seen = clicked_across_startup(&on_startup(), &unattended(Service::NotRegistered, true, true));
        assert_eq!(seen.get("ToggleBootService"), None, "{seen:?}");
    }

    #[test]
    fn a_failure_under_the_startup_rows_can_be_seen_and_put_away() {
        let mut state = on_startup();
        state.startup_error("Windows would not remove the sign-in entry.".into());
        let seen = clicked_across_startup(&state, &unattended(Service::NotRegistered, false, false));
        assert!(seen.get("DismissStartupNote").copied().unwrap_or(0) > 0, "{seen:?}");

        assert!(update(&mut state, Message::DismissStartupNote, Instant::now()).is_none());
        assert_eq!(state.startup(), None);
    }

    #[test]
    fn a_failed_sign_in_switch_does_not_borrow_the_hosting_error() {
        // The bug: the reason went into the field the Hosting page draws, so a
        // switch on another page failed with no sign of it anywhere.
        let mut state = on_startup();
        state.startup_error("no".into());
        assert_eq!(state.error, None);
        assert!(matches!(state.startup(), Some(Startup::Error(_))));
    }
}
