//! Reaching a machine: the connect dialog.
//!
//! Three things are needed and no more: where the machine is, who you are, and
//! how you want the picture. The first is a connect code, the second is a
//! username and password the host checks with Argon2id, and the third is one of
//! three profiles.
//!
//! ## Why a code and not the device ID
//!
//! The `PRV-XXXX-XXXX` shown everywhere else is five bytes of BLAKE3 over the
//! public key. That is a fingerprint: short enough to read aloud, one-way by
//! construction, and therefore impossible to dial. The connect code *is* the
//! key, which is why it is fifty-two characters. Both are shown together on
//! Settings so a person can check the pair agrees.
//!
//! Neither is a secret. Knowing a machine's public key lets someone try to
//! connect, and they still face the username and password on arrival.
//!
//! ## What a refusal says
//!
//! Nothing. A wrong password, an unknown account and a disabled account all
//! come back as the same sentence, because the difference between them is
//! exactly what someone probing for valid usernames wants to learn. The host
//! logs which it was; the person at this end is told to check both fields.
//!
//! ## Motion
//!
//! The dialog arrives from 97% and three pixels low over 220ms and leaves in
//! 180ms, accelerating away; the scrim fades in 200 and out in 180. It fades as
//! a whole, which iced cannot do for a subtree, so the opacity is threaded
//! through every colour the dialog paints — see [`Look`].

use std::time::Instant;

use iced::widget::{button, column, container, mouse_area, opaque, row, text, text_input, Space};
use iced::{Alignment, Background, Border, Color, Element, Length};

use pravera_core::QualityProfile;
use pravera_transport::{PeerAddress, PeerKey};

use crate::components::{self, BUTTON_PADDING};
use crate::icon;
use crate::motion::{self, HoverTracker, Tween};
use crate::net::known;
use crate::theme::{self, tokens as t};

/// Slots in the hover tracker: the three profiles, the two sound choices, the
/// connect button, then cancel.
const PROFILES: usize = 3;
const SOUND: usize = 2;
const SLOT_SOUND: usize = PROFILES;
const SLOT_CONNECT: usize = PROFILES + SOUND;
const SLOT_CANCEL: usize = SLOT_CONNECT + 1;
const HOVER_SLOTS: usize = SLOT_CANCEL + 1;

/// The dialog's width: DigiClip's 400, widened for two fields side by side.
pub const WIDTH: f32 = 460.0;

/// The password field, so recalling a machine can put the cursor in the one
/// thing still missing.
pub const PASSWORD_FIELD: &str = "pravera-connect-password";
/// Where the cursor lands when the code arrived from discovery rather than
/// from somebody typing it.
pub const USERNAME_FIELD: &str = "pravera-connect-username";
/// The connect code, for a machine nothing has told this one about yet.
pub const CODE_FIELD: &str = "pravera-connect-code";

#[derive(Debug, Clone)]
pub enum Message {
    CodeChanged(String),
    UsernameChanged(String),
    PasswordChanged(String),
    Profile(QualityProfile),
    /// Whether to ask the host for its sound as well as its screen.
    Sound(bool),
    Hover(usize, bool),
    /// The button, or Enter in any of the fields.
    Submit,
    /// Put the dialog away. Whatever was typed stays, so a mis-dismissed
    /// dialog costs one click rather than a retyped username.
    Dismiss,
}

/// What the dialog wants the application to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Nothing,
    /// Dial what the form describes.
    Connect,
    /// Put the cursor in the password field, which is now the only gap.
    FocusPassword,
}

/// What a successful login will open.
///
/// Carried by the dialog rather than by a flag on the application: a flag set
/// by the menu's Terminal entry and cleared only on submit survived a
/// cancelled dialog, and turned the next ordinary Connect into a shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Purpose {
    /// The machine's screen.
    #[default]
    Session,
    /// A shell on the machine, and no picture.
    Terminal,
}

/// A machine chosen from the Devices list.
///
/// Everything here is a display hint. Discovery finds machines by asking the
/// tailnet or the local subnet what is out there, and neither can say whether
/// the thing at an address is the machine it claims to be — or whether it runs
/// Pravera at all. The identity is the ed25519 key in the connect code, which
/// is why picking a device fills in the context and not the field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picked {
    pub name: String,
    /// How discovery reached it: "direct · Thunderbolt", "tailnet · relayed".
    pub route: String,
    /// Where it appears to be, if the source gave an address.
    pub address: Option<String>,
}

