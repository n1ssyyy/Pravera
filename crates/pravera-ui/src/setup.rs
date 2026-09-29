//! The installer window: what `Pravera-Setup-…` opens as.
//!
//! One small window with the same face as the app, doing one of four things
//! depending on what it finds: installing, updating an older copy, repairing
//! the same one, or removing it. It never asks for an administrator, never
//! shows a licence page nobody reads, and never asks where to put things a
//! per-user install has exactly one sensible place for. The two choices worth
//! a person's attention — a desktop shortcut, and whether to open Pravera at
//! the end — are switches, and everything else is a button.

use iced::widget::{button, column, container, mouse_area, row, svg, text, Space};
use iced::{Alignment, Background, Border, Element, Length, Size, Task};

use crate::components::{self, Tone};
use crate::icon;
use crate::install::{self, Cli, Installed, Layout, Options, Report, VERSION};
use crate::theme::{self, tokens as t};

const SIZE: Size = Size::new(560.0, 480.0);

/// Open the installer window and run it until it is closed.
pub fn run(cli: Cli) -> iced::Result {
    let result = iced::application(move || Setup::new(&cli), Setup::update, Setup::view)
        .title(|_: &Setup| "Pravera Setup".to_string())
        .theme(|_: &Setup| theme::theme())
        .font(t::FONT_BYTES)
        .font(t::font_at(t::EXTRA_WEIGHTS[0]))
        .font(t::font_at(t::EXTRA_WEIGHTS[1]))
        .default_font(t::FONT_UI)
        .window(iced::window::Settings {
            size: SIZE,
            resizable: false,
            icon: crate::window_icon(),
            decorations: false,
            position: iced::window::Position::Centered,
            ..iced::window::Settings::default()
        })
        .antialiasing(true)
        .run();
    #[cfg(windows)]
    install::delete_self_if_temporary();
    result
}

#[derive(Debug, Clone)]
enum Message {
    WindowReady(Option<iced::window::Id>),
    Drag,
    Close,
    /// A switch was pointed at; the switches here do not animate.
    Hover,
    ToggleDesktop,
    ToggleLaunch,
    TogglePurge,
    Install,
    /// Show the removal page.
    AskUninstall,
    Back,
    Uninstall,
    Finished(Result<Report, String>),
    Open,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Stage {
    Choose,
    ConfirmUninstall,
    Working { removing: bool },
    Done { removing: bool, outcome: Result<Report, String> },
}

struct Setup {
    window: Option<iced::window::Id>,
    layout: Option<Layout>,
    installed: Option<Installed>,
    stage: Stage,
    desktop: bool,
    launch: bool,
    purge: bool,
}

/// How what is installed compares with what this installer carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relation {
    Fresh,
    Older,
    Same,
    Newer,
}

impl Setup {
    fn new(cli: &Cli) -> (Setup, Task<Message>) {
        let layout = match &cli.dir {
            Some(dir) => Some(Layout::in_dir(dir.clone())),
            None => Layout::find(),
        };
        let installed = layout.as_ref().and_then(Layout::installed);
        let stage = if cli.uninstall && installed.is_some() {
            Stage::ConfirmUninstall
        } else {
            Stage::Choose
        };
        let setup = Setup {
            window: None,
            desktop: installed.as_ref().map_or(cli.desktop, |found| found.desktop || cli.desktop),
            launch: !cli.no_launch,
            purge: cli.purge,
            layout,
            installed,
            stage,
        };
        (setup, iced::window::latest().map(Message::WindowReady))
    }

    fn relation(&self) -> Relation {
        match &self.installed {
            None => Relation::Fresh,
            Some(Installed { version: None, .. }) => Relation::Older,
            Some(Installed { version: Some(there), .. }) => {
                let here = install::current_version();
                match there.cmp(&here) {
                    std::cmp::Ordering::Less => Relation::Older,
                    std::cmp::Ordering::Equal => Relation::Same,
                    std::cmp::Ordering::Greater => Relation::Newer,
                }
            }
        }
    }

