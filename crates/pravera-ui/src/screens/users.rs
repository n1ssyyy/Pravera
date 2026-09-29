//! Users: who can reach this machine, and what they may do once they are in.
//!
//! ## The screen is an audit, not a form
//!
//! The reason to open this is almost never "add somebody". It is "check". A
//! machine that runs unattended for weeks accepts whoever is listed here at
//! three in the morning with nobody watching, and the useful question is not
//! who exists but what each of them is allowed to do — including the things
//! they are *not* allowed to do, which is the half a list of granted
//! capabilities cannot show.
//!
//! So the substance of the screen is the capability grid, and it always shows
//! all ten. Six lit chips tell you nothing without the four dark ones beside
//! them; the denominator is the point. Changing a role ripples across the grid
//! left to right rather than snapping, because the thing worth noticing is
//! which cells changed.
//!
//! Adding an account is a real thing to do here and it is deliberately not the
//! loudest control on the screen.
//!
//! ## Roles are the noun, not a settings sub-page
//!
//! Permissions are held by roles and roles are held by accounts, so the two
//! lists sit side by side and a role shows how many accounts hold it. A role
//! nobody holds can be removed; one somebody holds cannot, because removing it
//! would leave that account naming nothing, taking its password, and being
//! refused with nothing on screen to explain why.
//!
//! ## What this screen cannot do
//!
//! Everything here is local to this machine. There is no remote user
//! management — the `ADMIN` permission exists in the protocol and nothing
//! implements it yet — so nothing here can be reached by a peer.

use std::time::Instant;

use iced::widget::{button, column, container, mouse_area, row, text, text_input, Space};
use iced::{Alignment, Background, Border, Element, Length};

use pravera_auth::Permission;
use pravera_host::Account;

use crate::components::{self, stat, Tone};
use crate::icon;
use crate::motion::{self, HoverTracker};
use crate::theme::{self, tokens as t};

/// Width of the left column holding the two lists.
///
/// Fixed rather than proportional: the roster is short strings and the detail
/// beside it is a grid whose column count should not change with the window.
const ROSTER: f32 = 280.0;

/// Height of one roster row.
const ROW: f32 = 32.0;

/// Height of one capability cell.
const CELL: f32 = 38.0;

/// Capability chips per row in the grid.
///
/// Two, so each chip has room for its full label. Five short columns would fit
/// the ten flags on one line and turn every label into an abbreviation, and an
/// abbreviated permission is one somebody guesses at.
const GRID_COLUMNS: usize = 2;

/// Hover slots: the roster, then the roles, then the new-role row, then the
/// capability cells.
///
/// One tracker rather than several, because a pointer is in one place at a
/// time and separate trackers would let two things be hovered at once.
const MAX_ROWS: usize = 64;
const SLOT_NEW_ROLE: usize = MAX_ROWS * 2;
const SLOT_CELLS: usize = SLOT_NEW_ROLE + 1;
const HOVER_SLOTS: usize = SLOT_CELLS + Permission::ALL.len();

#[derive(Debug, Clone)]
pub enum Message {
    Select(String),
    SelectRole(String),

    /// Move the selected account to a role.
    AssignRole(String),
    /// Turn the selected account on or off.
    ToggleEnabled,
    /// Remove the selected account. Asked for twice: see [`State::arming`].
    Remove,
    /// Take back a removal that was armed and not confirmed.
    CancelRemove,

    /// Open or close the new-account form.
    ToggleAdding,
    NewUsername(String),
    NewPassword(String),
    NewRole(String),
    CreateAccount,

    /// Toggle one capability on the role being edited.
    ToggleCapability(Permission),
    /// Open or close the new-role form.
    ToggleAddingRole,
    NewRoleName(String),
    CreateRole,
    RemoveRole(String),

    Hover(usize, bool),
}

/// What the screen is showing and what has been typed into it.
pub struct State {
    accounts: Vec<Account>,
    roles: Vec<RoleView>,
    /// The account whose detail is open, by name. A name rather than an index
    /// because the list is rebuilt from disk after every change and an index
    /// would silently come to mean a different account.
    selected: Option<String>,
    /// The role whose capabilities are open, if the detail pane is showing a
    /// role rather than an account.
    selected_role: Option<String>,

    adding: bool,
    new_username: String,
    new_password: String,
    new_role: String,

    adding_role: bool,
    new_role_name: String,
    /// Capabilities checked for a role being created or edited.
    draft: Permission,

    /// Set when removal has been asked for once. A second press does it.
    ///
    /// Not a modal. Removing an account on a machine with no monitor is not
    /// undoable — the password is gone and only a hash of it was ever stored —
    /// but a dialog for it would be the reflex answer and a worse one: it
    /// interrupts, it has to be dismissed, and people learn to dismiss it.
    /// Turning the button itself into the confirmation keeps the consequence
    /// attached to the thing that causes it.
    arming: bool,

    /// Why the last thing that was tried did not work, in the words shown.
    error: Option<String>,
    /// Per-capability, so a role change ripples across the grid.
    cells: Vec<iced::Animation<bool>>,
    hover: HoverTracker,
    /// When the page last arrived; its panels cascade in from here.
    arrived: Instant,
}

/// A role and how many accounts hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleView {
    pub name: String,
    pub permissions: Permission,
    pub builtin: bool,
    pub held_by: usize,
}

impl Default for State {
    fn default() -> Self {
        State {
            accounts: Vec::new(),
            roles: Vec::new(),
            selected: None,
            selected_role: None,
            adding: false,
            new_username: String::new(),
            new_password: String::new(),
            // The role that does the job people actually want. Not `admin`:
            // the default grant should not include approving UAC prompts.
            new_role: "operator".to_string(),
            adding_role: false,
            new_role_name: String::new(),
            draft: Permission::VIEW,
            arming: false,
            error: None,
            cells: (0..Permission::ALL.len())
                .map(|_| iced::Animation::new(false))
                .collect(),
            hover: HoverTracker::new(HOVER_SLOTS),
            arrived: Instant::now(),
        }
    }
}