/// What the form holds between keystrokes.
pub struct State {
    code: String,
    username: String,
    password: String,
    profile: QualityProfile,
    /// Whether to ask for the host's audio. Remembered between sessions by
    /// the application, which is what owns the settings file.
    audio: bool,
    purpose: Purpose,
    /// The machine picked on the Devices list, if the dialog was reached that
    /// way. Context only — see [`Picked`].
    picked: Option<Picked>,
    /// Whether the dialog is wanted on screen. The form outlives the dialog
    /// on purpose: dismissing and returning should not cost what was typed.
    open: bool,
    /// How present the dialog is, 0 to 1. Still above 0 for the length of
    /// the exit after `open` has dropped, which is what keeps it mounted.
    shown: Tween,
    /// The scrim's blur and dim.
    scrim: Tween,
    /// Set while a connection is being made, so the form cannot be submitted
    /// twice and the button can say what it is doing.
    connecting: bool,
    /// The last failure, in the words shown to the person.
    error: Option<String>,
    hover: HoverTracker,
}

impl Default for State {
    fn default() -> Self {
        State {
            code: String::new(),
            username: String::new(),
            password: String::new(),
            profile: QualityProfile::Adaptive,
            audio: pravera_audio::can_play(),
            purpose: Purpose::Session,
            picked: None,
            open: false,
            shown: Tween::at(0.0),
            scrim: Tween::at(0.0),
            connecting: false,
            error: None,
            hover: HoverTracker::new(HOVER_SLOTS),
        }
    }
}

impl State {
    pub fn profile(&self) -> QualityProfile {
        self.profile
    }

    pub fn purpose(&self) -> Purpose {
        self.purpose
    }

    /// Whether this form is asking for the host's sound.
    ///
    /// Always false on a build that cannot play any, whatever was remembered:
    /// asking for a stream nothing can render would cost the host a tap on its
    /// mixer and the link a megabit and a half, for silence.
    pub fn audio(&self) -> bool {
        self.audio && pravera_audio::can_play()
    }

    /// Restore what was chosen last time.
    pub fn set_audio(&mut self, audio: bool) {
        self.audio = audio;
    }

    /// Brings the dialog in. Idempotent while it is already open, so a second
    /// click on a row does not restart the arrival.
    fn arrive(&mut self, purpose: Purpose, now: Instant) {
        self.purpose = purpose;
        if !self.open {
            self.open = true;
            self.shown.enter(now, motion::DIALOG_IN);
            self.scrim.go(1.0, now, std::time::Duration::from_millis(200), motion::EASE_CHANGE);
        }
    }

    /// Takes the dialog away: the exit plays, and [`Self::take_finished_closing`]
    /// unmounts it when it lands.
    fn leave(&mut self, now: Instant) {
        if self.open {
            self.open = false;
            self.shown.exit(now, motion::DIALOG_OUT);
            self.scrim.go(0.0, now, motion::DIALOG_OUT, motion::EASE_EXIT);
        }
    }

    /// Somebody clicked a machine on the Devices list.
    ///
    /// Opens the dialog and leaves whatever was already typed alone: coming
    /// back to a half-filled form and finding it wiped is worse than a stale
    /// username.
    pub fn pick(&mut self, picked: Picked, purpose: Purpose, now: Instant) {
        if self.picked.as_ref() != Some(&picked) {
            self.error = None;
        }
        self.picked = Some(picked);
        self.arrive(purpose, now);
    }

    /// Open the dialog with nothing filled in: the add-a-device path, where
    /// the connect code is the one thing nobody can discover for you.
    pub fn open_empty(&mut self, now: Instant) {
        self.picked = None;
        self.error = None;
        self.arrive(Purpose::Session, now);
    }

    /// Whether the dialog is wanted on screen.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Whether the dialog is mounted at all — opening, open, or on its way
    /// out.
    pub fn is_mounted(&self) -> bool {
        self.open || self.shown.target() > 0.0 || self.shown.value(Instant::now()) > 0.0
    }

    /// How strong the scrim is right now, `0..1`.
    pub fn scrim_amount(&self, now: Instant) -> f32 {
        self.scrim.value(now)
    }

    /// How present the dialog is right now, `0..1`.
    pub fn presence(&self, now: Instant) -> f32 {
        self.shown.value(now)
    }

    /// Put the dialog away. Refused while connecting, because dismissing the
    /// one thing that says a connection is being attempted reads as the
    /// attempt having stopped.
    pub fn dismiss(&mut self, now: Instant) {
        if !self.connecting {
            self.leave(now);
        }
    }

    /// Whether the exit has settled; the dialog is then no longer mounted.
    /// Called from `pump`, where `now` is fresh.
    pub fn take_finished_closing(&mut self, now: Instant) -> bool {
        if !self.open && self.shown.is_gone(now) && !self.scrim.is_animating(now) {
            // Settle both so `is_mounted` stops reading a stale value.
            self.shown.snap(0.0);
            self.scrim.snap(0.0);
            true
        } else {
            false
        }
    }

    /// Fill in the connect code a discovery source announced.
    ///
    /// It is still only a claim. Dialling it proves who answers, and a machine
    /// that was connected to before is recalled by its *pinned* key instead —
    /// so an advertisement cannot quietly redirect a session that already had
    /// a key of its own.
    pub fn suggest_code(&mut self, code: &str) {
        self.code = code.to_string();
    }

