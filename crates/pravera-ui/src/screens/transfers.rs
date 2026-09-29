//! Transfers: two filesystems side by side, and a ledger of what moved.
//!
//! ## Why two panes rather than a picker
//!
//! The obvious cheap answer is a "send file" button and a native file dialog.
//! It is wrong for the machine this is built for. A headless box has no
//! monitor, so the interesting direction is usually *pulling* something off it,
//! and a dialog can only push. It also cannot show you what is over there,
//! which is the question you actually have: not "where is my file" but "what is
//! on that machine, and is the build I want finished".
//!
//! So both filesystems are on screen at once, browsed the same way, and each
//! pane's action points at the other pane. The direction of a copy is never
//! inferred from which control was pressed last.
//!
//! ## The ledger
//!
//! The queue is the one place this screen commits to a look. It is set as a
//! column of figures — every row's bytes in the same monospace column, aligned
//! on the same decimal point — because that is what makes a list of transfers
//! scannable rather than a stack of progress dialogs. The bar under each row is
//! hairline and does not compete with the number; the number is the truth and
//! the bar is the glance.
//!
//! Nothing here estimates. Bytes moved and bytes promised, and no time
//! remaining: a countdown that reads "3 seconds" for a minute is worse than no
//! countdown. A rate can be derived from successive readings by whoever wants
//! to own that guess.
//!
//! ## Nothing on this screen enforces anything
//!
//! Buttons a role cannot use are not offered, which is courtesy rather than
//! security. Every request is decided again on the host, at dispatch. See
//! `pravera_host::files`.

use std::time::Instant;

use iced::widget::{button, column, container, mouse_area, row, scrollable, text, Space};
use iced::{Alignment, Background, Border, Element, Length, Padding};

use pravera_client::Progress;
use pravera_core::Permission;
use pravera_proto::{Entry, EntryKind, Listing, Location};

use crate::components::{self, hairline, Tone};
use crate::icon;
use crate::motion::{self, HoverTracker};
use crate::theme::{self, tokens as t};

/// Height of one file row.
///
/// Tighter than the device list's 40: a directory is a long list read by
/// scanning, and more rows on screen is worth more there than the extra
/// breathing room is.
const ROW: f32 = 30.0;

/// Height of one ledger row, before its bar.
const JOB_ROW: f32 = 40.0;

/// Width of the size column in the file list, and of the ledger's figures.
///
/// One number rather than two, so a file's size and the same file's progress
/// line up vertically down the screen.
const FIGURE: f32 = 96.0;

/// Hover slots: two panes of rows. The pane controls are ghost buttons that
/// light themselves.
const MAX_ROWS: usize = 512;
const SLOT_REMOTE: usize = 0;
const SLOT_LOCAL: usize = MAX_ROWS;
const HOVER_SLOTS: usize = MAX_ROWS * 2;

/// Which filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// The machine at the other end of the session.
    Remote,
    /// This one.
    Local,
}

impl Side {
    pub fn other(self) -> Side {
        match self {
            Side::Remote => Side::Local,
            Side::Local => Side::Remote,
        }
    }

    fn slot(self) -> usize {
        match self {
            Side::Remote => SLOT_REMOTE,
            Side::Local => SLOT_LOCAL,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    /// Open a row: a directory is walked into, a file is selected.
    Open(Side, usize),
    /// Go to the directory above.
    Up(Side),
    /// Go back to the list of places to start from.
    Places(Side),
    /// Re-read a directory, after something changed underneath it.
    Reload(Side),
    /// Copy the selection to the other side.
    Send(Side),
    /// Stop a transfer that is running.
    Cancel(u64),
    /// Forget the transfers that have finished.
    ClearFinished,
    /// Go back to the picture.
    Resume,
    Hover(usize, bool),
}

/// What the screen asks the application to do, because it cannot do it itself.
#[derive(Debug, Clone)]
pub enum Action {
    Browse {
        side: Side,
        location: Location,
    },
    /// Copy `path` on `from` to `directory` on the other side.
    Copy {
        from: Side,
        path: String,
        name: String,
        directory: String,
        size: u64,
    },
    Cancel(u64),
    Resume,
}

pub enum Outcome {
    Done,
    Act(Action),
}

/// How a transfer ended, or that it has not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Running,
    Done,
    /// In the words shown to the person.
    Failed(String),
    Cancelled,
}

impl JobState {
    fn is_finished(&self) -> bool {
        !matches!(self, JobState::Running)
    }
}

/// One copy, running or finished.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: u64,
    pub name: String,
    /// Which side the bytes came *from*.
    pub from: Side,
    pub moved: u64,
    pub total: u64,
    pub state: JobState,
}

impl Job {
    fn fraction(&self) -> f32 {
        match &self.state {
            JobState::Done => 1.0,
            _ => Progress {
                moved: self.moved,
                total: self.total,
            }
            .fraction()
            .unwrap_or(0.0),
        }
    }
}

/// One filesystem, as this screen is currently looking at it.
struct Pane {
    listing: Option<Listing>,
    /// The row the action acts on. An index into the current listing, cleared
    /// whenever the listing is replaced — a row number means nothing once the
    /// directory underneath it has changed.
    selected: Option<usize>,
    /// Why the last browse did not work, in the words shown.
    error: Option<String>,
    /// A browse is in flight. Shown, so a slow network share reads as working
    /// rather than as a screen that ignored a click.
    loading: bool,
    rows: Vec<iced::animation::Animation<bool>>,
}

impl Pane {
    fn new() -> Pane {
        Pane {
            listing: None,
            selected: None,
            error: None,
            loading: true,
            rows: Vec::new(),
        }
    }

    /// The directory whose contents are showing, if it is one files can be put
    /// into. `None` in the places view, which is a shortcut list rather than a
    /// directory.
    fn directory(&self) -> Option<&str> {
        self.listing.as_ref()?.path.as_deref()
    }

    fn entries(&self) -> &[Entry] {
        self.listing
            .as_ref()
            .map(|listing| listing.entries.as_slice())
            .unwrap_or_default()
    }

