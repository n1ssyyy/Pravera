//! The installer window: what `Pravera-Setup-…` opens as.
//!
//! One small window with the same face as the app, doing one of four things
//! depending on what it finds: installing, updating an older copy, repairing
//! the same one, or removing it. It never asks for an administrator, never
//! shows a licence page nobody reads, and never asks where to put things a
//! per-user install has exactly one sensible place for. The two choices worth
//! a person's attention — a desktop shortcut, and whether to open Pravera at
//! the end — are switches, and everything else is a button.
//!
//! The top of every page is a path, because the path is the product: this
//! installer on one end, the machine on the other, and the line between them
//! filling as the real steps finish. Nothing on it is a timer. Each step is
//! reported by the install itself as it starts ([`install::Phase`]), and
//! each one's duration is measured, not estimated.

use std::time::{Duration, Instant};

use iced::mouse;
use iced::widget::canvas::{self, Path, Stroke};
use iced::widget::{button, column, container, mouse_area, row, text, Space};
use iced::{Alignment, Background, Border, Color, Element, Length, Point, Rectangle, Renderer, Size, Subscription, Task, Theme};

use crate::components::{self, Tone};
use crate::icon;
use crate::install::{self, Cli, Installed, Layout, Options, Phase, Report, VERSION};
use crate::motion::{self, HoverTracker, Tween};
use crate::theme::{self, tokens as t};

const SIZE: Size = Size::new(600.0, 560.0);
/// How long the finished page stays up when Pravera is opening on its own:
/// long enough to read that it worked, short enough not to be in the way.
const LINGER: Duration = Duration::from_millis(1600);
/// One pass of the pulse along the path while something is running.
const PULSE: Duration = Duration::from_millis(1300);
/// Height of the drawn path, and of the tiles at its ends.
const PATH_HEIGHT: f32 = 44.0;