    /// Fill the form from a machine that has been connected to before.
    ///
    /// The password is deliberately left empty and is deliberately not stored
    /// anywhere — see [`crate::net::known`]. Anything already typed into it
    /// belongs to whatever was being attempted before and would be wrong here.
    pub fn recall(&mut self, machine: &known::Machine, purpose: Purpose, now: Instant) {
        self.code = machine.code.clone();
        self.username = machine.username.clone();
        self.password.clear();
        self.error = None;
        self.picked = Some(Picked {
            name: machine.name.clone(),
            route: "remembered".to_string(),
            address: machine.device_id().map(|id| id.to_string()),
        });
        self.arrive(purpose, now);
    }

    /// What was typed into the connect code field.
    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn password(&self) -> &str {
        &self.password
    }

    pub fn is_connecting(&self) -> bool {
        self.connecting
    }

    /// A connection attempt started.
    pub fn began(&mut self) {
        self.connecting = true;
        self.error = None;
    }

    /// A connection attempt ended without a session.
    pub fn failed(&mut self, reason: String) {
        self.connecting = false;
        self.error = Some(reason);
    }

    /// A session started, so the dialog is done with. It leaves the way a
    /// dismiss does — the old code dropped it in the same frame — and the
    /// password does not stay in memory a moment longer than the login
    /// needed it.
    pub fn succeeded(&mut self, now: Instant) {
        self.connecting = false;
        self.error = None;
        self.password.clear();
        self.leave(now);
    }

    pub fn is_animating(&self, now: Instant) -> bool {
        self.hover.is_animating(now) || self.shown.is_animating(now) || self.scrim.is_animating(now)
    }

    /// What the form currently describes, or why it cannot be used yet.
    ///
    /// The code is parsed here rather than on submit so the button can be
    /// disabled while it is incomplete, which is a better answer than letting
    /// someone press Connect and wait for a failure that was knowable.
    pub fn peer(&self) -> Result<PeerAddress, &'static str> {
        if self.code.trim().is_empty() {
            return Err("Paste the connect code from the other machine.");
        }
        let key = PeerKey::from_code(&self.code).map_err(|_| "That is not a connect code.")?;
        if self.username.trim().is_empty() {
            return Err("Enter the username the other machine expects.");
        }
        Ok(PeerAddress::new(key))
    }

    fn ready(&self) -> bool {
        !self.connecting && self.peer().is_ok()
    }
}

/// Act on a dialog change.
pub fn update(state: &mut State, message: Message, now: Instant) -> Outcome {
    match message {
        Message::CodeChanged(code) => {
            state.code = code;
            state.error = None;
            Outcome::Nothing
        }
        Message::UsernameChanged(username) => {
            state.username = username;
            state.error = None;
            Outcome::Nothing
        }
        Message::PasswordChanged(password) => {
            state.password = password;
            state.error = None;
            Outcome::Nothing
        }
        Message::Profile(profile) => {
            state.profile = profile;
            Outcome::Nothing
        }
        Message::Sound(audio) => {
            state.audio = audio;
            Outcome::Nothing
        }
        Message::Hover(slot, entering) => {
            state.hover.set(slot, entering, now);
            Outcome::Nothing
        }
        Message::Dismiss => {
            state.dismiss(now);
            Outcome::Nothing
        }
        Message::Submit => {
            if state.ready() {
                Outcome::Connect
            } else {
                Outcome::Nothing
            }
        }
    }
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

/// Every colour the dialog paints, at the dialog's current presence.
///
/// iced cannot fade a subtree, and a dialog floats over the blurred frame,
/// which is not a flat colour a cover could fade from. So the opacity goes
/// where the colours are made.
#[derive(Clone, Copy)]
struct Look {
    alpha: f32,
}

impl Look {
    fn c(self, color: Color) -> Color {
        theme::faded(color, self.alpha)
    }

    fn text(self, color: Color) -> impl Fn(&iced::Theme) -> text::Style {
        theme::tinted(self.c(color))
    }
}

/// The dialog, centred, over the scrim. The application stacks this above
/// whichever layout is on screen.
pub fn dialog_view<'a>(state: &'a State, now: Instant) -> Element<'a, Message> {
    let alpha = state.presence(now);
    let look = Look { alpha };

    let card = components::panel(body(state, look, now))
        .edge(t::BEVEL_RAISED)
        .fill(t::POPOVER)
        .padding(t::SPACE_5)
        .width(Length::Fixed(WIDTH))
        .opacity(alpha)
        .shadow(theme::SHADOW_OVERLAY);

    // Opaque: a press on the card's labels and gaps must not fall through to
    // the scrim underneath, which is a dismiss.
    let card = opaque(motion::pop(card, alpha));

    container(card)
        .center(Length::Fill)
        .padding(t::SPACE_6)
        .into()
}