    fn options(&self) -> Options {
        Options {
            desktop: self.desktop,
            launch: self.launch,
            purge: self.purge,
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::WindowReady(id) => {
                self.window = id;
                id.map(crate::chrome::dress).unwrap_or_else(Task::none)
            }
            Message::Drag => self.window.map(iced::window::drag).unwrap_or_else(Task::none),
            Message::Close => iced::exit(),
            Message::Hover => Task::none(),
            Message::ToggleDesktop => {
                self.desktop = !self.desktop;
                Task::none()
            }
            Message::ToggleLaunch => {
                self.launch = !self.launch;
                Task::none()
            }
            Message::TogglePurge => {
                self.purge = !self.purge;
                Task::none()
            }
            Message::AskUninstall => {
                self.stage = Stage::ConfirmUninstall;
                Task::none()
            }
            Message::Back => {
                self.stage = Stage::Choose;
                Task::none()
            }
            Message::Install => self.start(false),
            Message::Uninstall => self.start(true),
            Message::Finished(outcome) => {
                let removing = matches!(self.stage, Stage::Working { removing: true });
                // Installed and opened: the app is on its way up and this
                // window has nothing left to say.
                if !removing && self.launch && outcome.as_ref().is_ok_and(|report| report.warnings.is_empty()) {
                    return iced::exit();
                }
                if let Some(layout) = &self.layout {
                    self.installed = layout.installed();
                }
                self.stage = Stage::Done { removing, outcome };
                Task::none()
            }
            Message::Open => {
                if let Some(layout) = &self.layout {
                    let _ = install::open(layout);
                }
                iced::exit()
            }
        }
    }