/// Open the installer window and run it until it is closed.
pub fn run(cli: Cli) -> iced::Result {
    let result = iced::application(move || Setup::new(&cli), Setup::update, Setup::view)
        .title(|_: &Setup| "Pravera Setup".to_string())
        .theme(|_: &Setup| theme::theme())
        .subscription(Setup::subscription)
        .font(t::FONT_BYTES)
        .font(t::font_at(t::EXTRA_WEIGHTS[0]))
        .font(t::font_at(t::EXTRA_WEIGHTS[1]))
        .default_font(t::FONT_UI)
        .window(iced::window::Settings {
            size: SIZE,
            resizable: false,
            visible: shots().is_none(),
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
    Frame(Instant),
    Drag,
    Close,
    HoverIn(usize),
    HoverOut(usize),
    ToggleDesktop,
    ToggleLaunch,
    TogglePurge,
    Install,
    /// Show the removal page.
    AskUninstall,
    Back,
    Uninstall,
    /// The action has started this step.
    Reached(Phase),
    Finished(Result<Report, String>),
    Open,
    /// Pictures of each page from a window nobody sees; see [`shots`].
    ShotNext(usize),
    ShotTake(usize),
    ShotTaken(usize, iced::window::Screenshot),
}

/// Debug builds with `PRAVERA_SHOT=<dir>`: stage every page in turn, with a
/// made-up run where one is needed, and write each to `<dir>/setup-<n>.png`.
fn shots() -> Option<std::path::PathBuf> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var_os("PRAVERA_SHOT").map(std::path::PathBuf::from)
}

const SHOT_PAGES: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Stage {
    Choose,
    ConfirmUninstall,
    Working,
    Done(Result<Report, String>),
}

/// What an install or uninstall went through, measured as it happened.
#[derive(Debug, Clone)]
struct Run {
    removing: bool,
    plan: Vec<Phase>,
    /// Each step reached, with when it started.
    reached: Vec<(Phase, Instant)>,
    started: Instant,
    finished: Option<Instant>,
}

impl Run {
    fn current(&self) -> Option<Phase> {
        self.reached.last().map(|(phase, _)| *phase)
    }

    /// How long a reached step took, or has taken so far.
    fn took(&self, index: usize, now: Instant) -> Duration {
        let start = self.reached[index].1;
        let end = self
            .reached
            .get(index + 1)
            .map(|(_, at)| *at)
            .or(self.finished)
            .unwrap_or(now);
        end.saturating_duration_since(start)
    }

    fn elapsed(&self, now: Instant) -> Duration {
        self.finished.unwrap_or(now).saturating_duration_since(self.started)
    }
}

struct Setup {
    window: Option<iced::window::Id>,
    layout: Option<Layout>,
    installed: Option<Installed>,
    stage: Stage,
    run: Option<Run>,
    desktop: bool,
    launch: bool,
    purge: bool,
    now: Instant,
    /// The page below the path arriving.
    page: Tween,
    /// How far along the path is drawn, 0 to 1.
    fill: Tween,
    switches: [Tween; 2],
    hover: HoverTracker,
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
        let desktop = installed.as_ref().map_or(cli.desktop, |found| found.desktop || cli.desktop);
        let launch = !cli.no_launch;
        let now = Instant::now();
        let mut page = Tween::at(0.0);
        page.enter(now, motion::ENTRANCE);
        let setup = Setup {
            window: None,
            desktop,
            launch,
            purge: cli.purge,
            layout,
            installed,
            stage,
            run: None,
            now,
            page,
            fill: Tween::at(0.0),
            switches: [Tween::at(on(desktop)), Tween::at(on(launch))],
            hover: HoverTracker::new(2),
        };
        let pictures = if shots().is_some() {
            iced::window::latest()
                .and_then(crate::shot::park)
                .chain(Task::done(Message::ShotNext(0)))
        } else {
            Task::none()
        };
        (
            setup,
            Task::batch([iced::window::latest().map(Message::WindowReady), pictures]),
        )
    }

    /// Put the window in the state a picture shows.
    fn stage_for_shot(&mut self, page: usize) {
        let now = Instant::now();
        let ago = |millis: u64| now - Duration::from_millis(millis);
        let run = |removing: bool, reached: Vec<(Phase, Instant)>, finished: Option<Instant>| Run {
            removing,
            plan: Phase::plan(removing, Options { desktop: false, launch: false, purge: false }),
            reached,
            started: ago(3200),
            finished,
        };
        self.launch = false;
        // The switches are read off their tweens, which nothing has moved: put
        // each where the page it belongs to would have put it.
        self.switches[0].snap(on(self.desktop));
        self.switches[1].snap(on(self.launch));
        match page {
            0 => {
                self.run = None;
                self.fill.snap(0.0);
                self.turn_to(Stage::Choose);
            }
            1 => {
                self.run = None;
                self.switches[0].snap(on(self.purge));
                self.turn_to(Stage::ConfirmUninstall);
            }
            2 => {
                self.run = Some(run(false, vec![(Phase::Closing, ago(3200)), (Phase::Copying, ago(900))], None));
                self.fill.snap(1.0 / 3.0);
                self.turn_to(Stage::Working);
            }
            3 => {
                self.run = Some(run(
                    false,
                    vec![(Phase::Closing, ago(3200)), (Phase::Copying, ago(2900)), (Phase::Integrating, ago(600))],
                    Some(now),
                ));
                self.fill.snap(1.0);
                self.turn_to(Stage::Done(Ok(Report::default())));
            }
            4 => {
                self.run = Some(run(false, vec![(Phase::Closing, ago(3200)), (Phase::Copying, ago(2900))], Some(now)));
                self.fill.snap(1.0 / 3.0);
                self.turn_to(Stage::Done(Err(
                    "Could not replace pravera.exe: the process cannot access the file because it is being used by another process. (os error 32)".into(),
                )));
            }
            _ => {
                self.run = Some(run(
                    true,
                    vec![(Phase::Closing, ago(3200)), (Phase::Unregistering, ago(2500)), (Phase::Deleting, ago(700))],
                    Some(now),
                ));
                self.fill.snap(1.0);
                self.turn_to(Stage::Done(Ok(Report::default())));
            }
        }
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

    fn installed_version(&self) -> String {
        self.installed
            .as_ref()
            .and_then(|found| found.version.as_ref())
            .map(ToString::to_string)
            .unwrap_or_else(|| "an older version".into())
    }

    fn options(&self) -> Options {
        Options {
            desktop: self.desktop,
            launch: self.launch,
            purge: self.purge,
        }
    }

    fn animating(&self) -> bool {
        let now = self.now;
        matches!(self.stage, Stage::Working)
            || self.page.is_animating(now)
            || self.fill.is_animating(now)
            || self.hover.is_animating(now)
            || self.switches.iter().any(|switch| switch.is_animating(now))
    }

    fn subscription(&self) -> Subscription<Message> {
        // A window never shown gets no frames; the pictures still need time.
        if shots().is_some() {
            return iced::time::every(Duration::from_millis(16)).map(Message::Frame);
        }
        if self.animating() {
            iced::window::frames().map(Message::Frame)
        } else {
            Subscription::none()
        }
    }

    /// A new page arrives: the part below the path rises into place.
    fn turn_to(&mut self, stage: Stage) {
        self.stage = stage;
        self.now = Instant::now();
        self.page.snap(0.0);
        self.page.enter(self.now, motion::ENTRANCE);
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::WindowReady(id) => {
                self.window = id;
                id.map(crate::chrome::dress).unwrap_or_else(Task::none)
            }
            Message::Frame(now) => {
                self.now = now;
                Task::none()
            }
            Message::Drag => self.window.map(iced::window::drag).unwrap_or_else(Task::none),
            Message::Close => iced::exit(),
            Message::HoverIn(index) => {
                self.hover.enter(index, Instant::now());
                Task::none()
            }
            Message::HoverOut(index) => {
                self.hover.exit(index, Instant::now());
                Task::none()
            }
            Message::ToggleDesktop => {
                self.desktop = !self.desktop;
                self.flip(0, self.desktop);
                Task::none()
            }
            Message::ToggleLaunch => {
                self.launch = !self.launch;
                self.flip(1, self.launch);
                Task::none()
            }
            Message::TogglePurge => {
                self.purge = !self.purge;
                self.flip(0, self.purge);
                Task::none()
            }
            Message::AskUninstall => {
                self.switches[0].snap(on(self.purge));
                self.turn_to(Stage::ConfirmUninstall);
                Task::none()
            }
            Message::Back => {
                self.switches[0].snap(on(self.desktop));
                self.turn_to(Stage::Choose);
                Task::none()
            }
            Message::Install => self.start(false),
            Message::Uninstall => self.start(true),
            Message::Reached(phase) => {
                let now = Instant::now();
                self.now = now;
                if let Some(run) = &mut self.run {
                    run.reached.push((phase, now));
                    let done = run.reached.len().saturating_sub(1) as f32;
                    let total = run.plan.len().max(1) as f32;
                    self.fill.go(done / total, now, motion::ENTRANCE, motion::EASE_CHANGE);
                }
                Task::none()
            }
            Message::Finished(outcome) => {
                let now = Instant::now();
                let mut removing = false;
                if let Some(run) = &mut self.run {
                    run.finished = Some(now);
                    removing = run.removing;
                }
                if outcome.is_ok() {
                    self.fill.go(1.0, now, motion::ENTRANCE, motion::EASE_CHANGE);
                }
                if let Some(layout) = &self.layout {
                    self.installed = layout.installed();
                }
                // Installed and opening: say so, then get out of the way.
                let leave = !removing && self.launch && outcome.as_ref().is_ok_and(|report| report.warnings.is_empty());
                self.turn_to(Stage::Done(outcome));
                if leave {
                    return Task::perform(tokio::time::sleep(LINGER), |_| Message::Close);
                }
                Task::none()
            }
            Message::Open => {
                if let Some(layout) = &self.layout {
                    let _ = install::open(layout);
                }
                iced::exit()
            }
            Message::ShotNext(page) => {
                if page >= SHOT_PAGES {
                    return iced::exit();
                }
                self.stage_for_shot(page);
                Task::perform(tokio::time::sleep(Duration::from_millis(900)), move |_| Message::ShotTake(page))
            }
            Message::ShotTake(page) => iced::window::latest()
                .and_then(iced::window::screenshot)
                .map(move |picture| Message::ShotTaken(page, picture)),
            Message::ShotTaken(page, picture) => {
                if let Some(dir) = shots() {
                    let _ = std::fs::create_dir_all(&dir);
                    let path = dir.join(format!("setup-{page}.png"));
                    if let Some(image) = image::RgbaImage::from_raw(picture.size.width, picture.size.height, picture.rgba.to_vec()) {
                        match image.save(&path) {
                            Ok(()) => println!("shot {}", path.display()),
                            Err(error) => eprintln!("shot {} failed: {error}", path.display()),
                        }
                    }
                }
                Task::done(Message::ShotNext(page + 1))
            }
        }
    }

    fn flip(&mut self, index: usize, value: bool) {
        self.switches[index].go(on(value), Instant::now(), motion::STANDARD, motion::EASE_CHANGE);
    }

    fn start(&mut self, removing: bool) -> Task<Message> {
        let options = self.options();
        let now = Instant::now();
        self.run = Some(Run {
            removing,
            plan: Phase::plan(removing, options),
            reached: Vec::new(),
            started: now,
            finished: None,
        });
        self.fill.snap(0.0);
        let Some(layout) = self.layout.clone() else {
            if let Some(run) = &mut self.run {
                run.finished = Some(now);
            }
            self.turn_to(Stage::Done(Err("There is no per-user place to install to on this system.".into())));
            return Task::none();
        };
        self.turn_to(Stage::Working);

        Task::run(
            iced::stream::channel(8, async move |mut sender: iced::futures::channel::mpsc::Sender<Message>| {
                let mut steps = sender.clone();
                let outcome = tokio::task::spawn_blocking(move || {
                    let mut report = |phase| {
                        let _ = steps.try_send(Message::Reached(phase));
                    };
                    if removing {
                        install::uninstall_with(&layout, options, &mut report)
                    } else {
                        install::install_with(&layout, options, &mut report)
                    }
                })
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
                let _ = iced::futures::SinkExt::send(&mut sender, Message::Finished(outcome)).await;
            }),
            |message| message,
        )
    }

    // ------------------------------------------------------------------ view

    fn view(&self) -> Element<'_, Message> {
        let page: Element<'_, Message> = match &self.stage {
            Stage::Choose => self.choose(),
            Stage::ConfirmUninstall => self.confirm_uninstall(),
            Stage::Working => self.working(),
            Stage::Done(outcome) => self.done(outcome),
        };

        let body = column![
            self.path(),
            Space::new().height(t::SPACE_6),
            motion::rise(page, self.page.value(self.now)),
        ]
        .height(Length::Fill);

        let frame = column![
            titlebar(),
            container(body)
                .padding(iced::Padding {
                    top: t::SPACE_2,
                    right: t::SPACE_8,
                    bottom: t::SPACE_6,
                    left: t::SPACE_8,
                })
                .width(Length::Fill)
                .height(Length::Fill),
        ];

        container(frame)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| iced::widget::container::Style {
                background: Some(Background::Color(t::BACKGROUND)),
                text_color: Some(t::FOREGROUND),
                ..Default::default()
            })
            .into()
    }

    /// The path at the top of every page: where Pravera comes from, where it
    /// goes, and how far along it the work is.
    fn path(&self) -> Element<'_, Message> {
        let removing = match &self.stage {
            Stage::ConfirmUninstall => true,
            Stage::Working | Stage::Done(_) => self.run.as_ref().is_some_and(|run| run.removing),
            Stage::Choose => false,
        };
        let tone = match &self.stage {
            Stage::Choose | Stage::ConfirmUninstall => Look::Idle,
            Stage::Working => Look::Live,
            Stage::Done(Ok(_)) => Look::Done,
            Stage::Done(Err(_)) => Look::Failed,
        };
        let ink = if removing { t::NEUTRAL_300 } else { t::ROUTE_DIRECT };
        let pulse = matches!(tone, Look::Live).then(|| {
            let started = self.run.as_ref().map_or(self.now, |run| run.started);
            let cycle = self.now.saturating_duration_since(started).as_secs_f32() / PULSE.as_secs_f32();
            cycle.fract()
        });

        let finished = matches!(&self.stage, Stage::Done(Ok(_)));
        let failed = matches!(&self.stage, Stage::Done(Err(_)));
        let (from_glyph, from_label, to_glyph, to_label) = if removing {
            (icon::DEVICES, machine_name(), icon::TRASH, "REMOVED".to_string())
        } else {
            (icon::LOGO, format!("SETUP {VERSION}"), icon::DEVICES, machine_name())
        };
        let to_tint = match (finished, failed) {
            (true, _) => ink,
            (_, true) => t::DESTRUCTIVE_TEXT,
            _ => t::MUTED_FOREGROUND,
        };

        let diagram = iced::widget::Canvas::new(Diagram {
            fill: self.fill.value(self.now),
            pulse,
            look: tone,
            ink,
        })
        .width(Length::Fill)
        .height(Length::Fixed(PATH_HEIGHT));

        let ends = row![
            components::glyph_tile(from_glyph, t::FOREGROUND, PATH_HEIGHT),
            diagram,
            components::glyph_tile(to_glyph, to_tint, PATH_HEIGHT),
        ]
        .spacing(t::SPACE_2)
        .align_y(Alignment::Center);

        let captions = row![
            mono_caption(from_label),
            Space::new().width(Length::Fill),
            mono_caption(to_label),
        ]
        .align_y(Alignment::Center);

        container(column![ends, captions].spacing(t::SPACE_2))
            .padding([t::SPACE_4, t::SPACE_5])
            .width(Length::Fill)
            .style(theme::card)
            .into()
    }

    fn place(&self) -> String {
        self.layout
            .as_ref()
            .map(|layout| layout.bundle.as_ref().unwrap_or(&layout.dir).display().to_string())
            .unwrap_or_else(|| "Nowhere: this system has no per-user program folder".into())
    }

    fn choose(&self) -> Element<'_, Message> {
        let (eyebrow, headline, detail, glyph, action) = match self.relation() {
            Relation::Fresh => (
                "New install".to_string(),
                format!("Install Pravera {VERSION}"),
                "Remote desktop over the fastest path between your machines.".to_string(),
                icon::DOWNLOAD,
                "Install",
            ),
            Relation::Older => (
                format!("Update from {}", self.installed_version()),
                format!("Update to Pravera {VERSION}"),
                "Your devices, accounts and settings stay exactly as they are.".to_string(),
                icon::REFRESH,
                "Update",
            ),
            Relation::Same => (
                "Already installed".to_string(),
                format!("Repair Pravera {VERSION}"),
                "Puts back anything that went missing. Your settings stay.".to_string(),
                icon::REFRESH,
                "Reinstall",
            ),
            Relation::Newer => (
                format!("{} is installed", self.installed_version()),
                format!("Go back to Pravera {VERSION}"),
                "The installed copy is newer than this installer.".to_string(),
                icon::ALERT,
                "Install anyway",
            ),
        };

        let facts = ledger(vec![
            ("Location", self.place(), true),
            ("Needs", "No administrator, no restart".into(), false),
            ("Updates", "Automatic, and never during a session".into(), false),
        ]);

        let switches = column![
            self.switch_row(0, "Desktop shortcut", "The Start menu entry is added either way.", Message::ToggleDesktop),
            self.switch_row(1, "Open Pravera when done", "It keeps itself up to date from then on.", Message::ToggleLaunch),
        ]
        .spacing(t::SPACE_1);

        let mut footer = row![].spacing(t::SPACE_2).align_y(Alignment::Center);
        if self.installed.is_some() {
            footer = footer.push(components::small_button(Some(icon::TRASH), "Uninstall", Some(Message::AskUninstall)));
        }
        footer = footer
            .push(Space::new().width(Length::Fill))
            .push(big_button(glyph, action, Look::Idle, theme::primary_button, Some(Message::Install)));

        column![
            heading(eyebrow, headline, detail),
            Space::new().height(t::SPACE_5),
            facts,
            Space::new().height(t::SPACE_3),
            switches,
            Space::new().height(Length::Fill),
            // What the switches keep from the buttons under them, however
            // short the window is.
            Space::new().height(t::SPACE_5),
            footer,
        ]
        .into()
    }

    fn confirm_uninstall(&self) -> Element<'_, Message> {
        let keeps = if self.purge {
            "Nothing. Identity, accounts and settings go too"
        } else {
            "Identity, accounts, known devices and settings"
        };
        let facts = ledger(vec![
            ("Location", self.place(), true),
            ("Removes", "The program, shortcuts, sign-in entry and apps listing".into(), false),
            ("Keeps", keeps.into(), false),
        ]);

        column![
            heading(
                "Uninstall".into(),
                format!("Remove Pravera {}", self.installed_version()),
                "Anything connected to this machine is disconnected.".into(),
            ),
            Space::new().height(t::SPACE_5),
            facts,
            Space::new().height(t::SPACE_3),
            self.switch_row(
                0,
                "Also delete my data",
                "Without it, reinstalling later picks up where you left off.",
                Message::TogglePurge,
            ),
            Space::new().height(Length::Fill),
            Space::new().height(t::SPACE_5),
            row![
                components::small_button(None, "Back", Some(Message::Back)),
                Space::new().width(Length::Fill),
                big_button(icon::TRASH, "Uninstall", Look::Failed, theme::destructive_button, Some(Message::Uninstall)),
            ]
            .align_y(Alignment::Center),
        ]
        .into()
    }

    fn working(&self) -> Element<'_, Message> {
        let Some(run) = &self.run else {
            return Space::new().into();
        };
        let step = run.reached.len().max(1);
        let total = run.plan.len();
        let current = run.current().unwrap_or(run.plan[0]);
        let verb = if run.removing { "Uninstalling" } else { "Installing" };

        column![
            heading(
                format!("{verb}  ·  step {step} of {total}"),
                present(current).to_string(),
                active_detail(current, self),
            ),
            Space::new().height(t::SPACE_5),
            self.steps(run, None),
        ]
        .into()
    }

    fn done<'a>(&'a self, outcome: &'a Result<Report, String>) -> Element<'a, Message> {
        let removing = self.run.as_ref().is_some_and(|run| run.removing);
        let took = self.run.as_ref().map_or(Duration::ZERO, |run| run.elapsed(self.now));
        let opening = !removing && self.launch && outcome.as_ref().is_ok_and(|report| report.warnings.is_empty());

        let (eyebrow, headline, detail) = match (outcome, removing) {
            (Ok(_), false) => (
                format!("Done in {}", seconds(took)),
                format!("Pravera {VERSION} is installed"),
                if opening {
                    "Opening it now. This window closes by itself.".to_string()
                } else {
                    "Find it in the Start menu, or open it from here.".to_string()
                },
            ),
            (Ok(_), true) => (
                format!("Done in {}", seconds(took)),
                "Pravera is removed".to_string(),
                "Thanks for trying it. Run this Setup again any time to come back.".to_string(),
            ),
            (Err(_), false) => ("Stopped".into(), "Pravera was not installed".into(), "Nothing half-done was left running.".into()),
            (Err(_), true) => ("Stopped".into(), "Pravera was not removed".into(), "What is below says what is left.".into()),
        };

        let mut notes = column![].spacing(t::SPACE_2);
        match outcome {
            Ok(report) => {
                for warning in &report.warnings {
                    notes = notes.push(components::callout(icon::ALERT, warning.as_str(), Tone::Warning));
                }
            }
            Err(error) => notes = notes.push(components::callout(icon::ALERT, error.as_str(), Tone::Danger)),
        }

        let mut footer = row![Space::new().width(Length::Fill)].spacing(t::SPACE_2).align_y(Alignment::Center);
        if opening {
            // Nothing to press: it is on its way.
        } else if !removing && outcome.is_ok() && self.installed.is_some() {
            footer = footer
                .push(components::small_button(None, "Close", Some(Message::Close)))
                .push(big_button(icon::CHEVRON_RIGHT, "Open Pravera", Look::Idle, theme::primary_button, Some(Message::Open)));
        } else if outcome.is_err() {
            let again = if removing { Message::Uninstall } else { Message::Install };
            footer = footer
                .push(components::small_button(None, "Close", Some(Message::Close)))
                .push(big_button(icon::REFRESH, "Try again", Look::Idle, theme::primary_button, Some(again)));
        } else {
            footer = footer.push(big_button(icon::CHECK, "Close", Look::Idle, theme::primary_button, Some(Message::Close)));
        }

        let steps = self.run.as_ref().map_or_else(|| Space::new().into(), |run| self.steps(run, Some(outcome.is_ok())));

        column![
            heading(eyebrow, headline, detail),
            Space::new().height(t::SPACE_5),
            components::scroll(column![steps, notes].spacing(t::SPACE_3)),
            Space::new().height(t::SPACE_4),
            footer,
        ]
        .height(Length::Fill)
        .into()
    }

    /// Every step of the plan: done ones checked with how long they took, the
    /// running one turning, the rest waiting. `finished` is whether the whole
    /// action succeeded, once it has ended.
    fn steps<'a>(&'a self, run: &'a Run, finished: Option<bool>) -> Element<'a, Message> {
        let mut list = column![].spacing(0);
        for (index, phase) in run.plan.iter().enumerate() {
            let reached = run.reached.iter().position(|(seen, _)| seen == phase);
            let last = reached.is_some_and(|at| at + 1 == run.reached.len());
            let state = match (reached, finished) {
                (None, Some(_)) => Mark::Skipped,
                (None, None) => Mark::Waiting,
                (Some(_), Some(false)) if last => Mark::Failed,
                (Some(_), None) if last => Mark::Running,
                (Some(_), _) => Mark::Done,
            };
            let took = reached
                .filter(|_| matches!(state, Mark::Done | Mark::Running | Mark::Failed))
                .map(|at| seconds(run.took(at, self.now)))
                .unwrap_or_default();

            let (ink, weight) = match state {
                Mark::Running => (t::FOREGROUND, t::FONT_UI_STRONG),
                Mark::Done => (t::NEUTRAL_300, t::FONT_UI),
                Mark::Failed => (t::DESTRUCTIVE_TEXT, t::FONT_UI_STRONG),
                Mark::Waiting | Mark::Skipped => (t::SUBTLE_FOREGROUND, t::FONT_UI),
            };

            let line = row![
                container(self.mark(state, run.removing))
                    .center_x(Length::Fixed(t::ICON))
                    .center_y(Length::Fixed(t::ICON)),
                text(plain(*phase)).size(t::TEXT_SM).font(weight).style(theme::tinted(ink)),
                Space::new().width(Length::Fill),
                text(took).size(t::TEXT_XS).style(theme::subtle),
            ]
            .spacing(t::SPACE_3)
            .align_y(Alignment::Center)
            .height(Length::Fixed(30.0));

            if index > 0 {
                list = list.push(components::hairline());
            }
            list = list.push(container(line).padding([0.0, t::SPACE_3]));
        }
        container(list)
            .width(Length::Fill)
            .style(theme::card)
            .into()
    }

    fn mark(&self, state: Mark, removing: bool) -> Element<'_, Message> {
        let ink = if removing { t::NEUTRAL_300 } else { t::ROUTE_DIRECT };
        match state {
            Mark::Done => icon::stroked(icon::CHECK, t::ICON_SM, ink),
            Mark::Failed => icon::stroked(icon::ALERT, t::ICON_SM, t::DESTRUCTIVE_TEXT),
            Mark::Running => {
                let turns = self
                    .run
                    .as_ref()
                    .map_or(0.0, |run| self.now.saturating_duration_since(run.started).as_secs_f32() * 1.1);
                icon::turned(SPINNER, t::ICON_SM, t::FOREGROUND, turns)
            }
            Mark::Waiting => components::dot(t::NEUTRAL_600, 6.0),
            Mark::Skipped => components::dot(t::NEUTRAL_700, 6.0),
        }
    }

    fn switch_row<'a>(&self, index: usize, title: &'a str, detail: &'a str, on_press: Message) -> Element<'a, Message> {
        components::switch_row(
            title,
            detail,
            self.switches[index].value(self.now),
            self.hover.amount(index, self.now),
            on_press,
            Message::HoverIn(index),
            Message::HoverOut(index),
        )
    }
}