impl State {
    pub fn new() -> State {
        State::default()
    }

    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }

    pub fn roles(&self) -> &[RoleView] {
        &self.roles
    }

    pub fn selected(&self) -> Option<&Account> {
        let name = self.selected.as_deref()?;
        self.accounts.iter().find(|a| a.username == name)
    }

    pub fn selected_role(&self) -> Option<&RoleView> {
        let name = self.selected_role.as_deref()?;
        self.roles.iter().find(|r| r.name == name)
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn is_adding(&self) -> bool {
        self.adding
    }

    pub fn new_username(&self) -> &str {
        &self.new_username
    }

    pub fn new_password(&self) -> &str {
        &self.new_password
    }

    pub fn new_role(&self) -> &str {
        &self.new_role
    }

    pub fn is_adding_role(&self) -> bool {
        self.adding_role
    }

    pub fn new_role_name(&self) -> &str {
        &self.new_role_name
    }

    pub fn draft(&self) -> Permission {
        self.draft
    }

    pub fn is_arming(&self) -> bool {
        self.arming
    }

    /// Whether the new-account form describes something that could be saved.
    pub fn can_create(&self) -> bool {
        !self.new_username.trim().is_empty() && !self.new_password.is_empty()
    }

    /// Replace what is shown with what is on disk.
    ///
    /// Called after every change rather than mutating the copy in memory, so
    /// the screen can never show an account the store refused to write.
    pub fn load(&mut self, accounts: Vec<Account>, roles: Vec<RoleView>, now: Instant) {
        // A selection that no longer names anything falls back to the first
        // account, so the detail pane is never blank next to a populated list.
        if !self
            .selected
            .as_deref()
            .is_some_and(|name| accounts.iter().any(|a| a.username == name))
        {
            self.selected = accounts.first().map(|a| a.username.clone());
        }
        if !self
            .selected_role
            .as_deref()
            .is_some_and(|name| roles.iter().any(|r| r.name == name))
        {
            self.selected_role = None;
        }

        self.hover.resize(HOVER_SLOTS, now);

        self.accounts = accounts;
        self.roles = roles;
        self.arming = false;
        self.sync_cells(now);
    }

    /// Point every capability cell at what is currently granted.
    fn sync_cells(&mut self, now: Instant) {
        let held = self.showing_permissions();
        for (index, flag) in Permission::ALL.iter().enumerate() {
            let lit = held.contains(*flag);
            if self.cells[index].value() != lit {
                self.cells[index].go_mut(lit, now);
            }
        }
    }

    /// The permission set the grid is drawing.
    ///
    /// The order matches what the detail pane shows, so the grid is always
    /// describing the thing above it rather than something behind it.
    pub fn showing_permissions(&self) -> Permission {
        if self.adding {
            return self.granted_by(&self.new_role);
        }
        if self.selected_role.is_some() {
            return self.draft;
        }
        self.selected()
            .map(|account| account.permissions)
            .unwrap_or_else(Permission::empty)
    }

    /// What a named role grants. An unknown name grants nothing, matching the
    /// host, which refuses a session rather than guessing at a missing role.
    fn granted_by(&self, name: &str) -> Permission {
        self.roles
            .iter()
            .find(|role| role.name == name)
            .map(|role| role.permissions)
            .unwrap_or_else(Permission::empty)
    }

    /// How lit a capability cell is, 0 to 1.
    pub fn cell(&self, index: usize, now: Instant) -> f32 {
        self.cells
            .get(index)
            .map(|cell| cell.interpolate(0.0, 1.0, now))
            .unwrap_or(0.0)
    }

    pub fn hovered(&self, slot: usize, now: Instant) -> f32 {
        self.hover.amount(slot, now)
    }

    pub fn is_animating(&self, now: Instant) -> bool {
        self.hover.is_animating(now)
            || self.cells.iter().any(|a| a.is_animating(now))
            || motion::cascading(self.arrived, now)
    }

    /// Start the page's entrance at `at`: header, lists, then the detail.
    pub fn replay(&mut self, at: Instant) {
        self.arrived = at;
    }

    pub fn failed(&mut self, reason: String) {
        self.error = Some(reason);
    }

    /// The account the screen is acting on, for the app to pass to the store.
    pub fn target(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    pub fn finished_adding(&mut self) {
        self.adding = false;
        self.new_username.clear();
        self.new_password.clear();
        self.error = None;
    }

    pub fn finished_adding_role(&mut self) {
        self.adding_role = false;
        self.new_role_name.clear();
        self.error = None;
    }
}

/// A message the screen handled itself, or one the app has to act on.
///
/// Anything that touches the account file leaves here: this screen renders and
/// remembers what was typed, and the store is reached through the app so that
/// one place decides what happens when a write fails.
pub enum Outcome {
    /// Handled here. Nothing further to do.
    Done,
    /// The app should carry this out against the store and call [`State::load`].
    Act(Action),
}

/// Something to be done to the account store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Create {
        username: String,
        password: String,
        role: String,
    },
    Assign {
        username: String,
        role: String,
    },
    SetEnabled {
        username: String,
        enabled: bool,
    },
    Remove {
        username: String,
    },
    /// Add a role that did not exist. Separate from [`Action::DefineRole`] even
    /// though both write the same thing, because only this one means the
    /// new-role field has done its job and should be put away — editing a
    /// capability must not close the form somebody is still typing in.
    CreateRole {
        name: String,
    },
    DefineRole {
        name: String,
        permissions: Permission,
    },
    RemoveRole {
        name: String,
    },
}