    /// The selected row, if one is selected and still exists.
    fn selection(&self) -> Option<&Entry> {
        self.entries().get(self.selected?)
    }

    fn arrived(&mut self, listing: Listing, now: Instant) {
        self.rows = (0..listing.entries.len())
            .map(|index| motion::entrance(false, index))
            .collect();
        for row in &mut self.rows {
            row.go_mut(true, now);
        }
        self.listing = Some(listing);
        // Deliberately cleared. A row index into the previous directory names a
        // different file in this one, and acting on it would copy something
        // nobody pointed at.
        self.selected = None;
        self.error = None;
        self.loading = false;
    }

    fn failed(&mut self, reason: String) {
        self.error = Some(reason);
        self.loading = false;
    }

    fn entrance(&self, index: usize, now: Instant) -> f32 {
        self.rows
            .get(index)
            .map(|row| row.interpolate(0.0, 1.0, now))
            .unwrap_or(1.0)
    }

    fn is_animating(&self, now: Instant) -> bool {
        self.rows.iter().any(|row| row.is_animating(now))
    }
}

/// Everything the screen is showing.
pub struct State {
    remote: Pane,
    local: Pane,
    /// The machine at the other end, for the pane heading. Empty when nothing
    /// is connected, which is also when the panes are not drawn at all.
    host_name: String,
    /// What the login may do. Used to decide which actions are offered — never
    /// to decide whether one is allowed, which is the host's job.
    permissions: Permission,
    connected: bool,
    jobs: Vec<Job>,
    hovered: HoverTracker,
    /// When the page last arrived; its panels cascade in from here.
    arrived: Instant,
}

impl Default for State {
    fn default() -> Self {
        State::new()
    }
}

impl State {
    pub fn new() -> State {
        State {
            remote: Pane::new(),
            local: Pane::new(),
            host_name: String::new(),
            permissions: Permission::empty(),
            connected: false,
            jobs: Vec::new(),
            hovered: HoverTracker::new(HOVER_SLOTS),
            arrived: Instant::now(),
        }
    }

    /// Start the page's entrance at `at`: header, panes, then the ledger.
    pub fn replay(&mut self, at: Instant) {
        self.arrived = at;
    }

    /// A session became available, or went away.
    ///
    /// Returns what to browse first. Nothing is asked for while disconnected:
    /// the panes are not on screen, and a listing nobody can see is a round
    /// trip nobody asked for.
    pub fn connected(
        &mut self,
        host_name: &str,
        permissions: Permission,
        now: Instant,
    ) -> Vec<Action> {
        let fresh = !self.connected || self.host_name != host_name;
        self.connected = true;
        self.host_name = host_name.to_owned();
        self.permissions = permissions;

        if !fresh {
            return Vec::new();
        }
        // A different machine has a different filesystem, so nothing from the
        // last one is worth keeping on screen.
        self.remote = Pane::new();
        self.local = Pane::new();
        self.hovered.clear(now);

        let mut work = vec![Action::Browse {
            side: Side::Local,
            location: Location::Places,
        }];
        if permissions.contains(Permission::FILE_READ) {
            work.push(Action::Browse {
                side: Side::Remote,
                location: Location::Places,
            });
        } else {
            self.remote
                .failed("This login may not browse this machine's files.".into());
        }
        work
    }

    /// The session ended. The queue survives it, because what moved before the
    /// connection dropped is exactly what somebody will want to check.
    pub fn disconnected(&mut self) {
        self.connected = false;
        self.host_name.clear();
        self.permissions = Permission::empty();
        self.remote = Pane::new();
        for job in &mut self.jobs {
            if job.state == JobState::Running {
                job.state = JobState::Failed("The session ended.".into());
            }
        }
    }

    /// A browse came back.
    pub fn listed(&mut self, side: Side, listing: Listing, now: Instant) {
        self.pane_mut(side).arrived(listing, now);
    }

    /// A browse did not.
    pub fn not_listed(&mut self, side: Side, reason: String) {
        self.pane_mut(side).failed(reason);
    }

    /// Add a transfer to the ledger.
    pub fn started(&mut self, id: u64, name: String, from: Side, total: u64) {
        self.jobs.push(Job {
            id,
            name,
            from,
            moved: 0,
            total,
            state: JobState::Running,
        });
    }

    /// Move a transfer's figures along.
    pub fn progressed(&mut self, id: u64, progress: Progress) {
        if let Some(job) = self.jobs.iter_mut().find(|job| job.id == id) {
            job.moved = progress.moved;
            // Trusted over the figure from the start: the host reports the
            // length it actually opened, which is the one that matters.
            job.total = progress.total;
        }
    }

    /// A transfer ended. `Ok` carries how many bytes arrived.
    ///
    /// Returns the directory to re-read, if the copy landed somewhere on
    /// screen. A file that arrived and does not appear in the list beside it
    /// reads as a failure.
    pub fn finished(&mut self, id: u64, result: Result<u64, String>) -> Option<Action> {
        let job = self.jobs.iter_mut().find(|job| job.id == id)?;
        let into = job.from.other();
        match result {
            Ok(moved) => {
                job.moved = moved;
                job.total = moved;
                job.state = JobState::Done;
            }
            Err(reason) => {
                job.state = JobState::Failed(reason);
                return None;
            }
        }

        let pane = self.pane(into);
        let location = Location::Path(pane.directory()?.to_owned());
        Some(Action::Browse {
            side: into,
            location,
        })
    }

    /// A transfer was stopped by the person.
    pub fn cancelled(&mut self, id: u64) {
        if let Some(job) = self.jobs.iter_mut().find(|job| job.id == id) {
            if job.state == JobState::Running {
                job.state = JobState::Cancelled;
            }
        }
    }

    pub fn is_animating(&self, now: Instant) -> bool {
        self.remote.is_animating(now)
            || self.local.is_animating(now)
            || self.hovered.is_animating(now)
            || motion::cascading(self.arrived, now)
            // A running transfer's bar has to be redrawn as its figures move,
            // and the figures arrive on a channel rather than from a clock.
            || self.jobs.iter().any(|job| job.state == JobState::Running)
    }