/// How a step in the list stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    Waiting,
    Running,
    Done,
    Failed,
    /// The action ended before this step came up.
    Skipped,
}

/// A quarter-and-a-bit of a ring: turned, it reads as work in progress on the
/// same 16-grid and stroke as every other mark.
const SPINNER: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" fill="none" stroke="#ffffff" stroke-width="1.75" stroke-linecap="round"><path d="M8 2a6 6 0 0 1 6 6"/></svg>"##;

/// A step as a line in the list.
fn plain(phase: Phase) -> String {
    match phase {
        Phase::Closing => "Close any running Pravera".into(),
        Phase::Copying => format!("Copy Pravera {VERSION}"),
        Phase::Integrating => INTEGRATING.into(),
        Phase::Opening => "Open Pravera".into(),
        Phase::Unregistering => "Remove the sign-in entry and service".into(),
        Phase::Deleting => "Delete the program and its shortcuts".into(),
        Phase::Purging => "Delete this machine's Pravera data".into(),
    }
}

#[cfg(windows)]
const INTEGRATING: &str = "Add it to Start and the apps list";
#[cfg(target_os = "macos")]
const INTEGRATING: &str = "Sign it for this Mac";
#[cfg(not(any(windows, target_os = "macos")))]
const INTEGRATING: &str = "Add it to the applications menu";