pub fn update(state: &mut State, message: Message, now: Instant) -> Outcome {
    // Any deliberate action cancels an armed removal. Arming persists only for
    // as long as the next thing you do is press the same button again.
    let disarms = !matches!(message, Message::Remove | Message::Hover(..));
    if disarms {
        state.arming = false;
    }

    match message {
        // Both of these take over the detail pane, so an open new-account form
        // closes rather than sitting behind whatever was clicked. What was
        // typed is kept: reopening the form gets it back.
        Message::Select(username) => {
            state.selected = Some(username);
            state.selected_role = None;
            state.adding = false;
            state.error = None;
            state.sync_cells(now);
            Outcome::Done
        }
        Message::SelectRole(name) => {
            state.draft = state.granted_by(&name);
            state.selected_role = Some(name);
            state.adding = false;
            state.error = None;
            state.sync_cells(now);
            Outcome::Done
        }
        Message::AssignRole(role) => match state.selected.clone() {
            Some(username) => Outcome::Act(Action::Assign { username, role }),
            None => Outcome::Done,
        },
        Message::ToggleEnabled => match state.selected() {
            Some(account) => Outcome::Act(Action::SetEnabled {
                username: account.username.clone(),
                enabled: !account.enabled,
            }),
            None => Outcome::Done,
        },
        Message::Remove => {
            let Some(username) = state.selected.clone() else {
                return Outcome::Done;
            };
            if !state.arming {
                state.arming = true;
                return Outcome::Done;
            }
            state.arming = false;
            Outcome::Act(Action::Remove { username })
        }
        Message::CancelRemove => {
            state.arming = false;
            Outcome::Done
        }

        Message::ToggleAdding => {
            state.adding = !state.adding;
            state.error = None;
            state.sync_cells(now);
            Outcome::Done
        }
        Message::NewUsername(username) => {
            state.new_username = username;
            state.error = None;
            Outcome::Done
        }
        Message::NewPassword(password) => {
            state.new_password = password;
            state.error = None;
            Outcome::Done
        }
        Message::NewRole(role) => {
            state.new_role = role;
            state.sync_cells(now);
            Outcome::Done
        }
        Message::CreateAccount => {
            if !state.can_create() {
                return Outcome::Done;
            }
            Outcome::Act(Action::Create {
                username: state.new_username.trim().to_string(),
                password: state.new_password.clone(),
                role: state.new_role.clone(),
            })
        }

        Message::ToggleCapability(flag) => {
            // Only a role's capabilities are editable. An account's are
            // whatever its role grants, which is the point of having roles.
            let Some(name) = state.selected_role.clone() else {
                return Outcome::Done;
            };
            state.draft.toggle(flag);
            state.sync_cells(now);
            Outcome::Act(Action::DefineRole {
                name,
                permissions: state.draft,
            })
        }
        Message::ToggleAddingRole => {
            state.adding_role = !state.adding_role;
            state.error = None;
            Outcome::Done
        }
        Message::NewRoleName(name) => {
            state.new_role_name = name;
            state.error = None;
            Outcome::Done
        }
        Message::CreateRole => {
            let name = state.new_role_name.trim().to_string();
            if name.is_empty() {
                return Outcome::Done;
            }
            Outcome::Act(Action::CreateRole { name })
        }
        Message::RemoveRole(name) => Outcome::Act(Action::RemoveRole { name }),

        Message::Hover(slot, entering) => {
            state.hover.set(slot, entering, now);
            Outcome::Done
        }
    }
}

// ------------------------------------------------------------------- drawing

pub fn view<'a>(state: &'a State, now: Instant) -> Element<'a, Message> {
    let since = state.arrived;
    let header = motion::rise(header(state), motion::cascade(since, now, 0));

    // With nothing to list, the two lists are two empty boxes. Say the thing
    // instead; the form takes its place once it is asked for.
    if state.accounts().is_empty() && !state.is_adding() {
        return components::page(header, motion::rise(nobody(), motion::cascade(since, now, 1)));
    }

    components::page(
        header,
        row![
            motion::rise(
                container(lists(state, now))
                    .width(Length::Fixed(ROSTER))
                    .height(Length::Fill),
                motion::cascade(since, now, 1),
            ),
            motion::rise(detail(state, now), motion::cascade(since, now, 2)),
        ]
        .spacing(t::GAP)
        .height(Length::Fill),
    )
}

fn header<'a>(state: &'a State) -> Element<'a, Message> {
    let total = state.accounts().len();
    let reachable = state
        .accounts()
        .iter()
        .filter(|account| account.can_connect())
        .count();
    let disabled = state
        .accounts()
        .iter()
        .filter(|account| !account.enabled)
        .count();

    let mut ledger = row![].spacing(t::SPACE_2).align_y(Alignment::Center);
    ledger = ledger.push(stat::figure(total, "accounts", t::FOREGROUND));
    ledger = ledger.push(stat::separator());
    ledger = ledger.push(stat::figure(
        reachable,
        "can connect",
        if reachable > 0 {
            t::SUCCESS
        } else {
            t::DESTRUCTIVE_TEXT
        },
    ));
    if disabled > 0 {
        ledger = ledger.push(stat::separator());
        ledger = ledger.push(stat::figure(disabled, "disabled", t::MUTED_FOREGROUND));
    }

    let open = state.is_adding();
    components::header("Users")
        .meta(ledger)
        .action(components::small_button(
            Some(if open { icon::CLOSE } else { icon::PLUS }),
            if open { "Cancel" } else { "Add account" },
            Some(Message::ToggleAdding),
        ))
        .into()
}

/// The state that matters most on this screen, said plainly, with the one
/// thing that fixes it right under it.
fn nobody<'a>() -> Element<'a, Message> {
    components::panel(
        container(
            components::empty(
                icon::USERS,
                "Nobody can connect to this machine",
                "Pravera has its own accounts, separate from the ones you sign in to Windows or \
                 Linux with. Until one exists here, every connection is refused — including yours.",
            )
            .push(Space::new().height(t::SPACE_2))
            .push(components::primary_button(
                Some(icon::PLUS),
                "Add account",
                Some(Message::ToggleAdding),
            )),
        )
        .center(Length::Fill),
    )
    .padding(0)
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

// --------------------------------------------------------------------- lists

/// Accounts over roles, in one card down the left: the two nouns of the
/// screen, each with its count, each row opening its detail on the right.
fn lists<'a>(state: &'a State, now: Instant) -> Element<'a, Message> {
    // A highlighted row while the pane shows a role or a half-typed new account
    // would point at something that is not on screen.
    let showing_account = !state.is_adding() && state.selected_role().is_none();

    let mut accounts = column![].spacing(2.0);
    if state.accounts().is_empty() {
        accounts = accounts.push(
            container(text("No accounts yet").size(t::TEXT_XS).style(theme::subtle))
                .padding([t::SPACE_1_5, t::SPACE_2 + 2.0]),
        );
    }
    for (index, account) in state.accounts().iter().enumerate() {
        accounts = accounts.push(roster_row(
            index,
            account,
            showing_account && state.target() == Some(account.username.as_str()),
            state.hovered(index.min(MAX_ROWS - 1), now),
        ));
    }

    let mut roles = column![].spacing(2.0);
    for (index, role) in state.roles().iter().enumerate() {
        let slot = (MAX_ROWS + index).min(MAX_ROWS * 2 - 1);
        roles = roles.push(role_row(
            slot,
            role,
            state.selected_role().map(|r| r.name.as_str()) == Some(role.name.as_str()),
            state.hovered(slot, now),
        ));
    }
    roles = roles.push(new_role_row(state, now));

    components::body_with(
        column![
            list_head("Accounts", state.accounts().len()),
            accounts,
            Space::new().height(t::SPACE_4),
            list_head("Roles", state.roles().len()),
            roles,
        ]
        .spacing(t::SPACE_1),
        [t::SPACE_3, t::SPACE_2],
    )
}