    fn pane(&self, side: Side) -> &Pane {
        match side {
            Side::Remote => &self.remote,
            Side::Local => &self.local,
        }
    }

    fn pane_mut(&mut self, side: Side) -> &mut Pane {
        match side {
            Side::Remote => &mut self.remote,
            Side::Local => &mut self.local,
        }
    }

    /// Whether this login may copy in the given direction.
    ///
    /// A hint for what to offer. The host decides again either way.
    fn may_copy(&self, from: Side) -> bool {
        match from {
            Side::Remote => self.permissions.contains(Permission::FILE_READ),
            Side::Local => self.permissions.contains(Permission::FILE_WRITE),
        }
    }
}

// ------------------------------------------------------------------- update

pub fn update(state: &mut State, message: Message, now: Instant) -> Outcome {
    match message {
        Message::Open(side, index) => {
            let Some(entry) = state.pane(side).entries().get(index).cloned() else {
                return Outcome::Done;
            };
            if !entry.kind.is_directory() {
                state.pane_mut(side).selected = Some(index);
                return Outcome::Done;
            }

            let listing = state.pane(side).listing.as_ref();
            let Some(path) = listing.map(|listing| listing.path_of(&entry)) else {
                return Outcome::Done;
            };
            state.pane_mut(side).loading = true;
            Outcome::Act(Action::Browse {
                side,
                location: Location::Path(path),
            })
        }

        Message::Up(side) => {
            let parent = state
                .pane(side)
                .listing
                .as_ref()
                .and_then(|listing| listing.parent.clone());
            let Some(parent) = parent else {
                // At a root. Up goes to the list of places rather than nowhere,
                // which is the only other thing "above this" could mean.
                return browse(state, side, Location::Places);
            };
            browse(state, side, Location::Path(parent))
        }

        Message::Places(side) => browse(state, side, Location::Places),

        Message::Reload(side) => {
            let location = match state.pane(side).directory() {
                Some(path) => Location::Path(path.to_owned()),
                None => Location::Places,
            };
            browse(state, side, location)
        }

        Message::Send(from) => {
            let Some(entry) = state.pane(from).selection().cloned() else {
                return Outcome::Done;
            };
            if !entry.kind.is_transferable() {
                return Outcome::Done;
            }
            let Some(listing) = state.pane(from).listing.as_ref() else {
                return Outcome::Done;
            };
            let path = listing.path_of(&entry);
            let Some(directory) = state.pane(from.other()).directory() else {
                return Outcome::Done;
            };
            Outcome::Act(Action::Copy {
                from,
                path,
                name: entry.name.clone(),
                directory: directory.to_owned(),
                size: entry.size,
            })
        }

        Message::Cancel(id) => Outcome::Act(Action::Cancel(id)),

        Message::ClearFinished => {
            state.jobs.retain(|job| !job.state.is_finished());
            Outcome::Done
        }

        Message::Resume => Outcome::Act(Action::Resume),

        Message::Hover(slot, entering) => {
            state.hovered.set(slot, entering, now);
            Outcome::Done
        }
    }
}

fn browse(state: &mut State, side: Side, location: Location) -> Outcome {
    state.pane_mut(side).loading = true;
    Outcome::Act(Action::Browse { side, location })
}

// --------------------------------------------------------------------- view

pub fn view<'a>(state: &'a State, now: Instant) -> Element<'a, Message> {
    let since = state.arrived;

    let mut header = components::header("Transfers");
    header = if state.connected {
        header.meta(
            row![
                components::dot(t::SUCCESS, 6.0),
                text(state.host_name.as_str())
                    .size(t::TEXT_XS)
                    .font(t::FONT_MONO)
                    .wrapping(text::Wrapping::None)
                    .style(theme::muted),
            ]
            .spacing(t::SPACE_1_5)
            .align_y(Alignment::Center),
        )
    } else {
        header.meta(components::pill("No session", Tone::Outline))
    };
    let running = state.jobs.iter().filter(|job| job.state == JobState::Running).count();
    if running > 0 {
        header = header.meta(components::pill(format!("{running} moving"), Tone::Success));
    }
    if state.connected {
        header = header.action(components::small_button(
            Some(icon::GAUGE),
            "Back to session",
            Some(Message::Resume),
        ));
    }

    // The two filesystems are two halves of the sheet, split by a hairline,
    // and the ledger is the strip along its foot.
    let top: Element<'a, Message> = if state.connected {
        row![
            pane(state, Side::Remote, &state.host_name, now),
            components::vrule(Length::Fill),
            pane(state, Side::Local, "This machine", now),
        ]
        .height(Length::Fill)
        .into()
    } else {
        nothing_connected()
    };

    components::page_footed(
        motion::settle(header, motion::cascade(since, now, 0)),
        motion::settle(top, motion::cascade(since, now, 1)),
        motion::settle(ledger(state, now), motion::cascade(since, now, 2)),
    )
}

fn nothing_connected<'a>() -> Element<'a, Message> {
    container(components::empty(
        icon::TRANSFERS,
        "No session",
        "A transfer needs a machine at the other end. Connect to one from Devices and both \
         filesystems open here side by side. Anything that moved earlier stays in the ledger.",
    ))
    .center(Length::Fill)
    .into()
}

// -------------------------------------------------------------------- panes

/// Height of a pane's heading band: which machine, and where in it.
const PANE_HEAD: f32 = 52.0;

/// Height of the column headings over a file list.
const HEADINGS: f32 = 28.0;