fn body<'a>(state: &'a State, look: Look, now: Instant) -> Element<'a, Message> {
    let terminal = state.purpose == Purpose::Terminal;
    let name = state
        .picked
        .as_ref()
        .map(|picked| picked.name.as_str())
        .unwrap_or("Add a device");

    let title = if terminal {
        format!("Terminal on {name}")
    } else {
        name.to_string()
    };
    let subtitle = match &state.picked {
        Some(picked) => match &picked.address {
            Some(address) => format!("{} · {}", picked.route, address),
            None => picked.route.clone(),
        },
        None => "its connect code, typed once".to_string(),
    };

    let avatar: Element<'a, Message> = if state.picked.is_some() {
        components::avatar_faded(name, 28.0, look.alpha)
    } else {
        container(icon::stroked(icon::ADD, t::ICON, look.c(t::MUTED_FOREGROUND)))
            .center_x(Length::Fixed(28.0))
            .center_y(Length::Fixed(28.0))
            .style(move |_| container::Style {
                background: Some(Background::Color(look.c(t::NEUTRAL_825))),
                border: Border {
                    color: look.c(t::BORDER),
                    width: 1.0,
                    radius: t::RADIUS_SM.into(),
                },
                ..container::Style::default()
            })
            .into()
    };

    let close = button(
        container(icon::stroked(icon::CLOSE, 12.0, look.c(t::MUTED_FOREGROUND)))
            .center_x(Length::Fill)
            .center_y(Length::Fill),
    )
    .width(Length::Fixed(24.0))
    .height(Length::Fixed(24.0))
    .padding(0)
    .style(move |theme, status| fade_button(theme::ghost_button(theme, status), look))
    .on_press_maybe((!state.connecting).then_some(Message::Dismiss));

    let header = row![
        avatar,
        column![
            text(title)
                .size(t::TEXT_SM)
                .font(t::FONT_UI_STRONG)
                .style(look.text(t::FOREGROUND)),
            text(subtitle)
                .size(t::TEXT_XS)
                .style(look.text(t::SUBTLE_FOREGROUND))
                .wrapping(text::Wrapping::None),
        ]
        .spacing(2.0)
        .width(Length::Fill),
        close,
    ]
    .spacing(t::SPACE_3)
    .align_y(Alignment::Center);

    // A machine with a code only owes a login. One without still owes proof
    // of which machine it is, and pretending otherwise would send a password
    // to whatever answers at that name.
    let needs_code = state.code.trim().is_empty() || PeerKey::from_code(&state.code).is_err();

    let mut form = column![].spacing(t::SPACE_4);

    if needs_code {
        form = form.push(field(
            "Connect code",
            input("PRV connect code, 52 characters", &state.code, look)
                .id(CODE_FIELD)
                .on_input(Message::CodeChanged)
                .on_submit(Message::Submit)
                .font(t::FONT_MONO),
            code_note(state, look),
            look,
        ));
    }

    form = form.push(
        row![
            field(
                "Username",
                input("username", &state.username, look)
                    .id(USERNAME_FIELD)
                    .on_input(Message::UsernameChanged)
                    .on_submit(Message::Submit),
                None,
                look,
            ),
            field(
                "Password",
                input("password", &state.password, look)
                    .id(PASSWORD_FIELD)
                    .on_input(Message::PasswordChanged)
                    .on_submit(Message::Submit)
                    .secure(true),
                None,
                look,
            ),
        ]
        .spacing(t::SPACE_3),
    );

    // A shell has no picture to tune and no sound to hear.
    if !terminal {
        form = form.push(profiles(state, look, now));
        form = form.push(sound(state, look, now));
    }

    if let Some(error) = &state.error {
        form = form.push(refusal(error, look));
    }

    column![
        header,
        rule(look),
        form,
        rule(look),
        footer(state, look, now)
    ]
    .spacing(t::SPACE_4)
    .into()
}

fn rule<'a>(look: Look) -> Element<'a, Message> {
    container(Space::new().height(Length::Fixed(1.0)))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(look.c(t::BORDER))),
            ..container::Style::default()
        })
        .into()
}

/// A text input at the dialog's opacity.
fn input<'a>(placeholder: &'a str, value: &'a str, look: Look) -> text_input::TextInput<'a, Message> {
    text_input(placeholder, value)
        .size(t::TEXT_SM)
        .padding([t::SPACE_2, t::SPACE_3])
        .style(move |theme, status| {
            let style = theme::input(theme, status);
            text_input::Style {
                background: match style.background {
                    Background::Color(color) => Background::Color(look.c(color)),
                    other => other,
                },
                border: Border {
                    color: look.c(style.border.color),
                    ..style.border
                },
                icon: look.c(style.icon),
                placeholder: look.c(style.placeholder),
                value: look.c(style.value),
                selection: look.c(style.selection),
            }
        })
}

fn fade_button(style: button::Style, look: Look) -> button::Style {
    button::Style {
        background: style.background.map(|background| match background {
            Background::Color(color) => Background::Color(look.c(color)),
            other => other,
        }),
        text_color: look.c(style.text_color),
        border: Border {
            color: look.c(style.border.color),
            ..style.border
        },
        shadow: iced::Shadow {
            color: look.c(style.shadow.color),
            ..style.shadow
        },
        ..style
    }
}