fn list_head<'a>(caption: &str, count: usize) -> Element<'a, Message> {
    container(components::section_head(
        caption,
        text(count.to_string())
            .size(t::TEXT_XS)
            .wrapping(text::Wrapping::None)
            .style(theme::subtle),
    ))
    .padding([0.0, t::SPACE_2 + 2.0])
    .into()
}

fn roster_row<'a>(index: usize, account: &'a Account, selected: bool, hover: f32) -> Element<'a, Message> {
    // The dot is the whole state of the account in one mark: granted, disabled,
    // or holding a role that grants nothing.
    let (tint, meaning) = if !account.enabled {
        (t::ROUTE_OFFLINE, "disabled")
    } else if !account.permissions.is_usable_session() {
        (t::DESTRUCTIVE_TEXT, "no access")
    } else {
        (t::SUCCESS, "")
    };

    let name_tint = theme::blend(
        if selected { t::FOREGROUND } else { t::NEUTRAL_300 },
        t::FOREGROUND,
        hover,
    );

    let trailing: Element<'a, Message> = if meaning.is_empty() {
        text(account.role.clone())
            .size(t::TEXT_XS)
            .wrapping(text::Wrapping::None)
            .style(theme::subtle)
            .into()
    } else {
        text(meaning)
            .size(t::TEXT_XS)
            .wrapping(text::Wrapping::None)
            .style(theme::tinted(tint))
            .into()
    };

    let body = row![
        container(components::dot(tint, 7.0))
            .center_x(Length::Fixed(t::ICON))
            .center_y(Length::Fixed(t::ICON)),
        text(account.username.clone())
            .size(t::TEXT_SM)
            .font(if selected { t::FONT_UI_MEDIUM } else { t::FONT_UI })
            .wrapping(text::Wrapping::None)
            .style(theme::tinted(name_tint))
            .width(Length::Fill),
        trailing,
    ]
    .spacing(t::SPACE_2 + 2.0)
    .align_y(Alignment::Center);

    mouse_area(components::list_row(body, selected, hover, ROW))
        .on_enter(Message::Hover(index.min(MAX_ROWS - 1), true))
        .on_exit(Message::Hover(index.min(MAX_ROWS - 1), false))
        .on_press(Message::Select(account.username.clone()))
        .interaction(iced::mouse::Interaction::Pointer)
        .into()
}

fn role_row<'a>(slot: usize, role: &'a RoleView, selected: bool, hover: f32) -> Element<'a, Message> {
    let granted = role.permissions.iter().count();

    let name_tint = theme::blend(
        if selected { t::FOREGROUND } else { t::NEUTRAL_300 },
        t::FOREGROUND,
        hover,
    );

    // How many capabilities out of how many there are. The denominator is the
    // information: "6" alone says nothing about how much is being withheld.
    let count = format!("{granted}/{}", Permission::ALL.len());

    let body = row![
        icon::stroked(
            if role.builtin { icon::SHIELD } else { icon::SHIELD_CHECK },
            t::ICON_SM,
            theme::blend(t::SUBTLE_FOREGROUND, t::FOREGROUND, if selected { 1.0 } else { hover }),
        ),
        text(role.name.clone())
            .size(t::TEXT_SM)
            .font(if selected { t::FONT_UI_MEDIUM } else { t::FONT_UI })
            .wrapping(text::Wrapping::None)
            .style(theme::tinted(name_tint))
            .width(Length::Fill),
        text(count)
            .size(t::TEXT_XS)
            .font(t::FONT_MONO)
            .wrapping(text::Wrapping::None)
            .style(theme::subtle),
    ]
    .spacing(t::SPACE_2 + 2.0)
    .align_y(Alignment::Center);

    mouse_area(components::list_row(body, selected, hover, ROW))
        .on_enter(Message::Hover(slot, true))
        .on_exit(Message::Hover(slot, false))
        .on_press(Message::SelectRole(role.name.clone()))
        .interaction(iced::mouse::Interaction::Pointer)
        .into()
}

fn new_role_row<'a>(state: &'a State, now: Instant) -> Element<'a, Message> {
    if !state.is_adding_role() {
        let hover = state.hovered(SLOT_NEW_ROLE, now);
        let tint = theme::blend(t::SUBTLE_FOREGROUND, t::FOREGROUND, hover);
        return mouse_area(components::list_row(
            row![
                icon::stroked(icon::PLUS, t::ICON_SM, tint),
                text("New role").size(t::TEXT_SM).style(theme::tinted(tint)),
            ]
            .spacing(t::SPACE_2 + 2.0)
            .align_y(Alignment::Center),
            false,
            hover,
            ROW,
        ))
        .on_enter(Message::Hover(SLOT_NEW_ROLE, true))
        .on_exit(Message::Hover(SLOT_NEW_ROLE, false))
        .on_press(Message::ToggleAddingRole)
        .interaction(iced::mouse::Interaction::Pointer)
        .into();
    }

    container(
        row![
            text_input("role name", state.new_role_name())
                .on_input(Message::NewRoleName)
                .on_submit(Message::CreateRole)
                .size(t::TEXT_SM)
                .padding([5.0, t::SPACE_2 + 2.0])
                .style(theme::input),
            components::small_button(None, "Add", Some(Message::CreateRole)),
            components::icon_button(icon::CLOSE, Some(Message::ToggleAddingRole)),
        ]
        .spacing(t::SPACE_1_5)
        .align_y(Alignment::Center),
    )
    .padding([t::SPACE_1, 0.0])
    .width(Length::Fill)
    .into()
}