fn pane<'a>(state: &'a State, side: Side, title: &'a str, now: Instant) -> Element<'a, Message> {
    let pane = state.pane(side);

    // No parent and no listing both mean the same thing here: there is nowhere
    // above this, so the control is drawn dead rather than offered.
    let has_parent = pane
        .listing
        .as_ref()
        .is_some_and(|listing| listing.parent.is_some() || listing.path.is_some());
    let here = pane.directory().map(|path| tail(path, PATH_MOST)).unwrap_or_else(|| "Places".into());

    let head = container(
        row![
            components::glyph_tile(
                if side == Side::Remote { icon::DEVICES } else { icon::FOLDER },
                t::FOREGROUND,
                30.0,
            ),
            container(
                column![
                    text(title)
                        .size(t::TEXT_SM)
                        .font(t::FONT_UI_MEDIUM)
                        .wrapping(text::Wrapping::None)
                        .style(theme::heading),
                    text(here)
                        .size(t::TEXT_XS)
                        .font(t::FONT_MONO)
                        .wrapping(text::Wrapping::None)
                        .style(theme::muted),
                ]
                .spacing(1.0),
            )
            .width(Length::Fill)
            .clip(true),
            components::icon_button(icon::REFRESH, (!pane.loading).then_some(Message::Reload(side))),
            components::icon_button(icon::LEVEL_UP, has_parent.then_some(Message::Up(side))),
        ]
        .spacing(t::SPACE_3)
        .align_y(Alignment::Center),
    )
    .padding(Padding {
        top: 0.0,
        bottom: 0.0,
        left: t::SPACE_3,
        right: t::SPACE_2,
    })
    .height(Length::Fixed(PANE_HEAD))
    .center_y(Length::Fixed(PANE_HEAD));

    let headings = container(
        row![
            container(components::section_label("Name")).width(Length::Fill),
            container(components::section_label("Size"))
                .width(Length::Fixed(FIGURE))
                .align_x(Alignment::End),
        ]
        .align_y(Alignment::Center),
    )
    .padding([0.0, t::SPACE_4])
    .height(Length::Fixed(HEADINGS))
    .center_y(Length::Fixed(HEADINGS));

    let body: Element<'a, Message> = if let Some(reason) = &pane.error {
        pane_message(icon::ALERT, t::DESTRUCTIVE_TEXT, reason)
    } else if pane.loading {
        pane_message(icon::REFRESH, t::MUTED_FOREGROUND, "Reading…")
    } else if pane.entries().is_empty() {
        pane_message(icon::FOLDER, t::MUTED_FOREGROUND, "This folder is empty.")
    } else {
        rows(state, side, now)
    };

    container(
        column![
            head,
            hairline(),
            headings,
            hairline(),
            container(body).height(Length::Fill),
            hairline(),
            container(footer(state, side)).padding(t::SPACE_2),
        ]
        .clip(true),
    )
    .width(Length::FillPortion(1))
    .height(Length::Fill)
    .into()
}

/// Longest path a pane's heading shows before it keeps only the end.
const PATH_MOST: usize = 44;

/// The end of a path, which is the part that says where you are.
fn tail(path: &str, most: usize) -> String {
    let count = path.chars().count();
    if count <= most {
        return path.to_owned();
    }
    let kept: String = path.chars().skip(count - (most - 1)).collect();
    format!("…{kept}")
}

fn rows<'a>(state: &'a State, side: Side, now: Instant) -> Element<'a, Message> {
    let pane = state.pane(side);
    let mut list = column![].width(Length::Fill);

    for (index, entry) in pane.entries().iter().enumerate() {
        if index >= MAX_ROWS {
            break;
        }
        list = list.push(file_row(state, side, index, entry, now));
    }

    if let Some(listing) = &pane.listing {
        if listing.truncated {
            list = list.push(
                container(
                    text(format!(
                        "Only the first {} are shown. This directory holds more.",
                        pane.entries().len()
                    ))
                    .size(t::TEXT_XS)
                    .style(theme::tinted(t::WARNING)),
                )
                .padding([t::SPACE_2, t::SPACE_2 + 2.0]),
            );
        }
    }

    components::scroll(container(list).padding([t::SPACE_1, t::SPACE_1_5]))
}

fn file_row<'a>(
    state: &'a State,
    side: Side,
    index: usize,
    entry: &'a Entry,
    now: Instant,
) -> Element<'a, Message> {
    let pane = state.pane(side);
    let slot = side.slot() + index;
    let lift = state.hovered.amount(slot, now);
    let entered = pane.entrance(index, now);
    let selected = pane.selected == Some(index);

    let glyph = match entry.kind {
        EntryKind::Directory => icon::FOLDER,
        EntryKind::File => icon::FILE,
        EntryKind::Other => icon::FILE_OTHER,
    };
    // A directory is where you are going; a file is what you came for. The
    // brighter of the two is the one the action acts on.
    let base = if entry.kind.is_transferable() {
        t::NEUTRAL_200
    } else {
        t::NEUTRAL_300
    };
    let held = lift.max(if selected { 1.0 } else { 0.0 });
    let name_tint = theme::blend(base, t::FOREGROUND, held);

    let size: Element<'a, Message> = if entry.kind.is_directory() {
        Space::new().width(Length::Fixed(FIGURE)).into()
    } else {
        container(
            text(bytes(entry.size))
                .size(t::TEXT_XS)
                .font(t::FONT_MONO)
                .wrapping(text::Wrapping::None)
                .style(theme::tinted(theme::blend(t::SUBTLE_FOREGROUND, t::NEUTRAL_300, held))),
        )
        .width(Length::Fixed(FIGURE))
        .align_x(Alignment::End)
        .into()
    };

    let content = row![
        icon::stroked(
            glyph,
            t::ICON_SM,
            theme::blend(t::SUBTLE_FOREGROUND, t::NEUTRAL_300, held),
        ),
        container(
            text(entry.name.as_str())
                .size(t::TEXT_SM)
                .font(if selected { t::FONT_UI_MEDIUM } else { t::FONT_UI })
                .style(theme::tinted(name_tint))
                .wrapping(text::Wrapping::None),
        )
        .width(Length::Fill)
        .clip(true),
        size,
    ]
    .spacing(t::SPACE_2 + 2.0)
    .align_y(Alignment::Center);

    // Selection is a fill with an edge, the same as a selected roster row: a
    // coloured stripe on a list row reads as a status marker, and this is not
    // a status.
    let fill = if selected {
        theme::blend(t::SELECTED, t::SECONDARY, 0.6 + 0.4 * lift)
    } else {
        t::with_alpha(t::SECONDARY, 0.75 * lift)
    };

    let body = container(content)
        .padding([0.0, t::SPACE_2 + 2.0])
        .height(Length::Fixed(ROW))
        .center_y(Length::Fixed(ROW))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(fill)),
            border: Border {
                color: if selected { t::BEVEL_RAISED.sides } else { iced::Color::TRANSPARENT },
                width: if selected { 1.0 } else { 0.0 },
                radius: t::RADIUS.into(),
            },
            ..Default::default()
        });

    // A listing arriving cascades down the pane: each row fades up over the
    // card rather than growing, so nothing below it moves.
    motion::rise_on(
        mouse_area(body)
            .on_press(Message::Open(side, index))
            .on_enter(Message::Hover(slot, true))
            .on_exit(Message::Hover(slot, false))
            .interaction(iced::mouse::Interaction::Pointer),
        entered,
        4.0,
        t::CARD,
    )
}