/// A step as the headline while it runs.
fn present(phase: Phase) -> &'static str {
    match phase {
        Phase::Closing => "Closing Pravera",
        Phase::Copying => "Copying Pravera",
        Phase::Integrating => "Adding shortcuts",
        Phase::Opening => "Opening Pravera",
        Phase::Unregistering => "Removing sign-in entries",
        Phase::Deleting => "Deleting Pravera",
        Phase::Purging => "Deleting your data",
    }
}

fn active_detail(phase: Phase, setup: &Setup) -> String {
    match phase {
        Phase::Closing => "Asking a running copy to quit first. It gets up to 15 seconds.".into(),
        Phase::Copying => format!("Into {}", setup.place()),
        Phase::Integrating => "So you can find it again.".into(),
        Phase::Opening => "Starting the copy that was just installed.".into(),
        Phase::Unregistering => "Only the entries that start this copy.".into(),
        Phase::Deleting => format!("From {}", setup.place()),
        Phase::Purging => "Identity, accounts, known devices and settings.".into(),
    }
}

/// The page's name in small capitals, its headline, and one line under it.
fn heading<'a>(eyebrow: String, headline: String, detail: String) -> Element<'a, Message> {
    column![
        text(t::tracked(&eyebrow.to_uppercase()))
            .size(t::TEXT_2XS)
            .font(t::FONT_UI_MEDIUM)
            .style(theme::subtle),
        Space::new().height(t::SPACE_1_5),
        text(headline).size(t::TEXT_XL).font(t::FONT_UI_STRONG).style(theme::heading),
        Space::new().height(t::SPACE_1),
        text(detail).size(t::TEXT_XS).style(theme::muted),
    ]
    .width(Length::Fill)
    .into()
}