// -------------------------------------------------------------------- detail

/// Whatever is being worked on: an account, a role, or a new account. A new
/// account is something being worked on, so it takes the same pane; opening
/// it in a band above the lists would push the roster down the screen at the
/// moment somebody most wants to see it.
fn detail<'a>(state: &'a State, now: Instant) -> Element<'a, Message> {
    let content: Element<'a, Message> = if state.is_adding() {
        new_account(state, now)
    } else if let Some(role) = state.selected_role() {
        role_detail(state, role, now)
    } else if let Some(account) = state.selected() {
        account_detail(state, account, now)
    } else {
        Space::new().into()
    };

    let mut body = column![].spacing(t::SPACE_4).width(Length::Fill);
    if let Some(error) = state.error() {
        body = body.push(components::callout(icon::ALERT, error, Tone::Danger));
    }
    components::reading_body(body.push(content))
}

fn account_detail<'a>(state: &'a State, account: &'a Account, now: Instant) -> Element<'a, Message> {
    let (status, tone) = if !account.enabled {
        ("Disabled", Tone::Neutral)
    } else if account.can_connect() {
        ("Can connect", Tone::Success)
    } else {
        ("Cannot connect", Tone::Danger)
    };
    let granted = account.permissions.iter().count();

    let lead = row![
        components::avatar(&account.username, 40.0),
        column![
            text(account.username.clone())
                .size(t::TEXT_BASE)
                .font(t::FONT_UI_STRONG)
                .wrapping(text::Wrapping::None)
                .style(theme::heading),
            text(format!(
                "{} role, {granted} of {} capabilities",
                account.role,
                Permission::ALL.len()
            ))
            .size(t::TEXT_XS)
            .style(theme::muted),
        ]
        .spacing(2.0)
        .width(Length::Fill),
        components::pill(status, tone),
    ]
    .spacing(t::SPACE_3)
    .align_y(Alignment::Center);

    let mut body = column![lead, components::hairline()].spacing(t::SPACE_5);

    // Why an account that looks fine cannot get in. Said here rather than
    // left to be inferred from a grid of dark cells.
    if account.enabled && !account.permissions.is_usable_session() {
        body = body.push(components::callout(
            icon::ALERT,
            "This role grants nothing a session can use, so every sign-in is refused. Pick a role that includes View screen.",
            Tone::Warning,
        ));
    }

    body.push(components::section(
        "Role",
        role_choice(state.roles(), &account.role, Message::AssignRole),
    ))
    .push(capabilities(state, now, false))
    .push(components::hairline())
    .push(account_actions(state, account))
    .into()
}

fn role_detail<'a>(state: &'a State, role: &'a RoleView, now: Instant) -> Element<'a, Message> {
    let held = match role.held_by {
        0 => "Held by nobody".to_string(),
        1 => "Held by 1 account".to_string(),
        count => format!("Held by {count} accounts"),
    };

    let lead = row![
        components::glyph_tile(
            if role.builtin { icon::SHIELD } else { icon::SHIELD_CHECK },
            t::FOREGROUND,
            40.0,
        ),
        column![
            text(role.name.clone())
                .size(t::TEXT_BASE)
                .font(t::FONT_UI_STRONG)
                .wrapping(text::Wrapping::None)
                .style(theme::heading),
            text(held).size(t::TEXT_XS).style(theme::muted),
        ]
        .spacing(2.0)
        .width(Length::Fill),
        if role.builtin {
            components::pill("Built in", Tone::Neutral)
        } else {
            components::pill("Custom", Tone::Outline)
        },
    ]
    .spacing(t::SPACE_3)
    .align_y(Alignment::Center);

    let mut body = column![
        lead,
        components::hairline(),
        components::note(if role.builtin {
            "Built-in roles are defined by Pravera and are the same on every machine, so they \
             cannot be edited. Make a role of your own to grant something different."
        } else {
            "Flip a capability to grant or withhold it. Every account holding this role changes \
             with it, the next time one of them signs in."
        }),
        capabilities(state, now, !role.builtin),
    ]
    .spacing(t::SPACE_5);

    if !role.builtin {
        body = body.push(components::hairline()).push(role_actions(role));
    }

    body.into()
}

/// The grid. All ten, always: six lit cells say nothing about how much is
/// withheld without the four dark ones beside them.
fn capabilities<'a>(state: &'a State, now: Instant, editable: bool) -> Element<'a, Message> {
    let held = state.showing_permissions().iter().count();

    let mut grid = column![].spacing(t::SPACE_2);
    let mut line = row![].spacing(t::SPACE_2);
    for (index, flag) in Permission::ALL.iter().enumerate() {
        let slot = SLOT_CELLS + index;
        line = line.push(capability(
            *flag,
            state.cell(index, now),
            editable,
            state.hovered(slot, now),
            slot,
        ));
        if (index + 1) % GRID_COLUMNS == 0 {
            grid = grid.push(line);
            line = row![].spacing(t::SPACE_2);
        }
    }

    // An odd number of flags would otherwise leave the last one stretched
    // across the full width, which reads as emphasis rather than as a leftover.
    let short = Permission::ALL.len() % GRID_COLUMNS;
    if short != 0 {
        for _ in short..GRID_COLUMNS {
            line = line.push(Space::new().width(Length::Fill));
        }
        grid = grid.push(line);
    }

    column![
        components::section_head(
            "Capabilities",
            text(format!("{held} of {} granted", Permission::ALL.len()))
                .size(t::TEXT_XS)
                .wrapping(text::Wrapping::None)
                .style(theme::subtle),
        ),
        grid,
    ]
    .spacing(t::SPACE_3)
    .into()
}