/// The action for one pane: copy the selection to the other side.
fn footer<'a>(state: &'a State, side: Side) -> Element<'a, Message> {
    let selection = state.pane(side).selection();
    let landing = state.pane(side.other()).directory();

    // Three different reasons nothing can be sent, and each says which one it
    // is. "Send" greyed out with no explanation is the thing people click
    // twice and then give up on.
    let (label, ready): (String, bool) = match (selection, landing) {
        _ if !state.may_copy(side) => (
            match side {
                Side::Remote => "This login may not take files from here.".into(),
                Side::Local => "This login may not put files there.".into(),
            },
            false,
        ),
        (None, _) => ("Pick a file to copy it across".into(), false),
        (Some(entry), _) if !entry.kind.is_transferable() => {
            ("Only files can be copied".into(), false)
        }
        (Some(_), None) => (
            format!(
                "Open a folder on the {} first",
                match side.other() {
                    Side::Remote => "left",
                    Side::Local => "right",
                }
            ),
            false,
        ),
        (Some(entry), Some(_)) => (
            format!(
                "{} {}",
                match side {
                    Side::Remote => "Get",
                    Side::Local => "Send",
                },
                entry.name
            ),
            true,
        ),
    };

    if !ready {
        return container(
            text(label)
                .size(t::TEXT_XS)
                .style(theme::subtle)
                .wrapping(text::Wrapping::None),
        )
        .center_x(Length::Fill)
        .center_y(Length::Fixed(t::CONTROL_HEIGHT_SM))
        .clip(true)
        .into();
    }

    // The glyph points at the other pane, so the direction is legible without
    // reading the label. Remote is on the left, so taking from it points down
    // into the ledger and sending to it points up out of this one.
    let glyph = match side {
        Side::Remote => icon::DOWNLOAD,
        Side::Local => icon::UPLOAD,
    };

    components::glide(|hover| {
        button(
            container(
                row![
                    icon::stroked(glyph, t::ICON_SM, t::PRIMARY_FOREGROUND),
                    text(label)
                        .size(t::TEXT_SM)
                        .font(t::FONT_UI_MEDIUM)
                        .wrapping(text::Wrapping::None),
                ]
                .spacing(t::SPACE_2)
                .align_y(Alignment::Center),
            )
            .center_x(Length::Fill)
            .clip(true),
        )
        .padding(components::BUTTON_PADDING_SM)
        .width(Length::Fill)
        .style(theme::gliding(hover, theme::primary_button))
        .on_press(Message::Send(side))
        .into()
    })
}

/// What a pane says in place of a list: loading, empty, or why not.
fn pane_message<'a>(glyph: &'static str, tint: iced::Color, words: &'a str) -> Element<'a, Message> {
    container(
        column![
            components::glyph_tile(glyph, tint, 36.0),
            container(
                text(words)
                    .size(t::TEXT_XS)
                    .align_x(iced::alignment::Horizontal::Center)
                    .style(theme::tinted(if tint == t::MUTED_FOREGROUND {
                        t::MUTED_FOREGROUND
                    } else {
                        t::NEUTRAL_200
                    })),
            )
            .max_width(280.0),
        ]
        .spacing(t::SPACE_3)
        .align_x(Alignment::Center),
    )
    .padding(t::SPACE_4)
    .center(Length::Fill)
    .into()
}

// ------------------------------------------------------------------- ledger

/// Height of the ledger's heading band.
const LEDGER_HEAD: f32 = 36.0;

/// The most height the ledger takes before it scrolls: about five transfers.
/// The panes are the work; the ledger is the receipt.
const LEDGER_MOST: f32 = 5.0 * (JOB_ROW + 8.0) + 4.0;