/// Facts in a hairline-ruled ledger: the name in a fixed column, the value
/// after it. `mono` values are machine strings, set in the machine's voice.
fn ledger<'a>(facts: Vec<(&'a str, String, bool)>) -> Element<'a, Message> {
    let mut list = column![].width(Length::Fill);
    for (index, (label, value, mono)) in facts.into_iter().enumerate() {
        if index > 0 {
            list = list.push(components::hairline());
        }
        let value = text(value)
            .size(t::TEXT_XS)
            .wrapping(text::Wrapping::None)
            .style(if mono { theme::heading } else { theme::muted });
        list = list.push(
            row![
                container(text(label).size(t::TEXT_XS).style(theme::subtle)).width(Length::Fixed(88.0)),
                container(value).width(Length::Fill).clip(true),
            ]
            .spacing(t::SPACE_3)
            .align_y(Alignment::Center)
            .height(Length::Fixed(30.0)),
        );
    }
    container(list).padding([0.0, t::SPACE_1]).into()
}

/// What this machine is called, for the end of the path that is it: the name
/// the system gives it, in capitals like every caption.
fn machine_name() -> String {
    let name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|line| line.trim().to_string())
        })
        .map(|name| name.trim().to_uppercase())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "THIS PC".into());
    tail(&name, 34)
}