/// A labelled input with an optional note under it.
fn field<'a>(
    label: &'static str,
    input: text_input::TextInput<'a, Message>,
    note: Option<Element<'a, Message>>,
    look: Look,
) -> Element<'a, Message> {
    let mut stack = column![
        text(t::tracked(label))
            .size(t::TEXT_2XS)
            .font(t::FONT_UI_MEDIUM)
            .style(look.text(t::SUBTLE_FOREGROUND)),
        input.width(Length::Fill),
    ]
    .spacing(t::SPACE_1_5)
    .width(Length::Fill);

    if let Some(note) = note {
        stack = stack.push(note);
    }
    stack.into()
}

/// Says whether what has been typed is a usable code, and what it names.
///
/// The device ID is derived from the code and shown back, so a person who was
/// given both can confirm they match before sending a password anywhere.
fn code_note<'a>(state: &'a State, look: Look) -> Option<Element<'a, Message>> {
    if state.code.trim().is_empty() {
        return None;
    }
    let (glyph, tint, words) = match PeerKey::from_code(&state.code) {
        Ok(key) => (icon::LOCK, t::SUCCESS, format!("names {}", key.device_id())),
        // Deliberately vague about *why*. The parser knows whether it was the
        // length or a bad character, and neither helps someone who mistyped.
        Err(_) => (icon::ALERT, t::WARNING, "not a complete connect code yet".to_string()),
    };
    Some(
        row![
            icon::stroked(glyph, 12.0, look.c(tint)),
            text(words)
                .size(t::TEXT_XS)
                .style(look.text(t::MUTED_FOREGROUND)),
        ]
        .spacing(t::SPACE_1_5)
        .align_y(Alignment::Center)
        .into(),
    )
}

/// The three quality profiles as one segmented control, with the chosen one
/// explained underneath.
fn profiles<'a>(state: &'a State, look: Look, now: Instant) -> Element<'a, Message> {
    let choices = [
        (
            QualityProfile::Quality,
            "Quality",
            "Sharp text. Best for reading and admin.",
        ),
        (
            QualityProfile::Adaptive,
            "Adaptive",
            "Retunes itself from the measured link.",
        ),
        (
            QualityProfile::Latency,
            "Latency",
            "Lowest delay. Best for anything played.",
        ),
    ];

    let blurb = choices
        .iter()
        .find(|(profile, _, _)| *profile == state.profile)
        .map_or("", |(_, _, blurb)| *blurb);

    let cells = choices
        .iter()
        .enumerate()
        .map(|(index, (profile, name, _))| {
            segment(
                name,
                *profile == state.profile,
                state.hover.amount(index, now),
                index,
                Message::Profile(*profile),
                look,
            )
        })
        .collect::<Vec<_>>();

    labelled(
        "Picture",
        column![
            segmented(cells, look),
            text(blurb).size(t::TEXT_XS).style(look.text(t::SUBTLE_FOREGROUND)),
        ]
        .spacing(t::SPACE_1_5)
        .into(),
        look,
    )
}

/// Whether to ask for the host's audio.
///
/// Sits beside the picture profiles because it is the same kind of decision —
/// what this session will cost and what it will give back — and because a
/// person deciding how a session should behave decides both at once.
fn sound<'a>(state: &'a State, look: Look, now: Instant) -> Element<'a, Message> {
    if !pravera_audio::can_play() {
        // Said plainly rather than shown as a control that does nothing.
        return labelled(
            "Sound",
            text("This build has no way to play remote audio.")
                .size(t::TEXT_XS)
                .style(look.text(t::SUBTLE_FOREGROUND))
                .into(),
            look,
        );
    }

    let on = state.audio;
    labelled(
        "Sound",
        segmented(
            vec![
                segment(
                    "Hear the host",
                    on,
                    state.hover.amount(SLOT_SOUND, now),
                    SLOT_SOUND,
                    Message::Sound(true),
                    look,
                ),
                segment(
                    "Silent",
                    !on,
                    state.hover.amount(SLOT_SOUND + 1, now),
                    SLOT_SOUND + 1,
                    Message::Sound(false),
                    look,
                ),
            ],
            look,
        ),
        look,
    )
}

/// A small caption above a control.
fn labelled<'a>(label: &'a str, body: Element<'a, Message>, look: Look) -> Element<'a, Message> {
    column![
        text(t::tracked(label))
            .size(t::TEXT_2XS)
            .font(t::FONT_UI_MEDIUM)
            .style(look.text(t::SUBTLE_FOREGROUND)),
        body
    ]
    .spacing(t::SPACE_1_5)
    .into()
}

/// DigiClip's segmented control: a bevelled well, the chosen cell lifted.
fn segmented<'a>(cells: Vec<Element<'a, Message>>, look: Look) -> Element<'a, Message> {
    components::panel(row(cells).spacing(2.0).width(Length::Fill))
        .edge(t::BEVEL_RAISED)
        .fill(t::BACKGROUND)
        .padding(2.0)
        .width(Length::Fill)
        .opacity(look.alpha)
        .into()
}