    fn start(&mut self, removing: bool) -> Task<Message> {
        let Some(layout) = self.layout.clone() else {
            self.stage = Stage::Done {
                removing,
                outcome: Err("There is no per-user place to install to on this system.".into()),
            };
            return Task::none();
        };
        self.stage = Stage::Working { removing };
        let options = self.options();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    if removing {
                        install::uninstall(&layout, options)
                    } else {
                        install::install(&layout, options)
                    }
                })
                .await
                .unwrap_or_else(|error| Err(error.to_string()))
            },
            Message::Finished,
        )
    }

    fn view(&self) -> Element<'_, Message> {
        let body: Element<'_, Message> = match &self.stage {
            Stage::Choose => self.choose(),
            Stage::ConfirmUninstall => self.confirm_uninstall(),
            Stage::Working { removing } => working(*removing),
            Stage::Done { removing, outcome } => self.done(*removing, outcome),
        };

        let frame = column![
            titlebar(),
            container(body)
                .padding([t::SPACE_6, t::SPACE_8])
                .width(Length::Fill)
                .height(Length::Fill),
        ];

        container(frame)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| iced::widget::container::Style {
                background: Some(Background::Color(t::BACKGROUND)),
                text_color: Some(t::FOREGROUND),
                border: Border {
                    color: t::BORDER,
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..Default::default()
            })
            .into()
    }

    fn where_to(&self) -> String {
        self.layout
            .as_ref()
            .map(|layout| layout.bundle.as_ref().unwrap_or(&layout.dir).display().to_string())
            .unwrap_or_else(|| "nowhere — this system has no per-user program folder".into())
    }

    fn choose(&self) -> Element<'_, Message> {
        let installed_version = self
            .installed
            .as_ref()
            .and_then(|found| found.version.as_ref())
            .map(ToString::to_string)
            .unwrap_or_else(|| "an unknown version".into());

        let (status, action): (String, &str) = match self.relation() {
            Relation::Fresh => (format!("Installs for you only, into {}. No administrator needed.", self.where_to()), "Install"),
            Relation::Older => (
                format!("Pravera {installed_version} is installed. This updates it to {VERSION}; your devices, accounts and settings stay."),
                "Update",
            ),
            Relation::Same => (
                format!("Pravera {VERSION} is already installed. Reinstalling puts back anything that went missing."),
                "Reinstall",
            ),
            Relation::Newer => (
                format!("Pravera {installed_version} is installed, which is newer than this installer ({VERSION}). Installing would go back a version."),
                "Install anyway",
            ),
        };

        let switches = components::rows(vec![
            inset(components::switch_row(
                "Desktop shortcut",
                "The Start menu entry is added either way.",
                travel(self.desktop),
                0.0,
                Message::ToggleDesktop,
                Message::Hover,
                Message::Hover,
            )),
            inset(components::switch_row(
                "Open Pravera when done",
                "It keeps itself up to date from then on.",
                travel(self.launch),
                0.0,
                Message::ToggleLaunch,
                Message::Hover,
                Message::Hover,
            )),
        ]);

        let mut footer = row![].spacing(t::SPACE_2).align_y(Alignment::Center);
        if self.installed.is_some() {
            footer = footer.push(components::small_button(Some(icon::TRASH), "Uninstall", Some(Message::AskUninstall)));
        }
        footer = footer
            .push(Space::new().width(Length::Fill))
            .push(big_button(action, theme::primary_button, Some(Message::Install)));

        column![
            brand(),
            Space::new().height(t::SPACE_6),
            components::note(status),
            Space::new().height(t::SPACE_4),
            only_switches(switches),
            Space::new().height(Length::Fill),
            footer,
        ]
        .into()
    }

    fn confirm_uninstall(&self) -> Element<'_, Message> {
        let purge = components::rows(vec![inset(components::switch_row(
            "Also delete my data",
            "This machine's identity, saved accounts, known devices and settings. Without it, \
             reinstalling later picks up where you left off.",
            travel(self.purge),
            0.0,
            Message::TogglePurge,
            Message::Hover,
            Message::Hover,
        ))]);

        column![
            brand(),
            Space::new().height(t::SPACE_6),
            components::note(format!(
                "Removes Pravera from {}, with its shortcuts, its start-at-sign-in entry and its \
                 entry in the list of installed apps. Anything connected to this machine is \
                 disconnected.",
                self.where_to()
            )),
            Space::new().height(t::SPACE_4),
            only_switches(purge),
            Space::new().height(Length::Fill),
            row![
                components::small_button(None, "Back", Some(Message::Back)),
                Space::new().width(Length::Fill),
                big_button("Uninstall", theme::destructive_button, Some(Message::Uninstall)),
            ]
            .align_y(Alignment::Center),
        ]
        .into()
    }

    fn done<'a>(&'a self, removing: bool, outcome: &'a Result<Report, String>) -> Element<'a, Message> {
        let (glyph, tint, headline) = match (outcome, removing) {
            (Err(_), false) => (icon::ALERT, t::DESTRUCTIVE, "Pravera was not installed".to_string()),
            (Err(_), true) => (icon::ALERT, t::DESTRUCTIVE, "Pravera was not removed".to_string()),
            (Ok(_), false) => (icon::CHECK, t::LIME, format!("Pravera {VERSION} is installed")),
            (Ok(_), true) => (icon::CHECK, t::LIME, "Pravera is removed".to_string()),
        };

        let mut lines = column![].spacing(t::SPACE_2);
        match outcome {
            Ok(report) => {
                for step in &report.steps {
                    lines = lines.push(
                        row![
                            icon::stroked(icon::CHECK, 12.0, t::MUTED_FOREGROUND),
                            text(step.as_str()).size(t::TEXT_XS).style(theme::muted),
                        ]
                        .spacing(t::SPACE_2)
                        .align_y(Alignment::Center),
                    );
                }
                for warning in &report.warnings {
                    lines = lines.push(components::callout(icon::ALERT, warning.as_str(), Tone::Warning));
                }
            }
            Err(error) => {
                lines = lines.push(components::callout(icon::ALERT, error.as_str(), Tone::Danger));
            }
        }

        let mut footer = row![Space::new().width(Length::Fill)].spacing(t::SPACE_2).align_y(Alignment::Center);
        let can_open = !removing && outcome.is_ok() && self.installed.is_some();
        if can_open {
            footer = footer
                .push(components::small_button(None, "Close", Some(Message::Close)))
                .push(big_button("Open Pravera", theme::primary_button, Some(Message::Open)));
        } else {
            footer = footer.push(big_button("Close", theme::primary_button, Some(Message::Close)));
        }

        column![
            components::hero(glyph, tint, headline, "What happened, step by step:"),
            Space::new().height(t::SPACE_4),
            components::scroll(lines),
            Space::new().height(t::SPACE_4),
            footer,
        ]
        .height(Length::Fill)
        .into()
    }
}