fn ledger<'a>(state: &'a State, now: Instant) -> Element<'a, Message> {
    let _ = now;
    let running = state.jobs.iter().filter(|job| job.state == JobState::Running).count();
    let finished = state.jobs.len() - running;

    let summary = match (running, finished) {
        (0, 0) => String::new(),
        (0, n) => format!("{n} finished"),
        (r, 0) => format!("{r} moving"),
        (r, n) => format!("{r} moving, {n} finished"),
    };

    let clear: Element<'a, Message> = if finished > 0 {
        components::glide(|hover| {
            button(components::label(Some(icon::CLOSE), "Clear finished", t::MUTED_FOREGROUND))
                .padding(components::BUTTON_PADDING_SM)
                .style(theme::gliding(hover, theme::ghost_button))
                .on_press(Message::ClearFinished)
                .into()
        })
    } else {
        Space::new().into()
    };

    let head = container(
        row![
            components::section_label("Ledger"),
            text(summary)
                .size(t::TEXT_XS)
                .wrapping(text::Wrapping::None)
                .style(theme::subtle),
            Space::new().width(Length::Fill),
            clear,
        ]
        .spacing(t::SPACE_3)
        .align_y(Alignment::Center),
    )
    .padding(Padding {
        top: 0.0,
        bottom: 0.0,
        left: t::SPACE_4,
        right: (LEDGER_HEAD - t::CONTROL_HEIGHT_SM) / 2.0,
    })
    .height(Length::Fixed(LEDGER_HEAD))
    .center_y(Length::Fixed(LEDGER_HEAD));

    let body: Element<'a, Message> = if state.jobs.is_empty() {
        container(
            row![
                icon::stroked(icon::TRANSFERS, t::ICON_SM, t::SUBTLE_FOREGROUND),
                text("Nothing has moved yet. Pick a file on either side and copy it across.")
                    .size(t::TEXT_XS)
                    .wrapping(text::Wrapping::None)
                    .style(theme::muted),
            ]
            .spacing(t::SPACE_2 + 2.0)
            .align_y(Alignment::Center),
        )
        .padding([0.0, t::SPACE_4])
        .height(Length::Fixed(52.0))
        .center_y(Length::Fixed(52.0))
        .clip(true)
        .into()
    } else {
        let mut list = column![].width(Length::Fill);
        for (index, job) in state.jobs.iter().rev().enumerate() {
            if index > 0 {
                list = list.push(hairline());
            }
            list = list.push(job_row(job));
        }
        container(
            scrollable(list)
                .direction(scrollable::Direction::Vertical(
                    scrollable::Scrollbar::new().width(6).scroller_width(4).margin(2),
                ))
                .style(theme::scrollbar)
                .width(Length::Fill),
        )
        .max_height(LEDGER_MOST)
        .into()
    };

    column![head, hairline(), body].width(Length::Fill).into()
}

fn job_row<'a>(job: &'a Job) -> Element<'a, Message> {
    let (glyph, tint): (&'static str, iced::Color) = match &job.state {
        JobState::Running => (
            match job.from {
                Side::Remote => icon::DOWNLOAD,
                Side::Local => icon::UPLOAD,
            },
            t::FOREGROUND,
        ),
        JobState::Done => (icon::CHECK, t::SUCCESS),
        JobState::Cancelled => (icon::CLOSE, t::SUBTLE_FOREGROUND),
        JobState::Failed(_) => (icon::ALERT, t::DESTRUCTIVE_TEXT),
    };

    // What happened, in the words a person would use, and which way.
    let (said, said_tint): (String, iced::Color) = match (&job.state, job.from) {
        (JobState::Running, Side::Remote) => ("Receiving onto this machine".into(), t::MUTED_FOREGROUND),
        (JobState::Running, Side::Local) => ("Sending from this machine".into(), t::MUTED_FOREGROUND),
        (JobState::Done, Side::Remote) => ("Received onto this machine".into(), t::MUTED_FOREGROUND),
        (JobState::Done, Side::Local) => ("Sent from this machine".into(), t::MUTED_FOREGROUND),
        (JobState::Cancelled, _) => ("Stopped".into(), t::MUTED_FOREGROUND),
        (JobState::Failed(reason), _) => (reason.clone(), t::DESTRUCTIVE_TEXT),
    };

    // The figures column: every row's bytes end at the same x, so the ledger
    // reads down rather than across.
    let figure = match &job.state {
        JobState::Done => bytes(job.total),
        _ => format!("{} of {}", bytes(job.moved), bytes(job.total)),
    };

    let stop: Element<'a, Message> = if job.state == JobState::Running {
        components::icon_button(icon::CLOSE, Some(Message::Cancel(job.id)))
    } else {
        Space::new().width(Length::Fixed(t::CONTROL_HEIGHT_SM)).into()
    };

    let line = row![
        components::glyph_tile(glyph, tint, 28.0),
        container(
            column![
                text(job.name.as_str())
                    .size(t::TEXT_SM)
                    .font(t::FONT_UI_MEDIUM)
                    .wrapping(text::Wrapping::None)
                    .style(theme::heading),
                text(said)
                    .size(t::TEXT_XS)
                    .wrapping(text::Wrapping::None)
                    .style(theme::tinted(said_tint)),
            ]
            .spacing(1.0),
        )
        .width(Length::Fill)
        .clip(true),
        container(
            text(figure)
                .size(t::TEXT_XS)
                .font(t::FONT_MONO)
                .wrapping(text::Wrapping::None)
                .style(theme::tinted(t::NEUTRAL_300)),
        )
        .width(Length::Fixed(FIGURE * 2.0))
        .align_x(Alignment::End),
        stop,
    ]
    .spacing(t::SPACE_3)
    .align_y(Alignment::Center)
    .height(Length::Fixed(JOB_ROW));

    column![line, bar(job.fraction(), tint, job.state == JobState::Running)]
        .padding(Padding {
            top: 0.0,
            bottom: t::SPACE_2,
            left: t::SPACE_4,
            right: t::SPACE_2,
        })
        .into()
}

/// The hairline under a ledger row.
///
/// Two pixels, full width, and it fills rather than sliding. It is the glance;
/// the figure beside it is the answer. A thicker bar would make the number
/// decorative, which is backwards.
fn bar<'a>(fraction: f32, tint: iced::Color, running: bool) -> Element<'a, Message> {
    let filled = fraction.clamp(0.0, 1.0);
    let track = t::NEUTRAL_800;
    let lit = if running {
        tint
    } else {
        t::with_alpha(tint, 0.55)
    };

    // Two containers rather than a progress bar widget: the filled part has to
    // be a fraction of whatever width the row happens to have, and
    // `FillPortion` expresses that without knowing the pixel width.
    let left = ((filled * 1000.0) as u16).max(if filled > 0.0 { 1 } else { 0 });
    let right = 1000u16.saturating_sub(left);

    let mut line = row![].height(Length::Fixed(2.0));
    if left > 0 {
        line = line.push(
            container(Space::new())
                .width(Length::FillPortion(left))
                .height(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(Background::Color(lit)),
                    ..Default::default()
                }),
        );
    }
    if right > 0 {
        line = line.push(
            container(Space::new())
                .width(Length::FillPortion(right))
                .height(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(Background::Color(track)),
                    ..Default::default()
                }),
        );
    }
    // Inset from the stop control, so the bar ends where the figures do.
    container(line)
        .padding(Padding {
            right: t::CONTROL_HEIGHT_SM + t::SPACE_3,
            ..Padding::ZERO
        })
        .into()
}