fn mono_caption<'a>(words: String) -> Element<'a, Message> {
    text(t::tracked(&words))
        .size(t::TEXT_2XS)
        .wrapping(text::Wrapping::None)
        .style(theme::subtle)
        .into()
}

/// A title bar that drags the window and closes it; nothing else, because a
/// fixed-size installer has nothing to minimise into or maximise to.
fn titlebar<'a>() -> Element<'a, Message> {
    let close = components::glide(|hover| {
        button(
            container(icon::stroked(icon::WIN_CLOSE, 10.0, t::MUTED_FOREGROUND))
                .center_x(Length::Fixed(46.0))
                .center_y(Length::Fixed(t::TITLEBAR_HEIGHT)),
        )
        .padding(0)
        .style(theme::gliding(hover, close_button))
        .on_press(Message::Close)
        .into()
    });

    let bar = row![
        Space::new().width(t::SPACE_4),
        icon::stroked(icon::LOGO, t::ICON_SM, t::FOREGROUND),
        Space::new().width(t::SPACE_2),
        text("Pravera Setup").size(t::TEXT_XS).font(t::FONT_UI_MEDIUM).style(theme::heading),
        Space::new().width(t::SPACE_2),
        components::pill(format!("v{VERSION}"), Tone::Outline),
        Space::new().width(Length::Fill),
        close,
    ]
    .align_y(Alignment::Center)
    .height(Length::Fixed(t::TITLEBAR_HEIGHT));

    mouse_area(container(bar).width(Length::Fill)).on_press(Message::Drag).into()
}