/// One capability. Granted cells are raised and marked; withheld ones are
/// sunk into the card, their words still readable — the withheld half is
/// exactly as important as the granted one. On a role that can be edited,
/// the mark is a switch and the whole cell flips it.
fn capability<'a>(flag: Permission, lit: f32, editable: bool, hover: f32, slot: usize) -> Element<'a, Message> {
    // The two that are different in kind get a warning colour when held.
    // `ELEVATE` means approving UAC prompts and `ADMIN` means changing this
    // screen from the other end; both are worth spotting in a grid you skim.
    let held_tint = if flag == Permission::ADMIN || flag == Permission::ELEVATE {
        t::WARNING
    } else {
        t::SUCCESS
    };
    let words = text(flag.label())
        .size(t::TEXT_XS + 1.0)
        .wrapping(text::Wrapping::None)
        .width(Length::Fill)
        .style(theme::tinted(theme::blend(t::NEUTRAL_400, t::NEUTRAL_100, lit)));

    let body: Element<'a, Message> = if editable {
        row![words, components::switch(lit, hover)]
            .spacing(t::SPACE_3)
            .align_y(Alignment::Center)
            .into()
    } else {
        let mark = container(icon::stroked(icon::CHECK, 10.0, theme::faded(held_tint, lit)))
            .center_x(Length::Fixed(t::ICON))
            .center_y(Length::Fixed(t::ICON))
            .style(move |_| container::Style {
                background: Some(Background::Color(t::with_alpha(held_tint, 0.16 * lit))),
                border: Border {
                    color: theme::blend(t::NEUTRAL_700, t::with_alpha(held_tint, 0.7), lit),
                    width: 1.0,
                    radius: t::RADIUS_FULL.into(),
                },
                ..container::Style::default()
            });
        row![mark, words]
            .spacing(t::SPACE_2 + 2.0)
            .align_y(Alignment::Center)
            .into()
    };

    let edge = theme::blend(
        theme::blend(t::NEUTRAL_825, t::BEVEL_RAISED.sides, lit),
        t::BEVEL_HOVER.top,
        hover * 0.6,
    );
    let cell = container(body)
        .padding([0.0, t::SPACE_3])
        .height(Length::Fixed(CELL))
        .center_y(Length::Fixed(CELL))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(theme::blend(t::BACKGROUND, t::ROW, lit))),
            border: Border {
                color: edge,
                width: 1.0,
                radius: t::RADIUS.into(),
            },
            ..container::Style::default()
        });

    if !editable {
        return cell.into();
    }

    mouse_area(cell)
        .on_press(Message::ToggleCapability(flag))
        .on_enter(Message::Hover(slot, true))
        .on_exit(Message::Hover(slot, false))
        .interaction(iced::mouse::Interaction::Pointer)
        .into()
}

fn account_actions<'a>(state: &'a State, account: &'a Account) -> Element<'a, Message> {
    let (toggle_label, toggle_icon) = if account.enabled {
        ("Disable", icon::DISABLED)
    } else {
        ("Enable", icon::USER)
    };

    let arming = state.is_arming();
    let remove = button(components::label(
        Some(icon::TRASH),
        if arming { "Remove for good" } else { "Remove" },
        if arming {
            t::DESTRUCTIVE_FOREGROUND
        } else {
            t::DESTRUCTIVE_TEXT
        },
    ))
    .padding(components::BUTTON_PADDING_SM)
    .style(if arming {
        theme::destructive_button
    } else {
        theme::danger_ghost_button
    })
    .on_press(Message::Remove);

    let mut line = row![
        components::small_button(Some(toggle_icon), toggle_label, Some(Message::ToggleEnabled)),
        Space::new().width(Length::Fill),
    ]
    .spacing(t::SPACE_2)
    .align_y(Alignment::Center);
    // Armed, the way back is right beside the way through.
    if arming {
        line = line.push(
            button(components::label(None, "Keep", t::FOREGROUND))
                .padding(components::BUTTON_PADDING_SM)
                .style(theme::ghost_button)
                .on_press(Message::CancelRemove),
        );
    }
    line.push(remove).into()
}

fn role_actions<'a>(role: &'a RoleView) -> Element<'a, Message> {
    // A role somebody holds cannot be removed, and the screen says why rather
    // than greying the button out with no explanation.
    if role.held_by > 0 {
        return components::note(format!(
            "Move the {} account{} holding this role somewhere else before removing it.",
            role.held_by,
            if role.held_by == 1 { "" } else { "s" }
        ));
    }

    row![
        Space::new().width(Length::Fill),
        button(components::label(Some(icon::TRASH), "Remove role", t::DESTRUCTIVE_TEXT))
            .padding(components::BUTTON_PADDING_SM)
            .style(theme::danger_ghost_button)
            .on_press(Message::RemoveRole(role.name.clone())),
    ]
    .align_y(Alignment::Center)
    .into()
}

// ---------------------------------------------------------------- new account

fn new_account<'a>(state: &'a State, now: Instant) -> Element<'a, Message> {
    column![
        components::hero(
            icon::USER,
            t::FOREGROUND,
            "New account",
            "A name and password of its own, separate from this machine's sign-in.",
        ),
        components::hairline(),
        row![
            field("Username", "username", state.new_username(), false, Message::NewUsername),
            field("Password", "password", state.new_password(), true, Message::NewPassword),
        ]
        .spacing(t::SPACE_4),
        components::section("Role", role_choice(state.roles(), state.new_role(), Message::NewRole)),
        // What the choice above actually buys, before it is made rather than
        // after. The same ten cells the rest of the screen uses.
        capabilities(state, now, false),
        components::callout(
            icon::LOCK,
            "The password is hashed with Argon2id and only the hash is written down. Nothing on this machine can tell you what it was, so keep it somewhere.",
            Tone::Info,
        ),
        row![
            Space::new().width(Length::Fill),
            components::small_button(None, "Cancel", Some(Message::ToggleAdding)),
            components::primary_button(
                Some(icon::CHECK),
                "Create account",
                state.can_create().then_some(Message::CreateAccount),
            ),
        ]
        .spacing(t::SPACE_2)
        .align_y(Alignment::Center),
    ]
    .spacing(t::SPACE_5)
    .into()
}