// ------------------------------------------------------------------ helpers

/// A size a person can read at a glance.
///
/// Binary units with their proper names, because a file manager beside this one
/// will say the same number and disagreeing with it is worse than being
/// pedantic. One decimal place above a kilobyte and none below: `1.5 MiB` is
/// useful, `1536.0 B` is not.
pub fn bytes(count: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if count < 1024 {
        return format!("{count} B");
    }
    let mut size = count as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(
        path: Option<&str>,
        parent: Option<&str>,
        names: &[(&str, EntryKind, u64)],
    ) -> Listing {
        Listing {
            path: path.map(str::to_owned),
            parent: parent.map(str::to_owned),
            label: String::new(),
            entries: names
                .iter()
                .map(|(name, kind, size)| Entry {
                    name: (*name).into(),
                    path: None,
                    kind: *kind,
                    size: *size,
                    modified: None,
                    readonly: false,
                })
                .collect(),
            truncated: false,
        }
    }

    fn connected() -> (State, Instant) {
        let now = Instant::now();
        let mut state = State::new();
        state.connected("EVERCORE", Permission::all(), now);
        (state, now)
    }

    #[test]
    fn connecting_asks_for_somewhere_to_start_on_both_machines() {
        // A client cannot guess a host's drive letters, so the places view is
        // the only way in that does not need a path typed from memory.
        let mut state = State::new();
        let work = state.connected("EVERCORE", Permission::all(), Instant::now());
        assert_eq!(work.len(), 2);
        assert!(work.iter().any(|action| matches!(
            action,
            Action::Browse {
                side: Side::Local,
                location: Location::Places
            }
        )));
        assert!(work.iter().any(|action| matches!(
            action,
            Action::Browse {
                side: Side::Remote,
                location: Location::Places
            }
        )));
    }

    #[test]
    fn a_login_that_may_not_read_the_host_is_not_asked_to_browse_it() {
        // Not politeness: a listing the host will refuse is a round trip that
        // ends in an error banner, and the banner is more useful said up front.
        let mut state = State::new();
        let work = state.connected(
            "EVERCORE",
            Permission::VIEW | Permission::CONTROL,
            Instant::now(),
        );
        assert_eq!(work.len(), 1);
        assert!(matches!(
            work[0],
            Action::Browse {
                side: Side::Local,
                ..
            }
        ));
        assert!(state.remote.error.is_some());
    }

    #[test]
    fn opening_a_folder_browses_and_opening_a_file_selects() {
        let (mut state, now) = connected();
        state.listed(
            Side::Remote,
            listing(
                Some("/home"),
                Some("/"),
                &[
                    ("kim", EntryKind::Directory, 0),
                    ("a.txt", EntryKind::File, 9),
                ],
            ),
            now,
        );

        match update(&mut state, Message::Open(Side::Remote, 0), now) {
            Outcome::Act(Action::Browse { side, location }) => {
                assert_eq!(side, Side::Remote);
                assert_eq!(location, Location::Path("/home/kim".into()));
            }
            _ => panic!("a folder did not open"),
        }

        assert!(matches!(
            update(&mut state, Message::Open(Side::Remote, 1), now),
            Outcome::Done
        ));
        assert_eq!(
            state.remote.selection().map(|e| e.name.as_str()),
            Some("a.txt")
        );
    }

    #[test]
    fn a_new_listing_forgets_which_row_was_selected() {
        // Row 3 of one directory is a different file from row 3 of the next,
        // and acting on it would copy something nobody pointed at.
        let (mut state, now) = connected();
        state.listed(
            Side::Local,
            listing(Some("/a"), None, &[("one.txt", EntryKind::File, 1)]),
            now,
        );
        update(&mut state, Message::Open(Side::Local, 0), now);
        assert!(state.local.selected.is_some());

        state.listed(
            Side::Local,
            listing(Some("/b"), None, &[("two.txt", EntryKind::File, 1)]),
            now,
        );
        assert_eq!(state.local.selected, None);
    }

    #[test]
    fn copying_names_the_file_here_and_the_folder_over_there() {
        let (mut state, now) = connected();
        state.listed(
            Side::Remote,
            listing(Some("/host"), None, &[("build.zip", EntryKind::File, 4096)]),
            now,
        );
        state.listed(Side::Local, listing(Some("/mine"), None, &[]), now);
        update(&mut state, Message::Open(Side::Remote, 0), now);

        match update(&mut state, Message::Send(Side::Remote), now) {
            Outcome::Act(Action::Copy {
                from,
                path,
                name,
                directory,
                size,
            }) => {
                assert_eq!(from, Side::Remote);
                assert_eq!(path, "/host/build.zip");
                assert_eq!(name, "build.zip");
                assert_eq!(directory, "/mine");
                assert_eq!(size, 4096);
            }
            other => panic!("no copy was asked for: {}", matches!(other, Outcome::Done)),
        }
    }

    #[test]
    fn a_folder_is_never_copied() {
        // The protocol carries files. A directory would need a walk, and a
        // half-walked directory tree is worse than a refusal.
        let (mut state, now) = connected();
        state.listed(
            Side::Remote,
            listing(Some("/host"), None, &[("logs", EntryKind::Directory, 0)]),
            now,
        );
        state.listed(Side::Local, listing(Some("/mine"), None, &[]), now);
        state.remote.selected = Some(0);
        assert!(matches!(
            update(&mut state, Message::Send(Side::Remote), now),
            Outcome::Done
        ));
    }

    #[test]
    fn nothing_is_copied_into_the_places_view() {
        // Places are shortcuts, not a directory. There is nowhere to put a file.
        let (mut state, now) = connected();
        state.listed(
            Side::Remote,
            listing(Some("/host"), None, &[("a.txt", EntryKind::File, 1)]),
            now,
        );
        state.listed(Side::Local, listing(None, None, &[]), now);
        state.remote.selected = Some(0);
        assert!(matches!(
            update(&mut state, Message::Send(Side::Remote), now),
            Outcome::Done
        ));
    }

    #[test]
    fn up_from_a_root_goes_to_the_places_rather_than_nowhere() {
        let (mut state, now) = connected();
        state.listed(Side::Remote, listing(Some("C:\\"), None, &[]), now);
        match update(&mut state, Message::Up(Side::Remote), now) {
            Outcome::Act(Action::Browse { location, .. }) => {
                assert_eq!(location, Location::Places);
            }
            _ => panic!("up did nothing at a root"),
        }
    }

    #[test]
    fn a_finished_copy_re_reads_the_folder_it_landed_in() {
        // A file that arrived and does not appear in the list beside it reads
        // as a failure.
        let (mut state, now) = connected();
        state.listed(Side::Local, listing(Some("/mine"), None, &[]), now);
        state.started(1, "build.zip".into(), Side::Remote, 4096);

        let after = state.finished(1, Ok(4096));
        match after {
            Some(Action::Browse { side, location }) => {
                assert_eq!(side, Side::Local);
                assert_eq!(location, Location::Path("/mine".into()));
            }
            other => panic!("nothing was re-read: {}", other.is_none()),
        }
        assert_eq!(state.jobs[0].state, JobState::Done);
    }

    #[test]
    fn a_failed_copy_does_not_re_read_anything_and_keeps_the_reason() {
        let (mut state, now) = connected();
        state.listed(Side::Local, listing(Some("/mine"), None, &[]), now);
        state.started(1, "build.zip".into(), Side::Remote, 4096);

        assert!(state.finished(1, Err("The disk is full.".into())).is_none());
        assert_eq!(
            state.jobs[0].state,
            JobState::Failed("The disk is full.".into())
        );
    }

    #[test]
    fn a_transfer_running_when_the_session_ends_is_marked_rather_than_left_spinning() {
        // A bar that stops moving and never resolves is the worst of both:
        // it looks like work in progress and there is no work in progress.
        let (mut state, _) = connected();
        state.started(1, "big.iso".into(), Side::Remote, 1 << 30);
        state.progressed(
            1,
            Progress {
                moved: 1 << 20,
                total: 1 << 30,
            },
        );
        state.disconnected();

        assert!(state.jobs[0].state.is_finished());
        assert!(!state.connected);
        assert!(state.remote.listing.is_none());
    }

    #[test]
    fn the_ledger_survives_a_disconnection() {
        // What moved before the connection dropped is exactly what somebody
        // will want to check afterwards.
        let (mut state, _) = connected();
        state.started(1, "done.txt".into(), Side::Remote, 10);
        state.finished(1, Ok(10));
        state.disconnected();
        assert_eq!(state.jobs.len(), 1);
        assert_eq!(state.jobs[0].state, JobState::Done);
    }

    #[test]
    fn clearing_keeps_what_is_still_running() {
        let (mut state, now) = connected();
        state.started(1, "done.txt".into(), Side::Remote, 10);
        state.started(2, "running.bin".into(), Side::Local, 100);
        state.finished(1, Ok(10));

        update(&mut state, Message::ClearFinished, now);
        assert_eq!(state.jobs.len(), 1);
        assert_eq!(state.jobs[0].id, 2);
    }

    #[test]
    fn progress_takes_the_length_the_far_end_actually_opened() {
        // The size in a listing can be stale by the time the transfer starts.
        // The one reported alongside the bytes is the one being moved.
        let (mut state, _) = connected();
        state.started(1, "log.txt".into(), Side::Remote, 100);
        state.progressed(
            1,
            Progress {
                moved: 40,
                total: 400,
            },
        );
        assert_eq!(state.jobs[0].total, 400);
        assert!((state.jobs[0].fraction() - 0.1).abs() < 1e-6);
    }

    #[test]
    fn a_finished_transfer_shows_a_full_bar_even_for_an_empty_file() {
        // Nothing moved, and it worked. A bar stuck at zero next to a tick is
        // a contradiction the person has to resolve themselves.
        let mut state = State::new();
        state.started(1, "empty.txt".into(), Side::Remote, 0);
        state.finished(1, Ok(0));
        assert_eq!(state.jobs[0].fraction(), 1.0);
    }

    #[test]
    fn sizes_read_the_way_a_file_manager_beside_this_one_would() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1024), "1.0 KiB");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(bytes(4 * 1024 * 1024 * 1024), "4.0 GiB");
        // Nothing is left in a unit it has outgrown, and nothing overflows the
        // top of the table.
        assert!(bytes(u64::MAX).ends_with("TiB"));
    }

    #[test]
    fn a_long_path_keeps_the_end_that_says_where_you_are() {
        assert_eq!(tail("C:\\Users", 44), "C:\\Users");
        let long = "C:\\Users\\somebody\\Documents\\Projects\\pravera\\target\\release";
        let shown = tail(long, 24);
        assert_eq!(shown.chars().count(), 24);
        assert!(shown.starts_with('…'));
        assert!(shown.ends_with("target\\release"), "{shown}");
    }

    #[test]
    fn reconnecting_to_the_same_machine_does_not_throw_away_the_listing() {
        // The connected callback runs on every session event, not only the
        // first, and re-browsing on each one would make the panes flicker.
        let (mut state, now) = connected();
        state.listed(Side::Remote, listing(Some("/home"), None, &[]), now);
        let again = state.connected("EVERCORE", Permission::all(), now);
        assert!(again.is_empty());
        assert!(state.remote.listing.is_some());
    }

    #[test]
    fn connecting_to_a_different_machine_starts_over() {
        let (mut state, now) = connected();
        state.listed(Side::Remote, listing(Some("/home"), None, &[]), now);
        let again = state.connected("OTHER", Permission::all(), now);
        assert_eq!(again.len(), 2);
        assert!(state.remote.listing.is_none());
    }
}