/// The window's close: the platform's red under the pointer, as every other
/// window on the desktop does it.
fn close_button(theme: &Theme, status: button::Status) -> button::Style {
    match status {
        button::Status::Hovered | button::Status::Pressed => button::Style {
            background: Some(Background::Color(if matches!(status, button::Status::Pressed) {
                t::DESTRUCTIVE_HOVER
            } else {
                t::DESTRUCTIVE
            })),
            text_color: t::DESTRUCTIVE_FOREGROUND,
            border: Border::default(),
            ..button::Style::default()
        },
        _ => theme::ghost_button(theme, status),
    }
}

/// The page's one action, a size up from the buttons inside the app because
/// here it is the whole point of the window.
fn big_button<'a>(
    glyph: &'static str,
    words: &'a str,
    look: Look,
    style: fn(&Theme, button::Status) -> button::Style,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    let ink = match look {
        Look::Failed => t::DESTRUCTIVE_FOREGROUND,
        _ => t::PRIMARY_FOREGROUND,
    };
    components::glide(|hover| {
        button(
            row![
                icon::stroked(glyph, t::ICON_SM, ink),
                text(words).size(t::TEXT_SM).font(t::FONT_UI_STRONG),
            ]
            .spacing(t::SPACE_2)
            .align_y(Alignment::Center),
        )
        .padding([t::SPACE_2 + 1.0, t::SPACE_5])
        .style(theme::gliding(hover, style))
        .on_press_maybe(on_press)
        .into()
    })
}

fn on(value: bool) -> f32 {
    if value {
        1.0
    } else {
        0.0
    }
}

/// A duration as the machine would report it: tenths of a second.
fn seconds(duration: Duration) -> String {
    format!("{:.1} s", duration.as_secs_f32())
}