/// One cell of a segmented control.
fn segment<'a>(
    name: &'a str,
    active: bool,
    hover: f32,
    slot: usize,
    message: Message,
    look: Look,
) -> Element<'a, Message> {
    let background = if active {
        t::SECONDARY
    } else {
        t::with_alpha(t::NEUTRAL_825, hover)
    };
    let foreground = if active {
        t::FOREGROUND
    } else {
        theme::blend(t::MUTED_FOREGROUND, t::FOREGROUND, hover)
    };

    mouse_area(
        button(
            container(
                text(name)
                    .size(t::TEXT_XS + 1.0)
                    .font(if active { t::FONT_UI_MEDIUM } else { t::FONT_UI })
                    .style(look.text(foreground)),
            )
            .center_x(Length::Fill),
        )
        .width(Length::Fill)
        .padding([5.0, t::SPACE_2])
        .style(move |_, status| {
            let background = match status {
                button::Status::Pressed => t::ACCENT,
                _ => background,
            };
            button::Style {
                background: Some(Background::Color(look.c(background))),
                text_color: look.c(foreground),
                border: Border {
                    radius: (t::RADIUS - 2.0).into(),
                    ..Border::default()
                },
                ..button::Style::default()
            }
        })
        .on_press(message),
    )
    .on_enter(Message::Hover(slot, true))
    .on_exit(Message::Hover(slot, false))
    .into()
}

/// What the host said, in the words shown to the person.
fn refusal<'a>(message: &'a str, look: Look) -> Element<'a, Message> {
    container(
        row![
            icon::stroked(icon::ALERT, t::ICON_SM, look.c(t::DESTRUCTIVE_TEXT)),
            text(message)
                .size(t::TEXT_XS + 1.0)
                .style(look.text(t::NEUTRAL_200))
                .width(Length::Fill),
        ]
        .spacing(t::SPACE_2)
        .align_y(Alignment::Center),
    )
    .padding([t::SPACE_2, t::SPACE_3])
    .width(Length::Fill)
    .style(move |_| container::Style {
        background: Some(Background::Color(look.c(t::DESTRUCTIVE_SOFT))),
        border: Border {
            color: look.c(t::with_alpha(t::DESTRUCTIVE, 0.45)),
            width: t::BORDER_WIDTH,
            radius: t::RADIUS.into(),
        },
        ..container::Style::default()
    })
    .into()
}