fn working<'a>(removing: bool) -> Element<'a, Message> {
    column![
        brand(),
        Space::new().height(Length::Fill),
        container(
            column![
                text(if removing { "Removing Pravera…" } else { "Installing Pravera…" })
                    .size(t::TEXT_LG)
                    .font(t::FONT_UI_STRONG)
                    .style(theme::heading),
                components::note("Closing any running copy first. This takes a few seconds."),
            ]
            .spacing(t::SPACE_2)
            .align_x(Alignment::Center),
        )
        .center_x(Length::Fill),
        Space::new().height(Length::Fill),
    ]
    .into()
}

/// The mark, the name and the version, as the top of every page.
fn brand<'a>() -> Element<'a, Message> {
    let logo = svg(svg::Handle::from_memory(install::LOGO_SVG.as_bytes()))
        .width(Length::Fixed(56.0))
        .height(Length::Fixed(56.0));
    row![
        logo,
        column![
            row![
                text("Pravera").size(t::TEXT_2XL).font(t::FONT_UI_STRONG).style(theme::heading),
                components::pill(format!("v{VERSION}"), Tone::Outline),
            ]
            .spacing(t::SPACE_3)
            .align_y(Alignment::Center),
            text("Peer-to-peer remote desktop. No accounts, no relays you do not run.")
                .size(t::TEXT_SM)
                .style(theme::muted),
        ]
        .spacing(t::SPACE_1),
    ]
    .spacing(t::SPACE_4)
    .align_y(Alignment::Center)
    .into()
}

/// A title bar that drags the window and closes it; nothing else, because a
/// fixed-size installer has nothing to minimise into or maximise to.
fn titlebar<'a>() -> Element<'a, Message> {
    let close = button(
        container(icon::stroked(icon::WIN_CLOSE, 10.0, t::MUTED_FOREGROUND))
            .center_x(Length::Fixed(40.0))
            .center_y(Length::Fixed(32.0)),
    )
    .padding(0)
    .style(theme::ghost_button)
    .on_press(Message::Close);

    let bar = row![
        Space::new().width(t::SPACE_3),
        text("Pravera Setup").size(t::TEXT_XS).style(theme::subtle),
        Space::new().width(Length::Fill),
        close,
    ]
    .align_y(Alignment::Center)
    .height(Length::Fixed(32.0));

    mouse_area(container(bar).width(Length::Fill)).on_press(Message::Drag).into()
}

fn big_button<'a>(
    words: &'a str,
    style: fn(&iced::Theme, button::Status) -> button::Style,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    button(text(words).size(t::TEXT_SM).font(t::FONT_UI_MEDIUM))
        .padding([t::SPACE_2, t::SPACE_5])
        .style(style)
        .on_press_maybe(on_press)
        .into()
}

fn inset<'a>(content: Element<'a, Message>) -> Element<'a, Message> {
    container(content).padding(t::SPACE_1).width(Length::Fill).into()
}

fn only_switches<'a>(rows: Element<'a, Message>) -> Element<'a, Message> {
    container(rows).width(Length::Fill).into()
}

fn travel(on: bool) -> f32 {
    if on {
        1.0
    } else {
        0.0
    }
}