/// The last `max` characters of `words`, with an ellipsis where the rest was.
fn tail(words: &str, max: usize) -> String {
    let count = words.chars().count();
    if count <= max {
        return words.to_string();
    }
    let kept: String = words.chars().skip(count - (max - 1)).collect();
    format!("…{kept}")
}

// ------------------------------------------------------------------ the path

/// What the path is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Look {
    /// Nothing started: a dashed line, the path that is about to be.
    Idle,
    /// Work under way: filling, with a pulse travelling it.
    Live,
    Done,
    Failed,
}

/// The line between the two tiles. The same drawing language as the route
/// meter in the device list: round caps, dots for ends, a dashed rule for a
/// path that is not there yet.
struct Diagram {
    fill: f32,
    pulse: Option<f32>,
    look: Look,
    ink: Color,
}

impl<Message> canvas::Program<Message> for Diagram {
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
        let y = height / 2.0;
        let node = 3.0;
        let left = Point::new(node + t::SPACE_1, y);
        let right = Point::new(width - node - t::SPACE_1, y);
        let at = |amount: f32| Point::new(left.x + (right.x - left.x) * amount.clamp(0.0, 1.0), y);
        let fill = if self.look == Look::Done { 1.0 } else { self.fill };
        let ink = if self.look == Look::Failed { t::DESTRUCTIVE_TEXT } else { self.ink };

        let dashed = |frame: &mut canvas::Frame, from: Point, to: Point| {
            frame.stroke(
                &Path::line(from, to),
                Stroke {
                    line_dash: canvas::LineDash {
                        segments: &[3.0, 5.0],
                        offset: 0,
                    },
                    ..stroke(1.5, t::NEUTRAL_600)
                },
            );
        };

        match self.look {
            Look::Idle => {
                dashed(&mut frame, left, right);
                frame.fill(&Path::circle(left, node), t::NEUTRAL_400);
                frame.stroke(&Path::circle(right, node - 0.5), stroke(1.25, t::NEUTRAL_600));
            }
            Look::Live | Look::Done => {
                frame.stroke(&Path::line(left, right), stroke(2.0, t::NEUTRAL_750));
                if fill > 0.001 {
                    frame.stroke(&Path::line(left, at(fill)), stroke(2.0, ink));
                }
                if let Some(pulse) = self.pulse {
                    // Brightest mid-path, gone at the ends, so it arrives
                    // rather than popping in at the tile's edge.
                    let strength = (pulse * std::f32::consts::PI).sin();
                    let spot = at(pulse);
                    frame.fill(&Path::circle(spot, 7.0), t::with_alpha(ink, 0.10 * strength));
                    frame.fill(&Path::circle(spot, 2.5), t::with_alpha(t::FOREGROUND, 0.9 * strength));
                }
                frame.fill(&Path::circle(left, node), ink);
                if fill >= 0.999 {
                    frame.fill(&Path::circle(right, node), ink);
                } else {
                    frame.stroke(&Path::circle(right, node - 0.5), stroke(1.25, t::NEUTRAL_600));
                }
            }
            Look::Failed => {
                let stop = at(fill.max(0.08));
                frame.stroke(&Path::line(left, stop), stroke(2.0, ink));
                dashed(&mut frame, stop, right);
                frame.fill(&Path::circle(left, node), ink);
                frame.fill(&Path::circle(stop, node - 0.5), ink);
                frame.stroke(&Path::circle(right, node - 0.5), stroke(1.25, t::NEUTRAL_600));
            }
        }

        vec![frame.into_geometry()]
    }
}

fn stroke(width: f32, color: Color) -> Stroke<'static> {
    Stroke {
        style: canvas::Style::Solid(color),
        width,
        line_cap: canvas::LineCap::Round,
        line_join: canvas::LineJoin::Round,
        ..Stroke::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_path_keeps_its_end() {
        let trimmed = tail(r"C:\Users\somebody\AppData\Local\Programs\Pravera", 20);
        assert_eq!(trimmed.chars().count(), 20);
        assert!(trimmed.starts_with('…'));
        assert!(trimmed.ends_with(r"Programs\Pravera"));
        assert_eq!(tail("short", 20), "short");
    }

    #[test]
    fn every_plan_starts_by_closing_the_running_copy() {
        for removing in [false, true] {
            for launch in [false, true] {
                for purge in [false, true] {
                    let plan = Phase::plan(removing, Options { desktop: false, launch, purge });
                    assert_eq!(plan[0], Phase::Closing);
                    assert_eq!(plan.contains(&Phase::Opening), !removing && launch);
                    assert_eq!(plan.contains(&Phase::Purging), removing && purge);
                }
            }
        }
    }

    #[test]
    fn a_step_takes_until_the_next_one_starts() {
        let start = Instant::now();
        let run = Run {
            removing: false,
            plan: vec![Phase::Closing, Phase::Copying],
            reached: vec![(Phase::Closing, start), (Phase::Copying, start + Duration::from_millis(1500))],
            started: start,
            finished: Some(start + Duration::from_millis(1800)),
        };
        assert_eq!(run.took(0, start), Duration::from_millis(1500));
        assert_eq!(run.took(1, start), Duration::from_millis(300));
        assert_eq!(seconds(run.elapsed(start)), "1.8 s");
    }
}