/// The roles as one segmented bar, the current one held.
///
/// Shared between assigning a role to an existing account and choosing one
/// for a new account, because those are the same decision and drifting apart
/// would make the second look like a different kind of thing.
fn role_choice<'a>(
    roles: &'a [RoleView],
    current: &str,
    on_pick: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    components::segmented(
        roles
            .iter()
            .map(|role| (role.name.clone(), role.name == current, on_pick(role.name.clone())))
            .collect(),
    )
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
            .on_submit(Message::CreateAccount)
            .secure(secure)
            .size(t::TEXT_SM)
            .padding([t::SPACE_2, t::SPACE_3])
            .style(theme::input),
    ]
    .spacing(t::SPACE_2)
    .width(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(username: &str, role: &str, enabled: bool, permissions: Permission) -> Account {
        Account {
            username: username.into(),
            role: role.into(),
            enabled,
            permissions,
        }
    }

    fn role(name: &str, permissions: Permission, builtin: bool, held_by: usize) -> RoleView {
        RoleView {
            name: name.into(),
            permissions,
            builtin,
            held_by,
        }
    }

    fn loaded(now: Instant) -> State {
        let mut state = State::new();
        state.load(
            vec![
                account(
                    "ops",
                    "operator",
                    true,
                    Permission::VIEW | Permission::CONTROL,
                ),
                account("arch", "backup", false, Permission::VIEW),
            ],
            vec![
                role("viewer", Permission::VIEW, true, 0),
                role("operator", Permission::VIEW | Permission::CONTROL, true, 1),
                role("backup", Permission::VIEW, false, 1),
            ],
            now,
        );
        state
    }

    #[test]
    fn opening_the_screen_selects_somebody_so_the_detail_is_never_blank() {
        let state = loaded(Instant::now());
        assert_eq!(state.target(), Some("ops"));
    }

    #[test]
    fn a_selection_that_no_longer_exists_falls_back_rather_than_showing_nothing() {
        // What happens after the selected account is removed: the list is
        // reloaded from disk and the name it was pointing at is gone.
        let now = Instant::now();
        let mut state = loaded(now);
        state.selected = Some("ops".into());

        state.load(
            vec![account("arch", "backup", false, Permission::VIEW)],
            vec![role("backup", Permission::VIEW, false, 1)],
            now,
        );
        assert_eq!(state.target(), Some("arch"));
    }

    #[test]
    fn an_empty_machine_selects_nobody() {
        let mut state = State::new();
        state.load(Vec::new(), Vec::new(), Instant::now());
        assert_eq!(state.target(), None);
    }

    #[test]
    fn removing_takes_two_presses() {
        // Removing an account is not undoable: only a hash of the password was
        // ever stored, so recreating it means knowing what it was.
        let now = Instant::now();
        let mut state = loaded(now);

        assert!(matches!(
            update(&mut state, Message::Remove, now),
            Outcome::Done
        ));
        assert!(state.is_arming());

        match update(&mut state, Message::Remove, now) {
            Outcome::Act(Action::Remove { username }) => assert_eq!(username, "ops"),
            _ => panic!("the second press should have removed it"),
        }
    }

    #[test]
    fn doing_anything_else_disarms_a_removal() {
        // Arming that outlives the moment is arming somebody forgets about, and
        // the next stray press on that button removes an account.
        let now = Instant::now();
        let mut state = loaded(now);

        update(&mut state, Message::Remove, now);
        assert!(state.is_arming());

        update(&mut state, Message::Select("arch".into()), now);
        assert!(!state.is_arming());

        // And the next press arms again rather than removing.
        assert!(matches!(
            update(&mut state, Message::Remove, now),
            Outcome::Done
        ));
    }

    #[test]
    fn hovering_does_not_disarm_a_removal() {
        // The pointer has to travel over other things to reach the button a
        // second time, and losing the arming on the way would make it
        // impossible to confirm.
        let now = Instant::now();
        let mut state = loaded(now);

        update(&mut state, Message::Remove, now);
        update(&mut state, Message::Hover(0, true), now);
        update(&mut state, Message::Hover(0, false), now);
        assert!(state.is_arming());
    }

    #[test]
    fn the_grid_shows_every_capability_not_just_the_granted_ones() {
        // The whole point of the screen. Six lit chips say nothing about how
        // much is being withheld without the four dark ones beside them.
        let now = Instant::now();
        let state = loaded(now);

        let held = state.showing_permissions();
        assert!(held.contains(Permission::CONTROL));
        assert!(!held.contains(Permission::ADMIN));
        assert_eq!(state.cells.len(), Permission::ALL.len());
    }

    #[test]
    fn switching_account_moves_the_grid_to_the_new_one() {
        let now = Instant::now();
        let mut state = loaded(now);

        assert!(state.showing_permissions().contains(Permission::CONTROL));
        update(&mut state, Message::Select("arch".into()), now);
        assert!(!state.showing_permissions().contains(Permission::CONTROL));
    }

    #[test]
    fn an_accounts_capabilities_cannot_be_edited_directly() {
        // They are whatever the role grants. Editing them per-account would
        // make roles decorative and leave two places to look for the answer.
        let now = Instant::now();
        let mut state = loaded(now);

        assert!(matches!(
            update(
                &mut state,
                Message::ToggleCapability(Permission::ADMIN),
                now
            ),
            Outcome::Done
        ));
        assert!(!state.showing_permissions().contains(Permission::ADMIN));
    }

    #[test]
    fn a_custom_roles_capabilities_can_be_edited() {
        let now = Instant::now();
        let mut state = loaded(now);

        update(&mut state, Message::SelectRole("backup".into()), now);
        match update(
            &mut state,
            Message::ToggleCapability(Permission::FILE_READ),
            now,
        ) {
            Outcome::Act(Action::DefineRole { name, permissions }) => {
                assert_eq!(name, "backup");
                assert!(permissions.contains(Permission::FILE_READ));
                assert!(permissions.contains(Permission::VIEW));
            }
            _ => panic!("editing a custom role should have written it"),
        }
    }

    #[test]
    fn a_new_role_starts_able_to_see_the_screen() {
        // The store refuses a role without it — such a role would be accepted
        // everywhere, grant a session, and show nothing.
        let now = Instant::now();
        let mut state = loaded(now);

        update(&mut state, Message::ToggleAddingRole, now);
        update(&mut state, Message::NewRoleName("night".into()), now);
        match update(&mut state, Message::CreateRole, now) {
            Outcome::Act(Action::CreateRole { name }) => assert_eq!(name, "night"),
            _ => panic!("a named role should have been created"),
        }
    }

    #[test]
    fn editing_a_capability_is_not_mistaken_for_finishing_the_new_role_field() {
        // Both write a role definition. Only one of them means the field has
        // done its job, and confusing them empties a name mid-typing.
        let now = Instant::now();
        let mut state = loaded(now);

        update(&mut state, Message::ToggleAddingRole, now);
        update(&mut state, Message::NewRoleName("night".into()), now);
        update(&mut state, Message::SelectRole("backup".into()), now);

        match update(
            &mut state,
            Message::ToggleCapability(Permission::AUDIO),
            now,
        ) {
            Outcome::Act(Action::DefineRole { .. }) => {}
            _ => panic!("editing a capability should redefine the role"),
        }
        assert_eq!(state.new_role_name(), "night");
    }

    #[test]
    fn a_half_typed_account_is_not_created() {
        let now = Instant::now();
        let mut state = loaded(now);

        update(&mut state, Message::ToggleAdding, now);
        update(&mut state, Message::NewUsername("ghost".into()), now);
        assert!(!state.can_create());
        assert!(matches!(
            update(&mut state, Message::CreateAccount, now),
            Outcome::Done
        ));

        update(&mut state, Message::NewPassword("hunter2".into()), now);
        assert!(state.can_create());
        match update(&mut state, Message::CreateAccount, now) {
            Outcome::Act(Action::Create { username, role, .. }) => {
                assert_eq!(username, "ghost");
                assert_eq!(role, "operator", "the default should not be admin");
            }
            _ => panic!("a complete account should have been created"),
        }
    }

    #[test]
    fn the_grid_shows_what_the_role_being_chosen_would_grant() {
        // Otherwise the form asks somebody to pick a word with no way to see
        // what it buys, and "operator" means nothing until you have been told.
        let now = Instant::now();
        let mut state = loaded(now);

        update(&mut state, Message::ToggleAdding, now);
        assert!(state.showing_permissions().contains(Permission::CONTROL));

        update(&mut state, Message::NewRole("viewer".into()), now);
        assert!(state.showing_permissions().contains(Permission::VIEW));
        assert!(!state.showing_permissions().contains(Permission::CONTROL));
    }

    #[test]
    fn clicking_a_row_closes_the_form_rather_than_doing_nothing_visible() {
        // The form and the detail share one pane, so a click that left the form
        // open would look like a click that did not register.
        let now = Instant::now();
        let mut state = loaded(now);

        update(&mut state, Message::ToggleAdding, now);
        update(&mut state, Message::NewUsername("ghost".into()), now);
        update(&mut state, Message::Select("arch".into()), now);

        assert!(!state.is_adding());
        assert_eq!(state.target(), Some("arch"));
        assert_eq!(
            state.new_username(),
            "ghost",
            "reopening the form should get back what was typed"
        );

        update(&mut state, Message::ToggleAdding, now);
        update(&mut state, Message::SelectRole("backup".into()), now);
        assert!(!state.is_adding());
    }

    #[test]
    fn closing_the_form_puts_the_grid_back_on_the_selected_account() {
        let now = Instant::now();
        let mut state = loaded(now);

        update(&mut state, Message::ToggleAdding, now);
        update(&mut state, Message::NewRole("viewer".into()), now);
        update(&mut state, Message::ToggleAdding, now);

        assert!(state.showing_permissions().contains(Permission::CONTROL));
    }

    #[test]
    fn a_password_is_never_kept_after_the_account_is_made() {
        let now = Instant::now();
        let mut state = loaded(now);

        update(&mut state, Message::ToggleAdding, now);
        update(&mut state, Message::NewPassword("hunter2".into()), now);
        state.finished_adding();

        assert!(state.new_password().is_empty());
        assert!(state.new_username().is_empty());
    }

    #[test]
    fn disabling_asks_for_the_opposite_of_what_is_set() {
        let now = Instant::now();
        let mut state = loaded(now);

        match update(&mut state, Message::ToggleEnabled, now) {
            Outcome::Act(Action::SetEnabled { username, enabled }) => {
                assert_eq!(username, "ops");
                assert!(!enabled, "an enabled account should be asked to turn off");
            }
            _ => panic!("the toggle should have acted"),
        }

        update(&mut state, Message::Select("arch".into()), now);
        match update(&mut state, Message::ToggleEnabled, now) {
            Outcome::Act(Action::SetEnabled { enabled, .. }) => assert!(enabled),
            _ => panic!("the toggle should have acted"),
        }
    }

    #[test]
    fn acting_with_nothing_selected_does_nothing_rather_than_panicking() {
        let now = Instant::now();
        let mut state = State::new();
        state.load(Vec::new(), Vec::new(), now);

        for message in [
            Message::ToggleEnabled,
            Message::Remove,
            Message::AssignRole("admin".into()),
            Message::ToggleCapability(Permission::VIEW),
        ] {
            assert!(matches!(update(&mut state, message, now), Outcome::Done));
        }
    }

    #[test]
    fn a_row_index_beyond_the_hover_table_does_not_run_off_the_end() {
        // The tracker is a fixed table and the account list is not.
        let now = Instant::now();
        let mut state = State::new();
        let many: Vec<Account> = (0..MAX_ROWS * 3)
            .map(|i| account(&format!("user{i}"), "viewer", true, Permission::VIEW))
            .collect();
        state.load(many, vec![role("viewer", Permission::VIEW, true, 0)], now);

        // Reading the last row's hover slot must not index out of bounds.
        let last = state.accounts().len() - 1;
        let _ = state.hovered(last.min(MAX_ROWS - 1), now);
    }
}