/// Why the button is disabled, then Cancel and the action, right-aligned.
fn footer<'a>(state: &'a State, look: Look, now: Instant) -> Element<'a, Message> {
    let terminal = state.purpose == Purpose::Terminal;
    let (label, enabled) = if state.is_connecting() {
        (if terminal { "Opening…" } else { "Connecting…" }, false)
    } else {
        (if terminal { "Open terminal" } else { "Connect" }, state.ready())
    };

    // Disabled with a reason beside it rather than silently inert: a button
    // that does nothing when pressed reads as a broken app.
    let hint: Element<'_, Message> = match state.peer() {
        Err(reason) if !state.is_connecting() => text(reason)
            .size(t::TEXT_XS)
            .style(look.text(t::SUBTLE_FOREGROUND))
            .into(),
        _ => Space::new().into(),
    };

    let cancel_hover = state.hover.amount(SLOT_CANCEL, now);
    let cancel = mouse_area(
        button(text("Cancel").size(t::TEXT_SM).font(t::FONT_UI_MEDIUM))
            .padding(BUTTON_PADDING)
            .style(move |_, status| {
                let background = match status {
                    button::Status::Pressed => t::ACCENT,
                    _ => t::with_alpha(t::SECONDARY, cancel_hover),
                };
                let base = button::Style {
                    background: Some(Background::Color(background)),
                    text_color: theme::blend(t::MUTED_FOREGROUND, t::FOREGROUND, cancel_hover),
                    border: Border {
                        radius: t::RADIUS.into(),
                        ..Border::default()
                    },
                    ..button::Style::default()
                };
                fade_button(base, look)
            })
            .on_press_maybe((!state.connecting).then_some(Message::Dismiss)),
    )
    .on_enter(Message::Hover(SLOT_CANCEL, true))
    .on_exit(Message::Hover(SLOT_CANCEL, false));

    let hover = state.hover.amount(SLOT_CONNECT, now);
    let glyph = if terminal { icon::TERMINAL } else { icon::CONNECT };
    let action = mouse_area(
        button(
            row![
                text(label).size(t::TEXT_SM).font(t::FONT_UI_MEDIUM),
                icon::stroked(glyph, t::ICON_SM, look.c(t::PRIMARY_FOREGROUND)),
            ]
            .spacing(t::SPACE_2)
            .align_y(Alignment::Center),
        )
        .padding(BUTTON_PADDING)
        .style(move |theme, status| {
            let mut style = theme::primary_button(theme, status);
            if enabled && !matches!(status, button::Status::Pressed) {
                style.background = Some(Background::Color(theme::blend(
                    t::PRIMARY,
                    t::PRIMARY_HOVER,
                    hover,
                )));
            }
            fade_button(style, look)
        })
        .on_press_maybe(enabled.then_some(Message::Submit)),
    )
    .on_enter(Message::Hover(SLOT_CONNECT, true))
    .on_exit(Message::Hover(SLOT_CONNECT, false));

    row![
        container(hint).width(Length::Fill),
        cancel,
        action
    ]
    .spacing(t::SPACE_2)
    .align_y(Alignment::Center)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Arbitrary key bytes.
    ///
    /// A connect code is a string encoding of thirty-two bytes; whether those
    /// bytes are a point on the curve is decided later, when the dial happens.
    /// Nothing here needs a real one.
    fn key() -> [u8; 32] {
        let mut key = [0u8; 32];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        key
    }

    fn code() -> String {
        pravera_core::connect_code::grouped(&key())
    }

    /// A form filled in as far as the test needs.
    fn filled(code: &str, username: &str, password: &str) -> State {
        State {
            code: code.to_string(),
            username: username.to_string(),
            password: password.to_string(),
            ..State::default()
        }
    }

    /// Press the button.
    fn submit(state: &mut State) -> Outcome {
        update(state, Message::Submit, Instant::now())
    }

    fn picked(name: &str) -> Picked {
        Picked {
            name: name.to_string(),
            route: "tailnet · direct".into(),
            address: Some("100.101.102.103".into()),
        }
    }

    fn machine() -> known::Machine {
        known::Machine {
            code: code(),
            name: "Evercore".into(),
            username: "driver".into(),
            last_used: 0,
        }
    }

    #[test]
    fn a_complete_form_names_the_machine_the_code_names() {
        let state = filled(&code(), "operator", "");

        let peer = state.peer().expect("a complete form");
        assert_eq!(
            peer.device_id(),
            PeerKey::from_code(&code()).unwrap().device_id()
        );
    }

    #[test]
    fn an_incomplete_code_is_refused_before_anything_is_dialled() {
        // Otherwise pressing Connect spends a network timeout discovering
        // something the form already knew.
        assert!(filled("", "operator", "").peer().is_err());
        assert!(filled("PRV-0000", "operator", "").peer().is_err());
        assert!(filled(&code(), "operator", "").peer().is_ok());
    }

    #[test]
    fn a_form_with_no_username_cannot_be_submitted() {
        let mut state = filled(&code(), "", "");
        assert!(state.peer().is_err());
        assert_eq!(submit(&mut state), Outcome::Nothing);
    }

    #[test]
    fn submitting_a_complete_form_asks_for_a_connection() {
        let mut state = filled(&code(), "operator", "hunter2");
        assert_eq!(submit(&mut state), Outcome::Connect);
    }

    #[test]
    fn a_second_press_while_connecting_does_nothing() {
        // Two endpoints dialling the same host from one window is two sessions
        // and one visible picture.
        let mut state = filled(&code(), "operator", "hunter2");
        state.began();
        assert_eq!(submit(&mut state), Outcome::Nothing);
    }

    #[test]
    fn picking_a_machine_does_not_fill_in_its_connect_code() {
        // It could not: discovery reports a hostname and an address, and the
        // machine at that address is whatever answers there.
        let mut state = State::default();
        state.pick(picked("homelab"), Purpose::Session, Instant::now());

        assert!(state.code.is_empty());
        assert!(state.peer().is_err(), "a picked machine is not dialable");
    }

    #[test]
    fn picking_a_machine_keeps_what_was_already_typed() {
        let mut state = filled(&code(), "operator", "hunter2");
        state.pick(picked("homelab"), Purpose::Session, Instant::now());

        assert_eq!(state.username, "operator");
        assert_eq!(state.code, code());
        assert!(state.peer().is_ok());
    }

    #[test]
    fn picking_a_different_machine_clears_the_last_refusal() {
        // The message said why *that* machine refused.
        let mut state = State::default();
        state.pick(picked("homelab"), Purpose::Session, Instant::now());
        state.failed("Check the username and password.".into());

        state.pick(picked("workshop"), Purpose::Session, Instant::now());
        assert!(state.error.is_none());
    }

    #[test]
    fn picking_the_same_machine_again_leaves_its_refusal_up() {
        let mut state = State::default();
        state.pick(picked("homelab"), Purpose::Session, Instant::now());
        state.failed("Check the username and password.".into());

        state.pick(picked("homelab"), Purpose::Session, Instant::now());
        assert!(state.error.is_some());
    }

    #[test]
    fn the_password_does_not_outlive_the_login_that_used_it() {
        let mut state = filled(&code(), "operator", "hunter2");
        state.succeeded(Instant::now());
        assert!(state.password.is_empty());
    }

    #[test]
    fn a_refusal_is_shown_without_being_rewritten() {
        let mut state = State::default();
        state.began();
        state.failed("That username and password were not accepted.".into());

        assert!(!state.connecting);
        assert_eq!(
            state.error.as_deref(),
            Some("That username and password were not accepted.")
        );
    }

    #[test]
    fn typing_again_clears_the_last_refusal() {
        // A stale error under a field being corrected reads as a live one.
        let mut state = State::default();
        state.failed("no".into());
        update(&mut state, Message::PasswordChanged("x".into()), Instant::now());
        assert!(state.error.is_none());
    }

    #[test]
    fn every_choice_and_button_has_a_hover_slot_of_its_own() {
        // Two cells sharing a slot would light up together.
        assert_eq!(QualityProfile::ALL.len(), PROFILES);
        assert_eq!(SLOT_SOUND, PROFILES);
        assert_eq!(SLOT_CONNECT, PROFILES + SOUND);
        assert_eq!(SLOT_CANCEL, SLOT_CONNECT + 1);
        assert_eq!(HOVER_SLOTS, SLOT_CANCEL + 1);
    }

    #[test]
    fn a_build_that_cannot_play_audio_never_asks_a_host_for_any() {
        let mut state = State::default();
        state.set_audio(true);
        assert_eq!(state.audio(), pravera_audio::can_play());
    }

    #[test]
    fn choosing_silence_is_remembered_for_the_next_session() {
        let mut state = State::default();
        update(&mut state, Message::Sound(false), Instant::now());
        assert!(!state.audio());
    }

    #[test]
    fn recalling_a_machine_opens_the_dialog_and_fills_everything_but_the_password() {
        let mut state = State::default();
        state.recall(&machine(), Purpose::Session, Instant::now());

        assert!(state.is_open());
        assert_eq!(state.code, code());
        assert_eq!(state.username, "driver");
        assert!(state.password.is_empty());
        assert!(state.peer().is_ok(), "the dialog should be ready to dial");
    }

    #[test]
    fn dismissing_does_nothing_while_a_connection_is_under_way() {
        // The dialog is the only thing saying an attempt is in flight.
        let mut state = filled(&code(), "operator", "hunter2");
        state.pick(picked("homelab"), Purpose::Session, Instant::now());
        state.began();

        update(&mut state, Message::Dismiss, Instant::now());
        assert!(state.is_open(), "the dialog was dismissed mid-connection");

        state.succeeded(Instant::now());
        assert!(!state.is_open(), "success leaves the dialog shut");
    }

    /// The bug that made the dialog vanish instead of leaving: success
    /// dropped it in the frame the session arrived.
    #[test]
    fn success_plays_the_exit_rather_than_vanishing() {
        let now = Instant::now();
        let mut state = filled(&code(), "operator", "hunter2");
        state.pick(picked("homelab"), Purpose::Session, now);
        let open = now + motion::DIALOG_IN;
        assert_eq!(state.presence(open), 1.0);

        state.succeeded(open);
        assert!(state.is_mounted(), "still drawn while it leaves");
        assert!(state.presence(open + motion::DIALOG_OUT / 4) > 0.5);
        assert!(!state.take_finished_closing(open + motion::DIALOG_OUT / 2));
        assert!(state.take_finished_closing(open + motion::DIALOG_OUT * 2));
        assert!(!state.is_mounted());
    }

    #[test]
    fn opening_fades_and_scales_in_rather_than_cutting() {
        let now = Instant::now();
        let mut state = State::default();
        state.open_empty(now);
        assert_eq!(state.presence(now), 0.0, "starts invisible");
        let early = state.presence(now + motion::DIALOG_IN / 3);
        assert!(early > 0.0 && early < 1.0, "is in motion, got {early}");
        assert!(state.is_animating(now));
    }

    #[test]
    fn a_second_click_on_an_open_dialog_does_not_restart_its_arrival() {
        let now = Instant::now();
        let mut state = State::default();
        state.pick(picked("homelab"), Purpose::Session, now);
        let later = now + motion::DIALOG_IN;
        state.pick(picked("homelab"), Purpose::Session, later);
        assert_eq!(state.presence(later), 1.0);
    }

    /// The leak the old global flag had: a cancelled Terminal turned the next
    /// ordinary Connect into a shell.
    #[test]
    fn the_purpose_belongs_to_the_opening_that_set_it() {
        let now = Instant::now();
        let mut state = State::default();
        state.pick(picked("homelab"), Purpose::Terminal, now);
        assert_eq!(state.purpose(), Purpose::Terminal);
        state.dismiss(now);
        state.pick(picked("homelab"), Purpose::Session, now + motion::DIALOG_OUT * 2);
        assert_eq!(state.purpose(), Purpose::Session);
        state.dismiss(now);
        state.open_empty(now + motion::DIALOG_OUT * 4);
        assert_eq!(state.purpose(), Purpose::Session);
    }
}
